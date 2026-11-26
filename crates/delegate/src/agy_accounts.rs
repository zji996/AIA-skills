//! Local Antigravity CLI (`agy`) multi-account management.
//!
//! Replaces third-party `agy-swap` with a transparent, manual-only, file-locked
//! credential slot manager under `~/.config/delegate/agy-accounts/<id>/`.
//!
//! Design invariants:
//! - No automatic rotation (quota exhaustion never switches accounts automatically).
//! - Real quota is fetched exclusively via the official `agy -p /usage </dev/null` slash command
//!   (no Google internal HTTP APIs); cached in `<state>/agy-quota.json` with fetch timestamp.
//! - Secret tokens (`access_token`, `refresh_token`, `id_token`) are never printed to
//!   stdout, stderr, JSON output, or logs.
//! - Slot directories are `0700`, credential files and `accounts.json` are `0600`,
//!   written atomically via `O_CREAT|O_EXCL` mode `0600` temp file + `sync_data` + `rename`.
//! - `use` can switch accounts even while `agy` processes are running: running processes
//!   and already-started sessions stay pinned to their original account (falling back to
//!   the active account only if the pinned slot is deleted, invalid, or quota-exhausted),
//!   while new runs (and `--fresh` sessions) pick up the newly active account.
//! - Because `agy` writes refreshed OAuth tokens back to `~/.gemini/antigravity-cli/antigravity-oauth-token`,
//!   each delegate-launched `agy` process runs with a private credential overlay `HOME`
//!   (`0700` temp HOME with slot credentials copied `0600` and all other `$HOME` / `~/.gemini` /
//!   `~/.config` entries symlinked to the real directories). On exit, any refreshed token is
//!   validated and saved back under `.lock` to that process's own slot without touching another
//!   account's live `~/.gemini`.

use crate::common::*;
use crate::{has, parse_simple, value};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub const SCHEMA_VERSION: u64 = 1;
pub const QUOTA_CACHE_SCHEMA_VERSION: u64 = 1;
const QUOTA_HISTORY_LIMIT: usize = 200;
const QUOTA_HISTORY_RETENTION_SECS: i64 = 7 * 86400;
const DEFAULT_QUOTA_TIMEOUT_SECS: u64 = 30;
static TEMP_HOME_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy)]
pub struct CredFile {
    pub slot_name: &'static str,
    pub home_rel: &'static str,
    pub is_core_token: bool,
    pub private_parent: bool,
}

/// Exact set of credential files written and read by Antigravity CLI (`agy`).
pub const CRED_FILES: &[CredFile] = &[
    CredFile {
        slot_name: "oauth_creds.json",
        home_rel: ".gemini/oauth_creds.json",
        is_core_token: true,
        private_parent: false,
    },
    CredFile {
        slot_name: "google_accounts.json",
        home_rel: ".gemini/google_accounts.json",
        is_core_token: false,
        private_parent: false,
    },
    CredFile {
        slot_name: "jetski-standalone-oauth-token",
        home_rel: ".gemini/jetski-standalone-oauth-token",
        is_core_token: true,
        private_parent: false,
    },
    CredFile {
        slot_name: "antigravity-cli-oauth-token",
        home_rel: ".gemini/antigravity-cli/antigravity-oauth-token",
        is_core_token: true,
        private_parent: false,
    },
    CredFile {
        slot_name: "antigravity-oauth-token",
        home_rel: ".gemini/antigravity/antigravity-oauth-token",
        is_core_token: true,
        private_parent: false,
    },
    CredFile {
        slot_name: "credential-user-scope",
        home_rel: ".config/antigravity/credential-user-scope",
        is_core_token: false,
        private_parent: true,
    },
];

/// Non-secret headless-Linux keyring markers seeded into temp HOME during `add`
/// so `agy` writes OAuth files to disk instead of stalling on a missing desktop keyring.
const KEYRING_MARKERS: &[&str] = &[
    ".gemini/cache/antigravity-keyring-unavailable",
    ".gemini/antigravity/cache/antigravity-keyring-unavailable",
    ".gemini/antigravity-cli/cache/antigravity-keyring-unavailable",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningAgy {
    pub pid: u32,
    pub delegate_run: Option<String>,
    pub run_name: Option<String>,
    pub role: Option<String>, // "author" | "reviewer"
    pub round: Option<String>,
    pub account: Option<String>,
    pub cmdline: String,
}

pub fn config_dir() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("delegate")
}

pub fn accounts_root() -> PathBuf {
    config_dir().join("agy-accounts")
}

fn accounts_meta_path(root: &Path) -> PathBuf {
    root.join("accounts.json")
}

fn accounts_lock_path(root: &Path) -> PathBuf {
    root.join(".lock")
}

fn slot_dir(root: &Path, id: &str) -> PathBuf {
    root.join(id)
}

fn ensure_dir_mode(path: &Path, mode: u32) -> Res<()> {
    if !path.exists() {
        fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {:o} {}: {e}", mode, path.display()))
}

fn ensure_parent_dir(path: &Path, private_parent: bool) -> Res<()> {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    let existed = parent.exists();
    if !existed {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    if private_parent && !existed {
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Atomic secret file write with `0600` permissions from creation (no world-readable window).
fn write_secret_0600(path: &Path, data: &[u8], private_parent: bool) -> Res<()> {
    ensure_parent_dir(path, private_parent)?;
    let tmp = atomic_tmp_path(path);
    let write_res = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_data()?;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(format!("{}: {err}", path.display()));
    }
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("{}: {err}", path.display()));
    }
    Ok(())
}

fn write_executable_0755(path: &Path, data: &[u8]) -> Res<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let tmp = atomic_tmp_path(path);
    let write_res = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o755)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_data()?;
        f.set_permissions(fs::Permissions::from_mode(0o755))?;
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(format!("{}: {err}", path.display()));
    }
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("{}: {err}", path.display()));
    }
    Ok(())
}

fn acquire_accounts_lock(root: &Path) -> Res<File> {
    ensure_dir_mode(root, 0o700)?;
    let lock_path = accounts_lock_path(root);
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| format!("open lock {}: {e}", lock_path.display()))?;
    let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
    f.lock()
        .map_err(|e| format!("lock {}: {e}", lock_path.display()))?;
    Ok(f)
}

pub fn validate_id(id: &str) -> Res<()> {
    if id.is_empty() || id.len() > 64 {
        return Err("account id must be 1..=64 characters".into());
    }
    if id.starts_with('.') || id.contains("..") {
        return Err(format!("invalid account id {id:?}: cannot start with '.' or contain '..'"));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(format!(
            "invalid account id {id:?}: use only ASCII letters, digits, '-', '_', '.'"
        ));
    }
    Ok(())
}

pub fn derive_id(email: &str, existing: &Map<String, Value>) -> String {
    let raw = email.split('@').next().unwrap_or(email).trim();
    let mut base = String::new();
    let mut dash = false;
    for ch in raw.chars() {
        let low = ch.to_ascii_lowercase();
        if low.is_ascii_alphanumeric() || low == '_' {
            base.push(low);
            dash = false;
        } else if !base.is_empty() && !dash {
            base.push('-');
            dash = true;
        }
        if base.len() >= 48 {
            break;
        }
    }
    let base = base.trim_matches(['-', '.', '_']).to_string();
    let base = if base.is_empty() {
        "account".to_string()
    } else {
        base
    };
    if !existing.contains_key(&base) {
        return base;
    }
    for n in 2..=999 {
        let candidate = format!("{base}-{n}");
        if !existing.contains_key(&candidate) {
            return candidate;
        }
    }
    format!("{base}-{}", now_ns() % 100_000)
}

fn load_state(root: &Path) -> Value {
    let v = json(accounts_meta_path(root));
    if v.is_object() && v["accounts"].is_object() {
        v
    } else {
        json!({
            "schemaVersion": SCHEMA_VERSION,
            "active": Value::Null,
            "accounts": {}
        })
    }
}

fn save_state(root: &Path, state: &Value) -> Res<()> {
    ensure_dir_mode(root, 0o700)?;
    let bytes = format!(
        "{}\n",
        serde_json::to_string_pretty(state).map_err(|e| e.to_string())?
    );
    write_secret_0600(&accounts_meta_path(root), bytes.as_bytes(), true)
}

fn read_email_from_json_file(path: &Path, keys: &[&str]) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    for key in keys {
        if let Some(val) = v.get(*key).and_then(Value::as_str) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Collect `(home_rel, email)` from all email-bearing credential files under `home_dir`,
/// prioritized in the order `agy` actually authenticates (`.gemini/antigravity-cli/antigravity-oauth-token`
/// first; `google_accounts.json` last because `agy` does not update it on token refresh).
fn collect_home_cred_emails(home_dir: &Path) -> Vec<(&'static str, String)> {
    const SOURCES: &[(&str, &str)] = &[
        (".gemini/antigravity-cli/antigravity-oauth-token", "id_token_email"),
        (".gemini/antigravity/antigravity-oauth-token", "id_token_email"),
        (".gemini/jetski-standalone-oauth-token", "id_token_email"),
        (".gemini/google_accounts.json", "active"),
    ];
    let mut out = Vec::new();
    for (rel, key) in SOURCES {
        if let Some(email) = read_email_from_json_file(&home_dir.join(rel), &[*key]) {
            out.push((*rel, email));
        }
    }
    out
}

fn live_email(home_dir: &Path) -> Option<String> {
    collect_home_cred_emails(home_dir)
        .into_iter()
        .next()
        .map(|(_, email)| email)
}

fn slot_email(dir: &Path) -> Option<String> {
    read_email_from_json_file(&dir.join("antigravity-cli-oauth-token"), &["id_token_email"])
        .or_else(|| {
            read_email_from_json_file(&dir.join("antigravity-oauth-token"), &["id_token_email"])
        })
        .or_else(|| {
            read_email_from_json_file(&dir.join("jetski-standalone-oauth-token"), &["id_token_email"])
        })
        .or_else(|| read_email_from_json_file(&dir.join("google_accounts.json"), &["active"]))
}

/// Read only the non-secret `token.expiry` ISO timestamp from local slot files.
fn slot_token_expiry(dir: &Path) -> Option<String> {
    for name in [
        "antigravity-cli-oauth-token",
        "antigravity-oauth-token",
        "jetski-standalone-oauth-token",
    ] {
        let p = dir.join(name);
        if let Ok(raw) = fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                if let Some(exp) = v["token"]["expiry"].as_str().filter(|s| !s.is_empty()) {
                    return Some(exp.to_string());
                }
            }
        }
    }
    None
}

fn slot_present_files(dir: &Path) -> Vec<&'static str> {
    CRED_FILES
        .iter()
        .filter(|cf| dir.join(cf.slot_name).is_file())
        .map(|cf| cf.slot_name)
        .collect()
}

fn slot_has_core_token(dir: &Path) -> bool {
    CRED_FILES.iter().any(|cf| {
        cf.is_core_token
            && fs::metadata(dir.join(cf.slot_name)).is_ok_and(|m| m.is_file() && m.len() > 0)
    })
}

fn home_has_any_live_creds(home_dir: &Path) -> bool {
    CRED_FILES
        .iter()
        .any(|cf| fs::metadata(home_dir.join(cf.home_rel)).is_ok_and(|m| m.is_file() && m.len() > 0))
}

fn path_basename(p: &str) -> &str {
    Path::new(p)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
}

fn is_agy_argv(argv: &[String], exe: Option<&Path>) -> bool {
    if argv.is_empty() {
        return false;
    }
    let b0 = path_basename(&argv[0]);
    if matches!(b0, "agy-swap" | "delegate" | "delegate-tui") {
        return false;
    }
    if b0 == "agy" {
        return true;
    }
    if let Some(exe_path) = exe {
        if exe_path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name == "agy")
        {
            return true;
        }
    }
    if matches!(b0, "sh" | "bash" | "dash" | "ash" | "zsh") {
        if let Some(arg1) = argv.get(1) {
            if path_basename(arg1) == "agy" {
                return true;
            }
        }
    }
    false
}

/// Detect running `agy` processes owned by the current UID that share the current `HOME`.
/// Pure `/proc` read (zero locks, zero subprocesses).
pub fn detect_running_agy_for_home(target_home: &Path) -> Vec<RunningAgy> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let my_pid = std::process::id();
    let Ok(my_uid) = fs::metadata("/proc/self").map(|m| m.uid()) else {
        return Vec::new();
    };
    let target_canon = fs::canonicalize(target_home).unwrap_or_else(|_| target_home.to_path_buf());
    let mut found = Vec::new();

    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(pid_str) = fname.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if pid == my_pid {
            continue;
        }
        let proc_dir = entry.path();
        // Fast path: read only `cmdline` first; skip `metadata`/`stat`/`exe`/`environ` for
        // the 99%+ of processes whose cmdline does not mention `agy`.
        let Ok(raw_cmd) = fs::read(proc_dir.join("cmdline")) else {
            continue;
        };
        if raw_cmd.is_empty() || !raw_cmd.windows(3).any(|w| w == b"agy") {
            continue;
        }
        let Ok(meta) = fs::metadata(&proc_dir) else {
            continue;
        };
        if meta.uid() != my_uid {
            continue;
        }
        // Skip zombies
        let stat = read(proc_dir.join("stat"));
        if let Some((_, tail)) = stat.rsplit_once(") ") {
            if tail.starts_with('Z') {
                continue;
            }
        }
        let argv: Vec<String> = raw_cmd
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        if argv.is_empty() {
            continue;
        }
        let exe = fs::read_link(proc_dir.join("exe")).ok();
        if !is_agy_argv(&argv, exe.as_deref()) {
            continue;
        }

        // Scope by HOME (or DELEGATE_AGY_REAL_HOME for delegate's per-process credential overlays)
        // so isolated temp HOMEs (e.g. `agy account add`, quota probes, or other-HOME tests) do not collide.
        let mut delegate_run: Option<String> = None;
        let mut run_name: Option<String> = None;
        let mut role: Option<String> = None;
        let mut round: Option<String> = None;
        let mut account: Option<String> = None;
        if let Ok(raw_env) = fs::read(proc_dir.join("environ")) {
            let mut proc_home: Option<PathBuf> = None;
            let mut delegate_real_home: Option<PathBuf> = None;
            for item in raw_env.split(|&b| b == 0) {
                if let Some(val) = item.strip_prefix(b"HOME=") {
                    proc_home = Some(PathBuf::from(String::from_utf8_lossy(val).into_owned()));
                } else if let Some(val) = item.strip_prefix(b"DELEGATE_AGY_REAL_HOME=") {
                    let s = String::from_utf8_lossy(val).into_owned();
                    if !s.is_empty() {
                        delegate_real_home = Some(PathBuf::from(s));
                    }
                } else if let Some(val) = item.strip_prefix(b"DELEGATE_AGY_ACCOUNT=") {
                    let s = String::from_utf8_lossy(val).into_owned();
                    if !s.is_empty() {
                        account = Some(s);
                    }
                } else if let Some(val) = item.strip_prefix(b"DELEGATE_RUN_DIR=") {
                    let p = PathBuf::from(String::from_utf8_lossy(val).into_owned());
                    let (actual_run_dir, detected_role, detected_round) =
                        parse_delegate_run_dir(&p);
                    if let Some(id) = actual_run_dir.file_name().and_then(|n| n.to_str()) {
                        if !id.is_empty() {
                            delegate_run = Some(id.to_string());
                        }
                    }
                    let meta_name = s(&json(actual_run_dir.join("meta.json")), "name").to_string();
                    if !meta_name.is_empty() {
                        run_name = Some(meta_name);
                    }
                    role = detected_role;
                    round = detected_round;
                }
            }
            if let Some(ph) = delegate_real_home.or(proc_home) {
                let ph_canon = fs::canonicalize(&ph).unwrap_or(ph);
                if ph_canon != target_canon {
                    continue;
                }
            }
        }

        found.push(RunningAgy {
            pid,
            delegate_run,
            run_name,
            role,
            round,
            account,
            cmdline: clip(&argv.join(" "), 120),
        });
    }

    found.sort_by_key(|p| p.pid);
    found
}

/// Lock-free lookup of the active `agy` account slot ID (never reads or returns email).
pub fn active_account_id() -> Option<String> {
    let root = accounts_root();
    let state = load_state(&root);
    state["active"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Strictly lock-free, zero-subprocess snapshot of local `agy` accounts for
/// `delegate snapshot --json`, `delegate agy account list/status`, and `delegate-tui`.
/// Never reads or exposes secret tokens.
pub fn snapshot_json() -> Value {
    snapshot_json_opts(true)
}

// ---------------------------------------------------------------------------
// Real Quota via `agy -p /usage </dev/null` + Cache in State Dir
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaPoolEntry {
    pub pool: String,
    pub window: String,       // "weekly" | "fiveHour" | "other"
    pub window_label: String, // e.g. "Weekly Limit Remaining"
    pub remaining_pct: f64,
    pub reset_at: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedAgyUsage {
    pub entries: Vec<QuotaPoolEntry>,
    pub unknown_lines: Vec<String>,
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for ch in chars.by_ref() {
                        if ('@'..='~').contains(&ch) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(ch) = chars.next() {
                        if ch == '\x07' {
                            break;
                        }
                        if ch == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        if c != '\r' {
            out.push(c);
        }
    }
    out
}

fn classify_usage_window(label: &str) -> &'static str {
    let low = label.to_ascii_lowercase();
    if low.contains("weekly") || low.contains("week") || low.contains("7d") || low.contains("7 day") {
        "weekly"
    } else if low.contains("five hour")
        || low.contains("5 hour")
        || low.contains("5h")
        || low.contains("five-hour")
        || low.contains("5-hour")
    {
        "fiveHour"
    } else {
        "other"
    }
}

fn parse_pct_value(raw: &str) -> Option<f64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let num = trimmed.strip_suffix('%').unwrap_or(trimmed).trim();
    let v = num.parse::<f64>().ok()?;
    if !v.is_finite() || !(0.0..=100.0).contains(&v) {
        return None;
    }
    Some((v * 100.0).round() / 100.0)
}

/// Parse stdout of `agy -p /usage </dev/null`.
///
/// Expected TSV format:
///   `Gemini Models\tWeekly Limit Remaining\t97%\t2026-11-30T20:09:03Z`
///   `Claude and GPT models\tFive Hour Limit Remaining\t92%\t2026-11-24T07:32:40Z`
///
/// Missing columns (e.g. absent reset timestamp) or missing windows are tolerated,
/// and any unrecognized/non-TSV lines are preserved verbatim in `unknown_lines` without error.
pub fn parse_agy_usage_output(raw: &str) -> ParsedAgyUsage {
    let mut parsed = ParsedAgyUsage::default();
    for raw_line in raw.lines() {
        let clean = strip_ansi(raw_line);
        let trimmed = clean.trim();
        if trimmed.is_empty() {
            continue;
        }
        let cols: Vec<&str> = clean.split('\t').map(str::trim).collect();
        if cols.len() >= 3 && !cols[0].is_empty() && !cols[1].is_empty() {
            if let Some(pct) = parse_pct_value(cols[2]) {
                let window = classify_usage_window(cols[1]).to_string();
                let reset_at = cols.get(3).copied().unwrap_or("").to_string();
                parsed.entries.push(QuotaPoolEntry {
                    pool: cols[0].to_string(),
                    window,
                    window_label: cols[1].to_string(),
                    remaining_pct: pct,
                    reset_at,
                });
                continue;
            }
        }
        parsed.unknown_lines.push(trimmed.to_string());
    }
    parsed
}

pub fn quota_cache_path() -> PathBuf {
    state_dir().join("agy-quota.json")
}

/// Lock-free read of `<state>/agy-quota.json`.
pub fn load_quota_cache() -> Value {
    let v = json(quota_cache_path());
    if v.is_object() && v["accounts"].is_object() {
        v
    } else {
        json!({
            "schemaVersion": QUOTA_CACHE_SCHEMA_VERSION,
            "updatedAt": "",
            "updatedEpoch": 0,
            "accounts": {}
        })
    }
}

fn save_quota_cache(doc: &Value) -> Res<()> {
    let path = quota_cache_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    write_json(&path, doc)
}

pub fn format_age_compact(age_secs: i64) -> String {
    let s = age_secs.max(0);
    if s < 60 {
        format!("{s}s前")
    } else if s < 3600 {
        format!("{}m前", s / 60)
    } else if s < 86400 {
        let h = s / 3600;
        let m = (s % 3600) / 60;
        if m == 0 {
            format!("{h}h前")
        } else {
            format!("{h}h{m:02}m前")
        }
    } else {
        let d = s / 86400;
        let h = (s % 86400) / 3600;
        if h == 0 {
            format!("{d}d前")
        } else {
            format!("{d}d{h}h前")
        }
    }
}

pub fn format_countdown_compact(rem_secs: i64) -> String {
    if rem_secs <= 0 {
        return "已重置".to_string();
    }
    if rem_secs < 60 {
        format!("{rem_secs}s")
    } else if rem_secs < 3600 {
        format!("{}m", rem_secs / 60)
    } else if rem_secs < 48 * 3600 {
        let h = rem_secs / 3600;
        let m = (rem_secs % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m:02}m")
        }
    } else {
        let d = rem_secs / 86400;
        let h = (rem_secs % 86400) / 3600;
        if h == 0 {
            format!("{d}d")
        } else {
            format!("{d}d{h}h")
        }
    }
}

/// Enrich a cached per-account quota record with live `ageSeconds` and per-entry `resetInSeconds`.
pub fn enrich_quota_for_snapshot(raw_acct: &Value, now_ep: i64) -> Option<Value> {
    if !raw_acct.is_object() {
        return None;
    }
    let fetched_at = s(raw_acct, "fetchedAt");
    let fetched_ep = raw_acct["fetchedEpoch"]
        .as_i64()
        .or_else(|| iso_epoch(fetched_at))
        .unwrap_or(0);
    if fetched_at.is_empty() && fetched_ep == 0 {
        return None;
    }
    let age_seconds = (now_ep - fetched_ep).max(0);
    let entries: Vec<Value> = raw_acct["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let mut obj = e.clone();
            let reset_at = s(e, "resetAt");
            if let Some(rep) = iso_epoch(reset_at) {
                obj["resetInSeconds"] = json!((rep - now_ep).max(0));
            }
            obj
        })
        .collect();
    let unknown_lines: Vec<Value> = raw_acct["unknownLines"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut out = json!({
        "fetchedAt": fetched_at,
        "fetchedEpoch": fetched_ep,
        "ageSeconds": age_seconds,
        "ok": b(raw_acct, "ok"),
        "entries": entries,
        "unknownLines": unknown_lines,
    });
    if b(raw_acct, "identityMismatch") {
        out["identityMismatch"] = json!(true);
    }
    if let Some(exp) = raw_acct["expectedEmail"].as_str().filter(|e| !e.is_empty()) {
        out["expectedEmail"] = json!(exp);
    }
    if let Some(obs) = raw_acct["observedEmail"].as_str().filter(|e| !e.is_empty()) {
        out["observedEmail"] = json!(obs);
    }
    if let Some(at) = raw_acct["mismatchAt"].as_str().filter(|e| !e.is_empty()) {
        out["mismatchAt"] = json!(at);
    }
    if let Some(err) = raw_acct["error"].as_str().filter(|e| !e.is_empty()) {
        out["error"] = json!(err);
    }
    Some(out)
}

fn quota_timeout_secs() -> u64 {
    env::var("DELEGATE_AGY_QUOTA_TIMEOUT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_QUOTA_TIMEOUT_SECS)
}

fn preserve_real_toolchain_envs(cmd: &mut Command, real_home: &Path) {
    for (key, rel) in [("RUSTUP_HOME", ".rustup"), ("CARGO_HOME", ".cargo")] {
        if env::var_os(key).is_none_or(|v| v.is_empty()) {
            cmd.env(key, real_home.join(rel));
        }
    }
}

fn run_agy_usage_cmd(
    agy_bin: &Path,
    target_home: &Path,
    real_home: &Path,
    isolated_temp_home: bool,
    timeout_secs: u64,
) -> Result<(String, String), String> {
    let mut cmd = Command::new(agy_bin);
    cmd.args(["-p", "/usage"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("AGY_SWAP_SKIP_SYNC", "1");
    if isolated_temp_home {
        cmd.env("HOME", target_home)
            .env("XDG_CONFIG_HOME", target_home.join(".config"))
            .env("XDG_CACHE_HOME", target_home.join(".cache"))
            .env("XDG_STATE_HOME", target_home.join(".local/state"));
        preserve_real_toolchain_envs(&mut cmd, real_home);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run {} -p /usage: {e}", agy_bin.display()))?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let out_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut p) = stdout_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut p) = stderr_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Ok(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(format!(
                        "agy -p /usage timed out after {timeout_secs}s"
                    ));
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(e) => break Err(format!("wait agy -p /usage failed: {e}")),
        }
    };

    let stdout_bytes = out_handle.join().unwrap_or_default();
    let stderr_bytes = err_handle.join().unwrap_or_default();
    let st = status?;
    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
    if !st.success() {
        let msg = if !stderr.trim().is_empty() {
            clip(stderr.trim(), 240)
        } else if !stdout.trim().is_empty() {
            clip(stdout.trim(), 240)
        } else {
            format!("exit status {st}")
        };
        return Err(format!("agy -p /usage failed: {msg}"));
    }
    Ok((stdout, stderr))
}

const IDENTITY_MISMATCH_MSG: &str = "身份不符，未更新";

fn is_valid_email_token(cand: &str) -> bool {
    let Some((local, domain)) = cand.split_once('@') else {
        return false;
    };
    if local.is_empty()
        || domain.is_empty()
        || domain.contains('@')
        || local.starts_with('.')
        || local.ends_with('.')
        || !local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'+' | b'-'))
    {
        return false;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    for label in &labels {
        if label.is_empty()
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return false;
        }
    }
    let tld = labels[labels.len() - 1];
    tld.len() >= 2 && tld.bytes().all(|b| b.is_ascii_alphabetic())
}

fn push_unique_email_ci(dst: &mut Vec<String>, email: &str) {
    let trimmed = email.trim();
    if trimmed.is_empty() {
        return;
    }
    if !dst.iter().any(|e| e.eq_ignore_ascii_case(trimmed)) {
        dst.push(trimmed.to_string());
    }
}

fn extract_email_tokens(line: &str) -> Vec<String> {
    let clean = strip_ansi(line);
    let mut found = Vec::new();
    for raw_tok in clean.split_whitespace() {
        if raw_tok.contains("://") || raw_tok.contains('/') {
            continue;
        }
        let stripped = raw_tok.trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | ',' | ';' | '(' | ')' | '<' | '>' | '[' | ']' | '{' | '}' | '.'
            )
        });
        let candidate = stripped
            .rsplit_once('=')
            .or_else(|| stripped.rsplit_once(':'))
            .map(|(_, rhs)| rhs)
            .unwrap_or(stripped)
            .trim_matches(|c: char| {
                matches!(
                    c,
                    '"' | '\'' | ',' | ';' | '(' | ')' | '<' | '>' | '[' | ']' | '{' | '}' | '.'
                )
            });
        if is_valid_email_token(candidate) {
            push_unique_email_ci(&mut found, candidate);
        }
    }
    found
}

/// Extract any identity emails present in `agy -p /usage` stdout/stderr (skipping standard TSV quota rows).
fn extract_identity_emails_from_output(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in text.lines() {
        let clean = strip_ansi(line);
        let cols: Vec<&str> = clean.split('\t').map(str::trim).collect();
        if cols.len() >= 3 && !cols[0].is_empty() && !cols[1].is_empty() && parse_pct_value(cols[2]).is_some() {
            continue;
        }
        for email in extract_email_tokens(&clean) {
            push_unique_email_ci(&mut found, &email);
        }
    }
    found
}

/// Extract OAuth authenticated emails written by `agy` to its isolated `TempHome` log directory.
fn extract_identity_emails_from_agy_logs(temp_home: &Path) -> Vec<String> {
    let log_dir = temp_home.join(".gemini/antigravity-cli/log");
    let Ok(entries) = fs::read_dir(&log_dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("log"))
        .collect();
    paths.sort();
    let mut found = Vec::new();
    for p in paths {
        let Ok(raw) = fs::read_to_string(&p) else {
            continue;
        };
        for line in raw.lines() {
            let low = line.to_ascii_lowercase();
            if low.contains("applyauthresult")
                || low.contains("authenticated successfully as")
                || low.contains("oauth: authenticated")
            {
                for email in extract_email_tokens(line) {
                    push_unique_email_ci(&mut found, &email);
                }
            }
        }
    }
    found
}

/// Verify that the identity actually used during `/usage` matches `expected_email`.
/// Returns `Ok(())` when verified, or `Err(observed_email)` (empty string if no identity could be observed).
fn verify_slot_identity(
    expected_email: &str,
    cred_emails: &[(&str, String)],
    output_emails: &[String],
    log_emails: &[String],
) -> Result<(), String> {
    let expected = expected_email.trim();
    let mut observed: Vec<String> = Vec::new();
    for e in log_emails {
        push_unique_email_ci(&mut observed, e);
    }
    for e in output_emails {
        push_unique_email_ci(&mut observed, e);
    }
    for (_, e) in cred_emails {
        push_unique_email_ci(&mut observed, e);
    }

    if expected.is_empty() {
        if observed.len() > 1 {
            return Err(observed[1].clone());
        }
        return Ok(());
    }

    if observed.is_empty() {
        return Err(String::new());
    }
    for actual in &observed {
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(actual.clone());
        }
    }
    Ok(())
}

#[derive(Debug)]
struct SlotQuotaFetch {
    id: String,
    ok: bool,
    identity_mismatch: bool,
    expected_email: String,
    observed_email: String,
    error: Option<String>,
    parsed: ParsedAgyUsage,
    refreshed_creds: usize,
}

impl SlotQuotaFetch {
    fn error_result(id: &str, expected_email: &str, err: String) -> Self {
        Self {
            id: id.to_string(),
            ok: false,
            identity_mismatch: false,
            expected_email: expected_email.to_string(),
            observed_email: String::new(),
            error: Some(err),
            parsed: ParsedAgyUsage::default(),
            refreshed_creds: 0,
        }
    }

    fn mismatch_result(id: &str, expected_email: &str, observed_email: String) -> Self {
        Self {
            id: id.to_string(),
            ok: false,
            identity_mismatch: true,
            expected_email: expected_email.to_string(),
            observed_email,
            error: Some(IDENTITY_MISMATCH_MSG.to_string()),
            parsed: ParsedAgyUsage::default(),
            refreshed_creds: 0,
        }
    }
}

fn seed_keyring_markers(real_home: &Path, temp_home: &Path) {
    for marker in KEYRING_MARKERS {
        let src = real_home.join(marker);
        if src.is_file() {
            let dst = temp_home.join(marker);
            if let Some(parent) = dst.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::copy(&src, &dst);
        }
    }
}

/// Run `agy -p /usage </dev/null` for a single account slot in an isolated `0700` temp HOME.
///
/// Every slot — including the active slot — runs from its own stored slot credentials in a
/// fresh `TempHome`, never in real `HOME`, so long-lived interactive `agy` processes refreshing
/// another account's token into `~/.gemini/antigravity-cli/antigravity-oauth-token` can never
/// cross-contaminate quota results.
///
/// Before accepting quota or writing back refreshed credentials (`0600`, under `.lock`),
/// verifies that the actual identity (slot credential emails + `agy` output + isolated `agy` log)
/// equals the target slot's expected email.
fn fetch_quota_for_slot(
    agy_bin: &Path,
    real_home: &Path,
    root: &Path,
    id: &str,
    expected_email_in: &str,
    timeout_secs: u64,
) -> SlotQuotaFetch {
    let sdir = slot_dir(root, id);
    let expected_email = {
        let trimmed = expected_email_in.trim();
        if !trimmed.is_empty() {
            trimmed.to_string()
        } else {
            slot_email(&sdir).unwrap_or_default()
        }
    };
    if !slot_has_core_token(&sdir) {
        return SlotQuotaFetch::error_result(
            id,
            &expected_email,
            "slot missing core OAuth credentials".to_string(),
        );
    }

    let temp_home = match TempHome::create() {
        Ok(t) => t,
        Err(err) => return SlotQuotaFetch::error_result(id, &expected_email, err),
    };
    seed_keyring_markers(real_home, &temp_home.path);
    if let Err(err) = install_slot_to_home(&sdir, &temp_home.path) {
        return SlotQuotaFetch::error_result(id, &expected_email, err);
    }

    // Pre-run identity guard: if any email-bearing credential in the slot disagrees with
    // `expected_email`, refuse before invoking `agy`.
    let pre_cred_emails = collect_home_cred_emails(&temp_home.path);
    if !expected_email.is_empty() {
        for (_, actual) in &pre_cred_emails {
            if !actual.eq_ignore_ascii_case(&expected_email) {
                return SlotQuotaFetch::mismatch_result(id, &expected_email, actual.clone());
            }
        }
    }

    // Snapshot credential bytes before running `agy -p /usage` to detect token refreshes.
    let mut before: BTreeMap<&'static str, Option<Vec<u8>>> = BTreeMap::new();
    for cf in CRED_FILES {
        let p = temp_home.path.join(cf.home_rel);
        let bytes = fs::read(&p).ok();
        before.insert(cf.slot_name, bytes);
    }

    let run_res = run_agy_usage_cmd(agy_bin, &temp_home.path, real_home, true, timeout_secs);

    let (stdout, stderr) = match run_res {
        Ok(pair) => pair,
        Err(err) => {
            drop(temp_home);
            return SlotQuotaFetch::error_result(id, &expected_email, err);
        }
    };

    // Post-run identity guard: verify credential files, agy stdout/stderr, and isolated agy logs
    // all match `expected_email` before persisting quota or writing back refreshed credentials.
    let post_cred_emails = collect_home_cred_emails(&temp_home.path);
    let mut output_emails = extract_identity_emails_from_output(&stdout);
    for e in extract_identity_emails_from_output(&stderr) {
        push_unique_email_ci(&mut output_emails, &e);
    }
    let log_emails = extract_identity_emails_from_agy_logs(&temp_home.path);
    if let Err(observed) =
        verify_slot_identity(&expected_email, &post_cred_emails, &output_emails, &log_emails)
    {
        drop(temp_home);
        return SlotQuotaFetch::mismatch_result(id, &expected_email, observed);
    }

    // Check if any credential files under temp HOME (e.g. ~/.gemini/*) were refreshed.
    let mut refreshed: Vec<(&CredFile, Vec<u8>)> = Vec::new();
    for cf in CRED_FILES {
        let p = temp_home.path.join(cf.home_rel);
        if let Ok(after_bytes) = fs::read(&p) {
            if !after_bytes.is_empty()
                && before.get(cf.slot_name).and_then(Option::as_ref) != Some(&after_bytes)
                && cred_file_valid_for_email(cf, &after_bytes, &expected_email)
            {
                refreshed.push((cf, after_bytes));
            }
        }
    }

    let mut refreshed_creds = 0usize;
    if !refreshed.is_empty() {
        if let Ok(_lock) = acquire_accounts_lock(root) {
            let _ = ensure_dir_mode(&sdir, 0o700);
            for (cf, bytes) in &refreshed {
                let dst = sdir.join(cf.slot_name);
                if should_update_slot_file(&dst, bytes) && write_secret_0600(&dst, bytes, true).is_ok()
                {
                    refreshed_creds += 1;
                }
            }
            if refreshed_creds > 0 {
                let mut st = load_state(root);
                if let Some(obj) = st["accounts"][id].as_object_mut() {
                    obj.insert("lastSyncedAt".into(), json!(iso()));
                }
                let _ = save_state(root, &st);
            }
        }
    }

    drop(temp_home);

    SlotQuotaFetch {
        id: id.to_string(),
        ok: true,
        identity_mismatch: false,
        expected_email,
        observed_email: String::new(),
        error: None,
        parsed: parse_agy_usage_output(&stdout),
        refreshed_creds,
    }
}

fn build_quota_history_point(now_ep: i64, entries: &[QuotaPoolEntry]) -> Option<Value> {
    if entries.is_empty() {
        return None;
    }
    let mut min_weekly: Option<f64> = None;
    let mut min_five_hour: Option<f64> = None;
    let mut pools: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for e in entries {
        match e.window.as_str() {
            "weekly" => {
                min_weekly = Some(min_weekly.map_or(e.remaining_pct, |p| p.min(e.remaining_pct)));
                pools
                    .entry(e.pool.clone())
                    .or_default()
                    .insert("weekly".into(), json!(e.remaining_pct));
            }
            "fiveHour" => {
                min_five_hour =
                    Some(min_five_hour.map_or(e.remaining_pct, |p| p.min(e.remaining_pct)));
                pools
                    .entry(e.pool.clone())
                    .or_default()
                    .insert("fiveHour".into(), json!(e.remaining_pct));
            }
            _ => {}
        }
    }
    if min_weekly.is_none() && min_five_hour.is_none() {
        return None;
    }
    let mut pt = Map::new();
    pt.insert("epoch".into(), json!(now_ep));
    if let Some(w) = min_weekly {
        pt.insert("weekly".into(), json!(w));
    }
    if let Some(fh) = min_five_hour {
        pt.insert("fiveHour".into(), json!(fh));
    }
    if !pools.is_empty() {
        let pools_val: Map<String, Value> = pools
            .into_iter()
            .map(|(k, v)| (k, Value::Object(v)))
            .collect();
        pt.insert("pools".into(), Value::Object(pools_val));
    }
    Some(Value::Object(pt))
}

/// Refresh real quota for all configured `agy` account slots via `agy -p /usage </dev/null`
/// and atomically persist the results + rolling history to `<state>/agy-quota.json`.
pub fn refresh_all_quotas(agy_bin_opt: Option<&str>) -> Res<Value> {
    let agy_bin = resolve_agy_bin(agy_bin_opt)?;
    let root = accounts_root();
    let real_home = home();
    let state = load_state(&root);
    let timeout_secs = quota_timeout_secs();

    let mut targets: Vec<(String, String)> = Vec::new();
    if let Some(map) = state["accounts"].as_object() {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for id in keys {
            let entry = &map[id];
            let email = s(entry, "email").to_string();
            targets.push((id.clone(), email));
        }
    }

    let mut results: Vec<SlotQuotaFetch> = Vec::with_capacity(targets.len());
    thread::scope(|sc| {
        let handles: Vec<_> = targets
            .iter()
            .map(|(id, email)| {
                let agy_bin = &agy_bin;
                let real_home = &real_home;
                let root = &root;
                let id_str = id.as_str();
                let email_str = email.as_str();
                sc.spawn(move || {
                    fetch_quota_for_slot(agy_bin, real_home, root, id_str, email_str, timeout_secs)
                })
            })
            .collect();
        for h in handles {
            if let Ok(r) = h.join() {
                results.push(r);
            }
        }
    });

    let now_iso = iso();
    let now_ep = epoch() as i64;
    let cutoff = now_ep - QUOTA_HISTORY_RETENTION_SECS;

    let _lock = crate::locks::acquire_state(&state_dir()).ok();
    let mut cache = load_quota_cache();
    if !cache["accounts"].is_object() {
        cache["accounts"] = json!({});
    }
    cache["schemaVersion"] = json!(QUOTA_CACHE_SCHEMA_VERSION);
    cache["updatedAt"] = json!(now_iso);
    cache["updatedEpoch"] = json!(now_ep);

    for res in results {
        let prev_acct = cache["accounts"][&res.id].clone();
        let mut history: Vec<Value> = prev_acct["history"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|pt| pt["epoch"].as_i64().unwrap_or(0) >= cutoff)
            .collect();

        if res.identity_mismatch {
            // Identity mismatch: NEVER store the fetched quota entries or append to history.
            // Preserve prior cached entries/history/timestamp (if any) and mark `identityMismatch`.
            let prev_entries = prev_acct["entries"].as_array().cloned().unwrap_or_default();
            let prev_unknown = prev_acct["unknownLines"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let prev_fetched_at = s(&prev_acct, "fetchedAt").to_string();
            let prev_fetched_ep = prev_acct["fetchedEpoch"].as_i64().unwrap_or(0);
            let (keep_at, keep_ep) = if !prev_fetched_at.is_empty() || prev_fetched_ep > 0 {
                (prev_fetched_at, prev_fetched_ep)
            } else {
                (now_iso.clone(), now_ep)
            };
            let mut acct_doc = json!({
                "fetchedAt": keep_at,
                "fetchedEpoch": keep_ep,
                "ok": false,
                "identityMismatch": true,
                "mismatchAt": now_iso,
                "error": res.error.unwrap_or_else(|| IDENTITY_MISMATCH_MSG.to_string()),
                "entries": prev_entries,
                "unknownLines": prev_unknown,
                "refreshedCreds": 0,
                "history": history,
            });
            if !res.expected_email.is_empty() {
                acct_doc["expectedEmail"] = json!(res.expected_email);
            }
            if !res.observed_email.is_empty() {
                acct_doc["observedEmail"] = json!(res.observed_email);
            }
            cache["accounts"][&res.id] = acct_doc;
            continue;
        }

        if res.ok {
            if let Some(pt) = build_quota_history_point(now_ep, &res.parsed.entries) {
                let dup = history.last().is_some_and(|prev| {
                    (now_ep - prev["epoch"].as_i64().unwrap_or(0)).abs() <= 10
                        && prev["weekly"] == pt["weekly"]
                        && prev["fiveHour"] == pt["fiveHour"]
                        && prev["pools"] == pt["pools"]
                });
                if !dup {
                    history.push(pt);
                }
            }
        }
        if history.len() > QUOTA_HISTORY_LIMIT {
            let drop_n = history.len() - QUOTA_HISTORY_LIMIT;
            history.drain(0..drop_n);
        }

        let entries_json: Vec<Value> = res
            .parsed
            .entries
            .iter()
            .map(|e| {
                json!({
                    "pool": e.pool,
                    "window": e.window,
                    "windowLabel": e.window_label,
                    "remainingPct": e.remaining_pct,
                    "resetAt": e.reset_at,
                })
            })
            .collect();

        let mut acct_doc = json!({
            "fetchedAt": now_iso,
            "fetchedEpoch": now_ep,
            "ok": res.ok,
            "entries": entries_json,
            "unknownLines": res.parsed.unknown_lines,
            "refreshedCreds": res.refreshed_creds,
            "history": history,
        });
        if let Some(err) = res.error {
            acct_doc["error"] = json!(err);
        }
        cache["accounts"][&res.id] = acct_doc;
    }

    save_quota_cache(&cache)?;
    Ok(cache)
}

type QuotaEntrySig = (String, String, String, i64, String);

fn quota_entries_signature(quota: &Value) -> Option<Vec<QuotaEntrySig>> {
    let arr = quota["entries"].as_array()?;
    if arr.is_empty() {
        return None;
    }
    let mut sig: Vec<QuotaEntrySig> = arr
        .iter()
        .map(|e| {
            let pct_x100 = (e["remainingPct"].as_f64().unwrap_or(0.0) * 100.0).round() as i64;
            (
                s(e, "pool").to_string(),
                s(e, "window").to_string(),
                s(e, "windowLabel").to_string(),
                pct_x100,
                s(e, "resetAt").to_string(),
            )
        })
        .collect();
    sig.sort();
    Some(sig)
}

/// Detect groups of 2+ account slots whose quota `entries` are identical entry-by-entry
/// (including `resetAt`). Weekly reset timestamps are per-account, so identical entries across
/// slots strongly indicate cross-account quota contamination.
fn find_duplicate_quota_slot_groups(accounts: &[Value]) -> Vec<Vec<String>> {
    let mut by_sig: BTreeMap<Vec<QuotaEntrySig>, Vec<String>> = BTreeMap::new();
    for item in accounts {
        let id = s(item, "id");
        if id.is_empty() || !item["quota"].is_object() {
            continue;
        }
        if let Some(sig) = quota_entries_signature(&item["quota"]) {
            by_sig.entry(sig).or_default().push(id.to_string());
        }
    }
    let mut groups: Vec<Vec<String>> = by_sig
        .into_values()
        .filter(|ids| ids.len() >= 2)
        .map(|mut ids| {
            ids.sort();
            ids
        })
        .collect();
    groups.sort();
    groups
}

/// Fast snapshot for `delegate snapshot --json`: skips the `/proc` scan when no local
/// `agy-accounts/accounts.json` exists yet so snapshot stays well under 20 ms.
pub fn snapshot_json_fast() -> Value {
    let has_accounts = accounts_meta_path(&accounts_root()).is_file();
    snapshot_json_opts(has_accounts)
}

/// Advisory-only quota depletion warning when projected depletion precedes window reset.
/// Never triggers automatic account switching or throttling.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaDepletionWarning {
    pub account_id: String,
    pub active: bool,
    pub pool: String,
    pub window: String,
    pub window_label: String,
    pub remaining_minutes: u64,
    pub reset_in_seconds: u64,
    pub remaining_pct: f64,
}

fn short_pool_label(pool: &str) -> String {
    let low = pool.to_ascii_lowercase();
    if low.contains("gemini") {
        "Gemini".to_string()
    } else if low.contains("claude") || low.contains("gpt") {
        "Claude/GPT".to_string()
    } else if pool.is_empty() {
        "额度".to_string()
    } else {
        pool.to_string()
    }
}

pub fn format_minutes_zh_short(mins: u64) -> String {
    if mins < 60 {
        return format!("{mins} 分钟");
    }
    let hours = mins / 60;
    let rem_m = mins % 60;
    if hours < 24 {
        if rem_m == 0 {
            format!("{hours} 小时")
        } else {
            format!("{hours} 小时 {rem_m} 分钟")
        }
    } else {
        let days = hours / 24;
        let rem_h = hours % 24;
        if rem_h == 0 {
            format!("{days} 天")
        } else {
            format!("{days} 天 {rem_h} 小时")
        }
    }
}

pub fn format_reset_zh_short(secs: u64) -> String {
    let mins = secs / 60;
    if mins < 60 {
        format!("{} 分钟", mins.max(1))
    } else if mins < 48 * 60 {
        let h = mins / 60;
        let m = mins % 60;
        if m == 0 {
            format!("{h} 小时")
        } else {
            format!("{h} 小时 {m} 分钟")
        }
    } else {
        let d = (secs + 43_200) / 86_400;
        format!("{d} 天")
    }
}

/// Extract quota depletion-before-reset warnings from an `agyAccounts` snapshot JSON object.
/// Sorted with the active account first, then earliest depletion first.
pub fn detect_quota_depletion_warnings(snap: &Value) -> Vec<QuotaDepletionWarning> {
    let mut warnings = Vec::new();
    let Some(accounts) = snap["accounts"].as_array() else {
        return warnings;
    };
    for acct in accounts {
        let id = s(acct, "id").to_string();
        if id.is_empty() {
            continue;
        }
        let active = b(acct, "active");
        let quota = &acct["quota"];
        if !b(quota, "ok") || b(quota, "identityMismatch") {
            continue;
        }
        let Some(entries) = quota["entries"].as_array() else {
            continue;
        };
        let qest = &acct["quotaEstimate"];
        if !qest.is_object() {
            continue;
        }
        let find_entry = |window_key: &str, pool_opt: Option<&str>| -> Option<(u64, f64)> {
            for e in entries {
                if s(e, "window") != window_key {
                    continue;
                }
                if let Some(p) = pool_opt {
                    if !p.is_empty() && s(e, "pool") != p {
                        continue;
                    }
                }
                if let Some(reset_s) = e["resetInSeconds"].as_u64() {
                    if reset_s > 0 {
                        let pct = e["remainingPct"].as_f64().unwrap_or(0.0);
                        return Some((reset_s, pct));
                    }
                }
            }
            None
        };
        let mut seen: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        let mut check_window = |est_obj: &Value, window_key: &str, pool_hint: Option<&str>| {
            if !est_obj.is_object() {
                return;
            }
            let Some(rem_mins) = est_obj["estimatedRemainingHours"]
                .as_f64()
                .filter(|h| h.is_finite() && *h >= 0.0)
                .map(|h| (h * 60.0).round() as u64)
                .or_else(|| est_obj["estimatedRemainingMinutes"].as_u64())
            else {
                return;
            };
            let pool_raw = pool_hint
                .filter(|p| !p.is_empty())
                .or_else(|| {
                    let p = s(est_obj, "pool");
                    (!p.is_empty()).then_some(p)
                });
            let Some((reset_secs, rem_pct)) = find_entry(window_key, pool_raw) else {
                return;
            };
            if rem_mins.saturating_mul(60) >= reset_secs {
                return;
            }
            let pool_disp = short_pool_label(pool_raw.unwrap_or(""));
            if !seen.insert((pool_disp.clone(), window_key.to_string())) {
                return;
            }
            let window_label = match window_key {
                "fiveHour" => "5 小时".to_string(),
                _ => "本周".to_string(),
            };
            warnings.push(QuotaDepletionWarning {
                account_id: id.clone(),
                active,
                pool: pool_disp,
                window: window_key.to_string(),
                window_label,
                remaining_minutes: rem_mins,
                reset_in_seconds: reset_secs,
                remaining_pct: rem_pct,
            });
        };

        if let Some(pools) = qest["pools"].as_array() {
            for pe in pools {
                let pname = s(pe, "pool");
                check_window(&pe["weekly"], "weekly", Some(pname));
                check_window(&pe["fiveHour"], "fiveHour", Some(pname));
            }
        }
        check_window(&qest["weekly"], "weekly", None);
        check_window(&qest["fiveHour"], "fiveHour", None);
    }

    warnings.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then_with(|| a.remaining_minutes.cmp(&b.remaining_minutes))
            .then_with(|| a.account_id.cmp(&b.account_id))
    });
    warnings
}

/// Lock-free, zero-subprocess quota depletion check scoped to `state` (`<state>/agy-quota.json`).
/// Returns empty when `<state>/agy-quota.json` does not exist so isolated tests never leak host state.
/// Build a lightweight lock-free accounts/quota snapshot for advisory alerts in `state`
/// without scanning `/proc` (isolated-state safe for unit/integration tests).
pub fn quota_snapshot_for_alerts_in(state: &Path, now_ep: i64) -> Option<Value> {
    let qpath = state.join("agy-quota.json");
    let root = accounts_root();
    let use_real_accounts = state == state_dir() && accounts_meta_path(&root).is_file();
    if use_real_accounts {
        return Some(snapshot_json_opts(false));
    }
    if !qpath.is_file() {
        return None;
    }
    let quota_cache = json(&qpath);
    if !quota_cache.is_object() {
        return None;
    }
    // Isolated state dir (e.g. unit/integration tests): build synthetic snapshot purely from `state`.
    let mut acct_map = Map::new();
    let mut active_id = String::new();
    if let Some(accts) = quota_cache["accounts"].as_object() {
        for (id, v) in accts {
            if b(v, "active") && active_id.is_empty() {
                active_id = id.clone();
            }
            acct_map.insert(id.clone(), json!({}));
        }
    }
    if acct_map.is_empty() {
        return None;
    }
    if active_id.is_empty() {
        if let Some(first) = acct_map.keys().next() {
            active_id = first.clone();
        }
    }
    let usage_snap = crate::usage::accounts_usage_snapshot_in(state, &acct_map, &quota_cache, now_ep);
    let mut items = Vec::new();
    for id in acct_map.keys() {
        let mut item = json!({
            "id": id,
            "active": id == &active_id,
        });
        if let Some(q) = enrich_quota_for_snapshot(&quota_cache["accounts"][id], now_ep) {
            item["quota"] = q;
        }
        if let Some(view) = usage_snap.per_account.get(id) {
            if !view.quota_estimate.is_null() {
                item["quotaEstimate"] = view.quota_estimate.clone();
            }
        }
        items.push(item);
    }
    Some(json!({ "active": active_id, "accounts": items }))
}

/// Format a prominent single-line warning for `delegate status` (advisory only).
pub fn format_status_quota_warning_line(warnings: &[QuotaDepletionWarning]) -> Option<String> {
    let top = warnings.first()?;
    let role = if top.active { "当前账号 " } else { "账号 " };
    let rem = format_minutes_zh_short(top.remaining_minutes);
    let reset = format_reset_zh_short(top.reset_in_seconds);
    let extra = if warnings.len() > 1 {
        format!("；另有 {} 项也预计提前用完", warnings.len() - 1)
    } else {
        String::new()
    };
    Some(format!(
        "⚠ 额度预警：{role}{} 的 {} {}额度约 {}后用完（{}后重置；不自动换号，请手动切换）{extra}",
        top.account_id, top.pool, top.window_label, rem, reset
    ))
}

fn snapshot_json_opts(scan_procs: bool) -> Value {
    let root = accounts_root();
    let home_dir = home();
    let state = load_state(&root);
    let active_id = state["active"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let live = live_email(&home_dir).unwrap_or_default();
    let running = if scan_procs {
        detect_running_agy_for_home(&home_dir)
    } else {
        Vec::new()
    };

    let now_ep = epoch() as i64;
    let quota_cache = load_quota_cache();
    let empty_map = Map::new();
    let accounts_map = state["accounts"].as_object().unwrap_or(&empty_map);
    let usage_snap = if !accounts_map.is_empty() || crate::usage::ledger_path().is_file() {
        Some(crate::usage::accounts_usage_snapshot(
            accounts_map,
            &quota_cache,
        ))
    } else {
        None
    };

    let mut items: Vec<Value> = Vec::new();
    let mut active_email = String::new();
    if let Some(map) = state["accounts"].as_object() {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for id in keys {
            let entry = &map[id];
            let sdir = slot_dir(&root, id);
            let email = {
                let stored = s(entry, "email");
                if !stored.is_empty() {
                    stored.to_string()
                } else {
                    slot_email(&sdir).unwrap_or_default()
                }
            };
            let is_active = active_id.as_deref() == Some(id.as_str());
            if is_active {
                active_email = email.clone();
            }
            let files = slot_present_files(&sdir);
            let token_expiry = slot_token_expiry(&sdir).unwrap_or_default();
            let usage_view = usage_snap.as_ref().and_then(|u| u.per_account.get(id));
            let stored_last_used = s(entry, "lastUsedAt");
            let effective_last_used = match usage_view.and_then(|v| v.last_used_at.as_deref()) {
                Some(u_ts) if !u_ts.is_empty() && (stored_last_used.is_empty() || u_ts > stored_last_used) => {
                    u_ts.to_string()
                }
                _ => stored_last_used.to_string(),
            };
            let mut item = json!({
                "id": id,
                "email": email,
                "label": s(entry, "label"),
                "active": is_active,
                "addedAt": s(entry, "addedAt"),
                "lastUsedAt": effective_last_used,
                "lastSyncedAt": s(entry, "lastSyncedAt"),
                "tokenExpiresAt": token_expiry,
                "files": files,
                "fileCount": files.len(),
                "source": s(entry, "source"),
            });
            if let Some(q) = enrich_quota_for_snapshot(&quota_cache["accounts"][id], now_ep) {
                item["quota"] = q;
            }
            if let Some(view) = usage_view {
                item["usage"] = view.usage.clone();
                if !view.quota_note.is_null() {
                    item["quotaNote"] = view.quota_note.clone();
                }
                if !view.quota_estimate.is_null() {
                    item["quotaEstimate"] = view.quota_estimate.clone();
                }
            }
            items.push(item);
        }
    }

    let dup_groups = find_duplicate_quota_slot_groups(&items);

    let live_drifted = !live.is_empty()
        && (active_id.is_none()
            || (!active_email.is_empty() && !active_email.eq_ignore_ascii_case(&live)));

    let running_json: Vec<Value> = running.iter().map(running_agy_to_json).collect();

    let quota_updated_at = s(&quota_cache, "updatedAt").to_string();
    let quota_updated_ep = quota_cache["updatedEpoch"].as_i64().unwrap_or(0);
    let quota_age_secs = if quota_updated_ep > 0 {
        Some((now_ep - quota_updated_ep).max(0))
    } else {
        None
    };

    let mut out = json!({
        "root": root.to_string_lossy(),
        "active": active_id,
        "liveEmail": live,
        "liveDrifted": live_drifted,
        "runningAgy": running_json,
        "accounts": items,
        "quotaUpdatedAt": quota_updated_at,
        "quotaAgeSeconds": quota_age_secs,
    });
    if !dup_groups.is_empty() {
        out["duplicateQuotaGroups"] = json!(dup_groups);
    }
    if let Some(snap) = usage_snap {
        out["unassignedUsage"] = snap.unassigned;
    }
    out
}

fn find_slot_by_email(state: &Value, email: &str) -> Option<String> {
    if email.is_empty() {
        return None;
    }
    let map = state["accounts"].as_object()?;
    for (id, entry) in map {
        if s(entry, "email").eq_ignore_ascii_case(email) {
            return Some(id.clone());
        }
    }
    None
}

fn parse_delegate_run_dir(p: &Path) -> (PathBuf, Option<String>, Option<String>) {
    let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let parent_is_review = p
        .parent()
        .and_then(|d| d.file_name())
        .and_then(|n| n.to_str())
        == Some("review");
    if parent_is_review {
        if let Some(run_dir) = p.parent().and_then(|d| d.parent()) {
            let round = (!fname.is_empty()).then(|| fname.to_string());
            return (run_dir.to_path_buf(), Some("reviewer".to_string()), round);
        }
    }
    (p.to_path_buf(), Some("author".to_string()), None)
}

fn running_agy_to_json(p: &RunningAgy) -> Value {
    let mut obj = json!({
        "pid": p.pid,
        "run": p.delegate_run,
        "cmdline": p.cmdline,
    });
    if let Some(ref name) = p.run_name {
        obj["runName"] = json!(name);
    }
    if let Some(ref role) = p.role {
        obj["role"] = json!(role);
    }
    if let Some(ref round) = p.round {
        obj["round"] = json!(round);
    }
    if let Some(ref acct) = p.account {
        obj["account"] = json!(acct);
    }
    obj
}

fn cred_file_valid_for_email(cf: &CredFile, bytes: &[u8], expected_email: &str) -> bool {
    if bytes.is_empty() {
        return false;
    }
    if cf.slot_name == "credential-user-scope" {
        return true;
    }
    let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
        return false;
    };
    if !v.is_object() {
        return false;
    }
    let expected = expected_email.trim();
    if expected.is_empty() {
        return true;
    }
    let file_email = match cf.slot_name {
        "google_accounts.json" => v.get("active").and_then(Value::as_str),
        "jetski-standalone-oauth-token"
        | "antigravity-cli-oauth-token"
        | "antigravity-oauth-token" => v.get("id_token_email").and_then(Value::as_str),
        _ => None,
    };
    match file_email.map(str::trim).filter(|s| !s.is_empty()) {
        Some(actual) => actual.eq_ignore_ascii_case(expected),
        None => true,
    }
}

fn extract_token_expiry_epoch(bytes: &[u8]) -> Option<i64> {
    let v: Value = serde_json::from_slice(bytes).ok()?;
    if let Some(exp) = v["token"]["expiry"].as_str().and_then(iso_epoch) {
        return Some(exp);
    }
    if let Some(ms) = v["expiry_date"].as_i64().filter(|&ms| ms > 0) {
        return Some(ms / 1000);
    }
    None
}

fn should_update_slot_file(existing_slot_path: &Path, new_bytes: &[u8]) -> bool {
    let Ok(existing_bytes) = fs::read(existing_slot_path) else {
        return true;
    };
    if existing_bytes == new_bytes {
        return false;
    }
    if let (Some(old_exp), Some(new_exp)) = (
        extract_token_expiry_epoch(&existing_bytes),
        extract_token_expiry_epoch(new_bytes),
    ) {
        if old_exp > new_exp {
            return false;
        }
    }
    true
}

/// Save back any present live credential files from `$HOME` into `slot_dir`.
/// Captures tokens refreshed by `agy` since the last switch.
fn save_live_to_slot(home_dir: &Path, dst_slot: &Path) -> Res<usize> {
    save_live_to_slot_checked(home_dir, dst_slot, "")
}

fn cred_file_carries_email(cf: &CredFile) -> bool {
    matches!(
        cf.slot_name,
        "google_accounts.json"
            | "jetski-standalone-oauth-token"
            | "antigravity-cli-oauth-token"
            | "antigravity-oauth-token"
    )
}

/// Save back live credential files from `$HOME` into `dst_slot`, validating JSON structure
/// and ensuring files carrying an email match `expected_email` (when non-empty) without
/// regressing a strictly newer token expiry already stored in the slot.
/// Email-less files (`oauth_creds.json`, `credential-user-scope`) are only copied when
/// every email-bearing credential file in `home_dir` agrees with `expected_email`.
fn save_live_to_slot_checked(
    home_dir: &Path,
    dst_slot: &Path,
    expected_email: &str,
) -> Res<usize> {
    ensure_dir_mode(dst_slot, 0o700)?;
    let expected = expected_email.trim();
    let home_emails = collect_home_cred_emails(home_dir);
    let home_consistent = expected.is_empty()
        || (!home_emails.is_empty()
            && home_emails
                .iter()
                .all(|(_, e)| e.eq_ignore_ascii_case(expected)));
    let mut copied = 0usize;
    for cf in CRED_FILES {
        if !cred_file_carries_email(cf) && !home_consistent {
            continue;
        }
        let src = home_dir.join(cf.home_rel);
        if src.is_file() {
            let bytes = fs::read(&src).map_err(|e| format!("read {}: {e}", src.display()))?;
            if bytes.is_empty() {
                continue;
            }
            if !cred_file_valid_for_email(cf, &bytes, expected) {
                continue;
            }
            let dst = dst_slot.join(cf.slot_name);
            if !should_update_slot_file(&dst, &bytes) {
                continue;
            }
            write_secret_0600(&dst, &bytes, true)?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// Atomically install credential files from `src_slot` into `$HOME`.
/// Removes any `CRED_FILES` in `$HOME` absent from `src_slot` so stale files
/// (like another account's `credential-user-scope`) never bleed across accounts.
fn install_slot_to_home(src_slot: &Path, home_dir: &Path) -> Res<usize> {
    let mut installed = 0usize;
    for cf in CRED_FILES {
        let src = src_slot.join(cf.slot_name);
        let dst = home_dir.join(cf.home_rel);
        if src.is_file() {
            let bytes = fs::read(&src).map_err(|e| format!("read {}: {e}", src.display()))?;
            write_secret_0600(&dst, &bytes, cf.private_parent)?;
            installed += 1;
        } else if dst.exists() {
            fs::remove_file(&dst).map_err(|e| format!("remove stale {}: {e}", dst.display()))?;
        }
    }
    Ok(installed)
}

fn format_running_proc_line(p: &RunningAgy) -> String {
    match (&p.delegate_run, &p.account) {
        (Some(run), Some(acct)) => {
            format!("  pid={} (delegate run: {}, account: {}) {}", p.pid, run, acct, p.cmdline)
        }
        (Some(run), None) => format!("  pid={} (delegate run: {}) {}", p.pid, run, p.cmdline),
        (None, Some(acct)) => format!("  pid={} (account: {}) {}", p.pid, acct, p.cmdline),
        (None, None) => format!("  pid={} {}", p.pid, p.cmdline),
    }
}

/// RAII temporary HOME directory cleaned up automatically on drop.
struct TempHome {
    path: PathBuf,
}

impl TempHome {
    fn create() -> Res<Self> {
        let base = env::var_os("XDG_RUNTIME_DIR")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(env::temp_dir);
        let seq = TEMP_HOME_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!(
            "delegate-agy-home-{}-{seq}-{:016x}",
            std::process::id(),
            now_ns() as u64
        ));
        fs::create_dir(&dir).map_err(|e| format!("create temp HOME {}: {e}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("chmod 0700 {}: {e}", dir.display()))?;
        Ok(Self { path: dir })
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn symlink_dir_entries_except(src_dir: &Path, dst_dir: &Path, skip: &[&str]) -> Res<()> {
    let Ok(entries) = fs::read_dir(src_dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if let Some(s) = name.to_str() {
            if skip.contains(&s) {
                continue;
            }
        }
        let dst = dst_dir.join(&name);
        if fs::symlink_metadata(&dst).is_ok() {
            continue;
        }
        let _ = std::os::unix::fs::symlink(entry.path(), &dst);
    }
    Ok(())
}

/// Recorded fallback when a session's pinned `agy` slot became unavailable and
/// switched to the currently active slot (never picks a third account).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgyAccountSwitch {
    pub from: String,
    pub to: String,
    pub reason: String,
}

/// Result of resolving an `agy` account slot and preparing its credential overlay.
pub struct PreparedAgyAccount {
    pub account_id: Option<String>,
    pub overlay: Option<AgyCredOverlay>,
    pub switched: Option<AgyAccountSwitch>,
}

/// Format a human-readable one-line notice for `status`, `card`, and TUI when a run
/// fell back from its pinned `agy` slot to the active slot.
pub fn format_account_switch_line(sw: &Value, is_reviewer: bool) -> Option<String> {
    if !sw.is_object() {
        return None;
    }
    let from = s(sw, "from");
    let to = s(sw, "to");
    let reason = s(sw, "reason");
    if from.is_empty() || to.is_empty() {
        return None;
    }
    let detail = match (is_reviewer, reason.is_empty()) {
        (true, true) => "审查员".to_string(),
        (true, false) => format!("审查员：{reason}"),
        (false, true) => "原槽位不可用".to_string(),
        (false, false) => reason.to_string(),
    };
    Some(format!("本 run 已从 {from} 换到 {to}（{detail}）"))
}

fn slot_model_pool_matches(model: &str, pool: &str) -> bool {
    let pool_low = pool.to_ascii_lowercase();
    let model_low = model.trim().to_ascii_lowercase();
    if model_low.is_empty() {
        return true;
    }
    if model_low.contains("gemini") {
        pool_low.contains("gemini")
    } else if model_low.contains("claude")
        || model_low.contains("gpt")
        || model_low.contains("opus")
        || model_low.contains("sonnet")
    {
        pool_low.contains("claude") || pool_low.contains("gpt")
    } else {
        // Conservative: if the model does not map clearly to a known quota pool,
        // do not guess that it is exhausted.
        false
    }
}

/// Check whether a pinned `agy` slot is definitely unavailable.
/// Returns `Some(reason)` only when the slot is deleted, its credentials/identity are
/// invalid, or cached real quota shows 0% remaining for an un-reset window of `model`.
/// When uncertain (missing/stale/errored quota cache), returns `None` so the session
/// stays on the pinned slot.
pub(crate) fn check_pinned_slot_unavailability(
    root: &Path,
    state: &Value,
    quota_cache: &Value,
    pinned_id: &str,
    model: &str,
    now_ep: i64,
) -> Option<String> {
    if state["accounts"].get(pinned_id).is_none() {
        return Some("原槽位已删除".to_string());
    }
    let sdir = slot_dir(root, pinned_id);
    if !sdir.is_dir() {
        return Some("原槽位已删除".to_string());
    }
    if !slot_has_core_token(&sdir) {
        return Some("凭据失效".to_string());
    }
    let expected_email = s(&state["accounts"][pinned_id], "email").trim().to_string();
    let mut valid_core = 0usize;
    for cf in CRED_FILES {
        let p = sdir.join(cf.slot_name);
        let Ok(bytes) = fs::read(&p) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        if !expected_email.is_empty() {
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                let file_email = match cf.slot_name {
                    "google_accounts.json" => v.get("active").and_then(Value::as_str),
                    "jetski-standalone-oauth-token"
                    | "antigravity-cli-oauth-token"
                    | "antigravity-oauth-token" => v.get("id_token_email").and_then(Value::as_str),
                    _ => None,
                };
                if let Some(actual) = file_email.map(str::trim).filter(|s| !s.is_empty()) {
                    if !actual.eq_ignore_ascii_case(&expected_email) {
                        return Some("身份校验失败".to_string());
                    }
                }
            }
        }
        if cf.is_core_token && cred_file_valid_for_email(cf, &bytes, &expected_email) {
            valid_core += 1;
        }
    }
    if valid_core == 0 {
        return Some("凭据失效".to_string());
    }

    let acct_quota = &quota_cache["accounts"][pinned_id];
    if acct_quota.is_object() {
        if b(acct_quota, "identityMismatch") {
            return Some("身份校验失败".to_string());
        }
        if b(acct_quota, "ok") {
            if let Some(entries) = acct_quota["entries"].as_array() {
                for entry in entries {
                    let pool = s(entry, "pool");
                    if !slot_model_pool_matches(model, pool) {
                        continue;
                    }
                    let Some(pct) = entry["remainingPct"].as_f64() else {
                        continue;
                    };
                    if pct.is_finite() && pct <= 0.0 {
                        let reset_at = s(entry, "resetAt");
                        let not_yet_reset = if reset_at.is_empty() {
                            true
                        } else {
                            iso_epoch(reset_at).is_some_and(|rep| rep > now_ep)
                        };
                        if not_yet_reset {
                            return Some("额度已耗尽".to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

/// Private credential overlay HOME for a single delegate-launched `agy` process.
///
/// Copies `CRED_FILES` from the selected account slot (resolved under `.lock` before spawn)
/// into a `0700` temporary HOME while symlinking all non-credential entries of `$HOME`,
/// `$HOME/.gemini`, `$HOME/.gemini/antigravity-cli`, `$HOME/.gemini/antigravity`, and
/// `$XDG_CONFIG_HOME` back to the real directories so sessions, settings, skills, caches,
/// and child toolchains (`.cargo`, `.gitconfig`, `.local`, etc.) work transparently.
///
/// On exit (`finish()`), any refreshed `CRED_FILES` are validated and atomically saved
/// back (`0600`) under `.lock` to **this process's own account slot**. Live `~/.gemini`
/// is only mirrored if this account is still active and undrifted; if the user switched
/// accounts while the process ran (or this session is pinned to a non-active slot),
/// live `~/.gemini` is never touched.
pub struct AgyCredOverlay {
    temp_home: TempHome,
    real_home: PathBuf,
    root: PathBuf,
    account_id: String,
    account_email: String,
    before: BTreeMap<&'static str, Option<Vec<u8>>>,
    finished: bool,
}

impl AgyCredOverlay {
    /// Prepare a private credential overlay for the currently active `agy` account slot.
    ///
    /// Returns `(resolved_account_id, Some(overlay))` when an active slot with core
    /// credentials exists. Returns `(None, None)` when no local `agy-accounts` store is
    /// configured (preserving direct-HOME behavior for single-account setups and tests).
    pub fn prepare_active() -> Res<(Option<String>, Option<Self>)> {
        let prep = Self::prepare_for_session(None, "")?;
        Ok((prep.account_id, prep.overlay))
    }

    /// Prepare a private credential overlay for a session, preferring `pinned_id` if set.
    ///
    /// Falls back to the currently active slot ONLY when `pinned_id` is unavailable
    /// (deleted, invalid credentials / identity mismatch, or cached real quota exhausted).
    /// Never chooses a third account.
    pub fn prepare_for_session(
        pinned_id: Option<&str>,
        model: &str,
    ) -> Res<PreparedAgyAccount> {
        let root = accounts_root();
        if !accounts_meta_path(&root).is_file() {
            return Ok(PreparedAgyAccount {
                account_id: None,
                overlay: None,
                switched: None,
            });
        }
        let real_home = home();
        let real_xdg_config = env::var_os("XDG_CONFIG_HOME")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| real_home.join(".config"));
        let quota_cache = load_quota_cache();
        Self::prepare_for_session_in(
            &root,
            &real_home,
            &real_xdg_config,
            pinned_id,
            model,
            &quota_cache,
            epoch() as i64,
        )
    }

    #[cfg(test)]
    fn prepare_active_for(
        root: &Path,
        real_home: &Path,
        real_xdg_config: &Path,
    ) -> Res<(Option<String>, Option<Self>)> {
        let empty_quota = json!({"accounts": {}});
        let prep = Self::prepare_for_session_in(
            root,
            real_home,
            real_xdg_config,
            None,
            "",
            &empty_quota,
            epoch() as i64,
        )?;
        Ok((prep.account_id, prep.overlay))
    }

    fn prepare_for_session_in(
        root: &Path,
        real_home: &Path,
        real_xdg_config: &Path,
        pinned_id: Option<&str>,
        model: &str,
        quota_cache: &Value,
        now_ep: i64,
    ) -> Res<PreparedAgyAccount> {
        let _lock = acquire_accounts_lock(root)?;
        let mut state = load_state(root);
        let active_id = state["active"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let pinned = pinned_id.map(str::trim).filter(|s| !s.is_empty());
        let (selected_id, switched) = if let Some(pin) = pinned {
            if let Some(reason) =
                check_pinned_slot_unavailability(root, &state, quota_cache, pin, model, now_ep)
            {
                let can_fallback_to_active = active_id.as_deref().is_some_and(|act| {
                    act != pin
                        && state["accounts"].get(act).is_some()
                        && slot_has_core_token(&slot_dir(root, act))
                });
                if can_fallback_to_active {
                    let act = active_id.clone().unwrap();
                    (
                        Some(act.clone()),
                        Some(AgyAccountSwitch {
                            from: pin.to_string(),
                            to: act,
                            reason,
                        }),
                    )
                } else if state["accounts"].get(pin).is_some()
                    && slot_has_core_token(&slot_dir(root, pin))
                {
                    // No distinct active account to fall back to; stay on pinned slot
                    // (never auto-select a third account).
                    (Some(pin.to_string()), None)
                } else {
                    (None, None)
                }
            } else {
                (Some(pin.to_string()), None)
            }
        } else {
            (active_id.clone(), None)
        };

        let Some(selected_id) = selected_id else {
            return Ok(PreparedAgyAccount {
                account_id: None,
                overlay: None,
                switched: None,
            });
        };
        let sdir = slot_dir(root, &selected_id);
        if !slot_has_core_token(&sdir) {
            return Ok(PreparedAgyAccount {
                account_id: Some(selected_id),
                overlay: None,
                switched,
            });
        }
        let account_email = {
            let stored = s(&state["accounts"][&selected_id], "email");
            if !stored.is_empty() {
                stored.to_string()
            } else {
                slot_email(&sdir).unwrap_or_default()
            }
        };

        // Only sync fresher refreshed tokens from live ~/.gemini when the selected slot
        // IS the currently active slot (never cross-sync live creds into a non-active pinned slot).
        let is_selected_active = active_id.as_deref() == Some(selected_id.as_str());
        if is_selected_active {
            let live = live_email(real_home).unwrap_or_default();
            let live_matches = live.is_empty()
                || account_email.is_empty()
                || live.eq_ignore_ascii_case(&account_email);
            if live_matches {
                let mut synced = 0usize;
                for cf in CRED_FILES {
                    let live_path = real_home.join(cf.home_rel);
                    let slot_path = sdir.join(cf.slot_name);
                    let Ok(live_meta) = fs::metadata(&live_path) else {
                        continue;
                    };
                    if !live_meta.is_file() || live_meta.len() == 0 {
                        continue;
                    }
                    let live_newer = match (
                        live_meta.modified().ok(),
                        fs::metadata(&slot_path).and_then(|m| m.modified()).ok(),
                    ) {
                        (Some(lm), Some(sm)) => lm > sm,
                        (Some(_), None) => true,
                        _ => false,
                    };
                    if !live_newer {
                        continue;
                    }
                    let Ok(bytes) = fs::read(&live_path) else {
                        continue;
                    };
                    if cred_file_valid_for_email(cf, &bytes, &account_email)
                        && should_update_slot_file(&slot_path, &bytes)
                        && write_secret_0600(&slot_path, &bytes, true).is_ok()
                    {
                        synced += 1;
                    }
                }
                if synced > 0 {
                    if let Some(obj) = state["accounts"][&selected_id].as_object_mut() {
                        obj.insert("lastSyncedAt".into(), json!(iso()));
                    }
                    let _ = save_state(root, &state);
                }
            }
        }

        let temp_home = TempHome::create()?;
        let ov = &temp_home.path;

        // 1. Top-level HOME entries (everything except .gemini and .config).
        symlink_dir_entries_except(real_home, ov, &[".gemini", ".config"])?;

        // 2. .config overlay (isolate only .config/antigravity/credential-user-scope).
        let ov_cfg = ov.join(".config");
        let ov_cfg_ag = ov_cfg.join("antigravity");
        fs::create_dir_all(&ov_cfg_ag)
            .map_err(|e| format!("create {}: {e}", ov_cfg_ag.display()))?;
        let _ = fs::set_permissions(&ov_cfg_ag, fs::Permissions::from_mode(0o700));
        symlink_dir_entries_except(real_xdg_config, &ov_cfg, &["antigravity"])?;
        symlink_dir_entries_except(
            &real_xdg_config.join("antigravity"),
            &ov_cfg_ag,
            &["credential-user-scope"],
        )?;

        // 3. .gemini overlay (isolate only the 5 credential files; share sessions/settings/logs).
        let real_gem = real_home.join(".gemini");
        let real_cli = real_gem.join("antigravity-cli");
        let real_ag = real_gem.join("antigravity");
        // Ensure persistent conversation/brain directories exist in real HOME so newly created
        // conversations survive overlay cleanup and can be resumed by later `reply` runs.
        for sub in ["conversations", "brain"] {
            let _ = fs::create_dir_all(real_cli.join(sub));
        }
        let ov_gem = ov.join(".gemini");
        let ov_cli = ov_gem.join("antigravity-cli");
        let ov_ag = ov_gem.join("antigravity");
        fs::create_dir_all(&ov_cli).map_err(|e| format!("create {}: {e}", ov_cli.display()))?;
        fs::create_dir_all(&ov_ag).map_err(|e| format!("create {}: {e}", ov_ag.display()))?;

        symlink_dir_entries_except(
            &real_gem,
            &ov_gem,
            &[
                "oauth_creds.json",
                "google_accounts.json",
                "jetski-standalone-oauth-token",
                "antigravity-cli",
                "antigravity",
            ],
        )?;
        symlink_dir_entries_except(&real_cli, &ov_cli, &["antigravity-oauth-token"])?;
        symlink_dir_entries_except(&real_ag, &ov_ag, &["antigravity-oauth-token"])?;

        // 4. Copy private 0600 credentials from the selected slot into the overlay.
        install_slot_to_home(&sdir, ov)?;

        // 5. Snapshot pre-run credential bytes to detect refreshes on exit.
        let mut before = BTreeMap::new();
        for cf in CRED_FILES {
            let bytes = fs::read(ov.join(cf.home_rel)).ok();
            before.insert(cf.slot_name, bytes);
        }

        let id_copy = selected_id.clone();
        Ok(PreparedAgyAccount {
            account_id: Some(id_copy),
            overlay: Some(Self {
                temp_home,
                real_home: real_home.to_path_buf(),
                root: root.to_path_buf(),
                account_id: selected_id,
                account_email,
                before,
                finished: false,
            }),
            switched,
        })
    }

    #[cfg(test)]
    pub fn overlay_home(&self) -> &Path {
        &self.temp_home.path
    }

    pub fn apply_env(&self, cmd: &mut Command) {
        self.apply_env_with(cmd, |k| env::var_os(k));
    }

    fn apply_env_with<F>(&self, cmd: &mut Command, get_env: F)
    where
        F: Fn(&str) -> Option<std::ffi::OsString>,
    {
        cmd.env("HOME", &self.temp_home.path)
            .env("XDG_CONFIG_HOME", self.temp_home.path.join(".config"))
            .env("DELEGATE_AGY_REAL_HOME", &self.real_home)
            .env("DELEGATE_AGY_ACCOUNT", &self.account_id);
        // Keep non-credential XDG paths and Rust toolchain homes anchored to real_home
        // so long-lived daemons (e.g. sccache) spawned by child runs never cache paths
        // under the ephemeral overlay HOME that TempHome::drop removes on exit.
        for (key, rel) in [
            ("XDG_STATE_HOME", ".local/state"),
            ("XDG_CACHE_HOME", ".cache"),
            ("XDG_DATA_HOME", ".local/share"),
            ("RUSTUP_HOME", ".rustup"),
            ("CARGO_HOME", ".cargo"),
        ] {
            if get_env(key).is_none_or(|v| v.is_empty()) {
                cmd.env(key, self.real_home.join(rel));
            }
        }
    }

    /// Save back any credential files refreshed during the child's lifetime into its own
    /// account slot under `.lock`. Safe to call multiple times (idempotent).
    pub fn finish(&mut self) -> usize {
        if self.finished {
            return 0;
        }
        self.finished = true;

        let mut refreshed: Vec<(&CredFile, Vec<u8>)> = Vec::new();
        for cf in CRED_FILES {
            let p = self.temp_home.path.join(cf.home_rel);
            let Ok(after_bytes) = fs::read(&p) else {
                continue;
            };
            if after_bytes.is_empty() {
                continue;
            }
            if self.before.get(cf.slot_name).and_then(Option::as_ref) == Some(&after_bytes) {
                continue;
            }
            if !cred_file_valid_for_email(cf, &after_bytes, &self.account_email) {
                continue;
            }
            refreshed.push((cf, after_bytes));
        }
        if refreshed.is_empty() {
            return 0;
        }

        let Ok(_lock) = acquire_accounts_lock(&self.root) else {
            return 0;
        };
        let mut state = load_state(&self.root);
        if state["accounts"].get(&self.account_id).is_none() {
            return 0;
        }
        let sdir = slot_dir(&self.root, &self.account_id);
        let _ = ensure_dir_mode(&sdir, 0o700);
        let mut saved = 0usize;
        let mut saved_files: Vec<(&CredFile, &[u8])> = Vec::new();
        for (cf, bytes) in &refreshed {
            let dst = sdir.join(cf.slot_name);
            if !should_update_slot_file(&dst, bytes) {
                continue;
            }
            if write_secret_0600(&dst, bytes, true).is_ok() {
                saved += 1;
                saved_files.push((cf, bytes.as_slice()));
            }
        }
        if saved > 0 {
            if let Some(obj) = state["accounts"][&self.account_id].as_object_mut() {
                obj.insert("lastSyncedAt".into(), json!(iso()));
            }
            let _ = save_state(&self.root, &state);

            // Mirror back to live ~/.gemini ONLY if this account is still active and live
            // credentials have not drifted to another email.
            let still_active = state["active"].as_str() == Some(self.account_id.as_str());
            if still_active {
                let live = live_email(&self.real_home).unwrap_or_default();
                let live_matches = live.is_empty()
                    || self.account_email.is_empty()
                    || live.eq_ignore_ascii_case(&self.account_email);
                if live_matches {
                    for (cf, bytes) in saved_files {
                        let _ = write_secret_0600(
                            &self.real_home.join(cf.home_rel),
                            bytes,
                            cf.private_parent,
                        );
                    }
                }
            }
        }
        saved
    }
}

fn is_executable_file(p: &Path) -> bool {
    p.is_file()
        && fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

fn resolve_agy_bin(explicit: Option<&str>) -> Res<PathBuf> {
    if let Some(p) = explicit.filter(|s| !s.trim().is_empty()) {
        let pb = PathBuf::from(p);
        if is_executable_file(&pb) {
            return Ok(pb);
        }
        return Err(format!("--agy-bin is not executable: {}", pb.display()));
    }
    if let Ok(env_bin) = env::var("DELEGATE_AGY_BIN") {
        let trimmed = env_bin.trim();
        if !trimmed.is_empty() {
            let pb = PathBuf::from(trimmed);
            if is_executable_file(&pb) {
                return Ok(pb);
            }
            return Err(format!("DELEGATE_AGY_BIN is not executable: {}", pb.display()));
        }
    }
    // Prefer real binary under ~/.local/libexec/agy when present to bypass any legacy wrapper.
    let libexec = home().join(".local/libexec/agy");
    if is_executable_file(&libexec) {
        return Ok(libexec);
    }
    if let Some(paths) = env::var_os("PATH") {
        for dir in env::split_paths(&paths) {
            let candidate = dir.join("agy");
            if is_executable_file(&candidate) {
                return Ok(candidate);
            }
        }
    }
    Err("could not locate executable `agy` (checked ~/.local/libexec/agy and PATH; override with --agy-bin)".into())
}

fn print_duplicate_quota_warnings(snap: &Value) {
    if let Some(groups) = snap["duplicateQuotaGroups"].as_array() {
        for grp in groups {
            if let Some(ids) = grp.as_array() {
                let names: Vec<&str> = ids.iter().filter_map(Value::as_str).collect();
                if names.len() >= 2 {
                    println!(
                        "warning: 槽位 {} 额度条目（含 resetAt）完全相同，疑似串号，请核对槽位凭证",
                        names.join(", ")
                    );
                }
            }
        }
    }
}

fn format_quota_brief(quota: &Value) -> String {
    if !quota.is_object() {
        return String::new();
    }
    let age = quota["ageSeconds"]
        .as_i64()
        .map(format_age_compact)
        .unwrap_or_default();
    if b(quota, "identityMismatch") {
        return if age.is_empty() {
            format!(" quota[{IDENTITY_MISMATCH_MSG}]")
        } else {
            format!(" quota[{IDENTITY_MISMATCH_MSG} · {age}]")
        };
    }
    if !b(quota, "ok") {
        let err = s(quota, "error");
        let msg = if err.is_empty() { "失败" } else { err };
        return format!(" quota[err: {msg}, {age}]");
    }
    let entries = quota["entries"].as_array().cloned().unwrap_or_default();
    if entries.is_empty() {
        let unk = quota["unknownLines"].as_array().map_or(0, Vec::len);
        if unk > 0 {
            return format!(" quota[raw:{unk}行, {age}]");
        }
        return String::new();
    }
    let mut parts = Vec::new();
    for e in &entries {
        let pool = s(e, "pool");
        let short_pool = pool.split_whitespace().next().unwrap_or(pool);
        let win = match s(e, "window") {
            "weekly" => "wk",
            "fiveHour" => "5h",
            _ => s(e, "windowLabel"),
        };
        let pct = e["remainingPct"].as_f64().unwrap_or(0.0);
        let reset = e["resetInSeconds"]
            .as_i64()
            .map(|r| format!("({})", format_countdown_compact(r)))
            .unwrap_or_default();
        parts.push(format!("{short_pool}:{win}={pct:.0}%{reset}"));
    }
    format!(" quota[{} · {age}]", parts.join(", "))
}

fn cmd_list(args: &[String]) -> Res<i32> {
    let usage = "delegate agy account list [--quota] [--agy-bin PATH] [--json]";
    let (pos, flags, kv) = parse_simple(usage, args, &["--json", "--quota"], &["--agy-bin"])?;
    if !pos.is_empty() {
        return Err(unrecognized_arg(&pos.join(" "), &["--json", "--quota", "--agy-bin"], usage));
    }
    if has(&flags, "--quota") {
        refresh_all_quotas(value(&kv, "--agy-bin"))?;
    }
    let snap = snapshot_json();
    if has(&flags, "--json") {
        println!("{snap}");
        return Ok(0);
    }
    let accounts = snap["accounts"].as_array().cloned().unwrap_or_default();
    let running = snap["runningAgy"].as_array().map_or(0, Vec::len);
    let active = snap["active"].as_str().unwrap_or("");
    if accounts.is_empty() {
        println!("No agy accounts configured ({}).", s(&snap, "root"));
        println!("Next: delegate agy account add [<id>] | delegate agy account import-agy-swap --dry-run");
        return Ok(0);
    }
    let age_hdr = snap["quotaAgeSeconds"]
        .as_i64()
        .map(|a| format!(", quota age: {}", format_age_compact(a)))
        .unwrap_or_default();
    println!(
        "agy accounts ({}, active: {}, running agy: {}{age_hdr}):",
        accounts.len(),
        if active.is_empty() { "none" } else { active },
        running
    );
    for item in &accounts {
        let mark = if b(item, "active") { "*" } else { " " };
        let id = s(item, "id");
        let email = s(item, "email");
        let last_used = s(item, "lastUsedAt");
        let last_str = if last_used.is_empty() { "-" } else { last_used };
        let files = item["fileCount"].as_u64().unwrap_or(0);
        let label = s(item, "label");
        let label_part = if label.is_empty() {
            String::new()
        } else {
            format!(" [{label}]")
        };
        let t5 = n(&item["usage"]["fiveHour"], "total");
        let t7 = n(&item["usage"]["sevenDay"], "total");
        let unc = item["usage"]["fiveHour"]["uncountedRunning"].as_u64().unwrap_or(0);
        let unc_mark = if unc > 0 { "+" } else { "" };
        let usage_part = format!(
            " 5h={}{unc_mark} 7d={}",
            crate::usage::format_token_compact(t5),
            crate::usage::format_token_compact(t7),
        );
        let quota_part = if item["quota"].is_object() {
            format_quota_brief(&item["quota"])
        } else {
            let mut note_bits = Vec::new();
            if let Some(w) = item["quotaNote"]["weekly"].as_f64() {
                note_bits.push(format!("wk={w:.2}%"));
            }
            if let Some(fh) = item["quotaNote"]["fiveHour"].as_f64() {
                note_bits.push(format!("5h={fh:.2}%"));
            }
            if note_bits.is_empty() {
                String::new()
            } else {
                format!(" rem({})", note_bits.join(","))
            }
        };
        println!("  {mark} {id:<16} {email:<30} last_used={last_str} files={files}{usage_part}{quota_part}{label_part}");
    }
    if b(&snap, "liveDrifted") {
        println!(
            "note: live ~/.gemini account ({}) differs from active slot ({})",
            s(&snap, "liveEmail"),
            if active.is_empty() { "none" } else { active }
        );
    }
    print_duplicate_quota_warnings(&snap);
    println!("Next: delegate agy account use <id> | delegate agy account status --quota");
    Ok(0)
}

fn cmd_status(args: &[String]) -> Res<i32> {
    let usage = "delegate agy account status [--quota] [--agy-bin PATH] [--json]";
    let (pos, flags, kv) = parse_simple(usage, args, &["--json", "--quota"], &["--agy-bin"])?;
    if !pos.is_empty() {
        return Err(unrecognized_arg(&pos.join(" "), &["--json", "--quota", "--agy-bin"], usage));
    }
    if has(&flags, "--quota") {
        refresh_all_quotas(value(&kv, "--agy-bin"))?;
    }
    let snap = snapshot_json();
    if has(&flags, "--json") {
        println!("{snap}");
        return Ok(0);
    }
    let active = snap["active"].as_str().unwrap_or("");
    let live = s(&snap, "liveEmail");
    let accounts = snap["accounts"].as_array().cloned().unwrap_or_default();
    let running = snap["runningAgy"].as_array().cloned().unwrap_or_default();
    println!(
        "active: {}",
        if active.is_empty() { "(none)" } else { active }
    );
    if !live.is_empty() {
        println!("live ~/.gemini email: {live}");
    }
    if b(&snap, "liveDrifted") {
        println!("warning: live credentials differ from active slot");
    }
    print_duplicate_quota_warnings(&snap);
    if let Some(cur) = accounts.iter().find(|a| b(a, "active")) {
        let last_used = s(cur, "lastUsedAt");
        if !last_used.is_empty() {
            println!("last used: {last_used}");
        }
        let synced = s(cur, "lastSyncedAt");
        if !synced.is_empty() {
            println!("last synced: {synced}");
        }
        let exp = s(cur, "tokenExpiresAt");
        if !exp.is_empty() {
            println!("local token expiry: {exp}");
        }
        println!("stored files: {}", cur["fileCount"].as_u64().unwrap_or(0));
        let t1 = n(&cur["usage"]["oneHour"], "total");
        let t5 = n(&cur["usage"]["fiveHour"], "total");
        let t7 = n(&cur["usage"]["sevenDay"], "total");
        println!(
            "local usage: 1h={}  5h={}  7d={}",
            crate::usage::format_token_compact(t1),
            crate::usage::format_token_compact(t5),
            crate::usage::format_token_compact(t7),
        );
        if cur["quota"].is_object() {
            let q = &cur["quota"];
            let age = q["ageSeconds"]
                .as_i64()
                .map(format_age_compact)
                .unwrap_or_else(|| "-".to_string());
            let fetched = s(q, "fetchedAt");
            let mismatch = b(q, "identityMismatch");
            if mismatch {
                let exp = s(q, "expectedEmail");
                let obs = s(q, "observedEmail");
                let detail = match (!exp.is_empty(), !obs.is_empty()) {
                    (true, true) => format!("（期望 {exp}，实际 {obs}）"),
                    (true, false) => format!("（期望 {exp}，实际身份未知）"),
                    _ => String::new(),
                };
                println!("real quota (agy -p /usage, {IDENTITY_MISMATCH_MSG}):");
                println!("  warning: {IDENTITY_MISMATCH_MSG}{detail}");
                if let Some(entries) = q["entries"].as_array().filter(|a| !a.is_empty()) {
                    println!("  cached prior quota ({age}, fetched {fetched}):");
                    for e in entries {
                        let pool = s(e, "pool");
                        let wlabel = s(e, "windowLabel");
                        let pct = e["remainingPct"].as_f64().unwrap_or(0.0);
                        let reset_part = match (e["resetInSeconds"].as_i64(), s(e, "resetAt")) {
                            (Some(rem), at) if !at.is_empty() => {
                                format!(" · reset in {} ({at})", format_countdown_compact(rem))
                            }
                            (_, at) if !at.is_empty() => format!(" · reset {at}"),
                            _ => String::new(),
                        };
                        println!("  - {pool} | {wlabel}: {pct:.0}%{reset_part}");
                    }
                }
            } else {
                println!("real quota (agy -p /usage, {age}, fetched {fetched}):");
                if !b(q, "ok") {
                    println!("  error: {}", s(q, "error"));
                }
                if let Some(entries) = q["entries"].as_array() {
                    for e in entries {
                        let pool = s(e, "pool");
                        let wlabel = s(e, "windowLabel");
                        let pct = e["remainingPct"].as_f64().unwrap_or(0.0);
                        let reset_part = match (e["resetInSeconds"].as_i64(), s(e, "resetAt")) {
                            (Some(rem), at) if !at.is_empty() => {
                                format!(" · reset in {} ({at})", format_countdown_compact(rem))
                            }
                            (_, at) if !at.is_empty() => format!(" · reset {at}"),
                            _ => String::new(),
                        };
                        println!("  - {pool} | {wlabel}: {pct:.0}%{reset_part}");
                    }
                }
                if let Some(unk) = q["unknownLines"].as_array() {
                    for line in unk.iter().filter_map(Value::as_str) {
                        println!("  [raw] {line}");
                    }
                }
            }
        }
        if cur["quotaEstimate"].is_object() {
            for (label, key) in [("weekly", "weekly"), ("five-hour", "fiveHour")] {
                let est = &cur["quotaEstimate"][key];
                if est.is_object() {
                    let tpp = crate::usage::format_token_compact(n(est, "tokensPerPct"));
                    let rem_h = est["estimatedRemainingHours"]
                        .as_f64()
                        .map(crate::usage::format_hours_compact)
                        .unwrap_or_else(|| "-".to_string());
                    let src = s(est, "source");
                    let bsrc = s(est, "burnRateSource");
                    println!(
                        "  {label} estimate ({src}, pace {bsrc}): ~{tpp} tok / 1% · est remaining: {rem_h} (advisory only)"
                    );
                }
            }
        }
        if cur["quotaNote"].is_object() {
            let mut parts = Vec::new();
            if let Some(w) = cur["quotaNote"]["weekly"].as_f64() {
                parts.push(format!("weekly={w:.2}%"));
            }
            if let Some(fh) = cur["quotaNote"]["fiveHour"].as_f64() {
                parts.push(format!("five-hour={fh:.2}%"));
            }
            if !parts.is_empty() {
                let prio = if cur["quota"].is_object() {
                    "fallback; real quota takes priority"
                } else {
                    "advisory only"
                };
                println!("quota note ({prio}): {}", parts.join(", "));
            }
        }
    }
    println!("total slots: {}", accounts.len());
    println!("running agy processes: {}", running.len());
    for p in &running {
        let pid = p["pid"].as_u64().unwrap_or(0);
        let cmd = s(p, "cmdline");
        if let Some(run) = p["run"].as_str().filter(|r| !r.is_empty()) {
            println!("  pid={pid} (delegate run: {run}) {cmd}");
        } else {
            println!("  pid={pid} {cmd}");
        }
    }
    println!("store: {}", s(&snap, "root"));
    println!("Next: delegate agy account list --quota | delegate agy account use <id>");
    Ok(0)
}

fn cmd_add(args: &[String]) -> Res<i32> {
    let mut main_args = Vec::new();
    let mut passthrough_args = Vec::new();
    let mut seen_sep = false;
    for a in args {
        if !seen_sep && a == "--" {
            seen_sep = true;
            continue;
        }
        if seen_sep {
            passthrough_args.push(a.clone());
        } else {
            main_args.push(a.clone());
        }
    }
    let (pos, flags, kv) = parse_simple(
        "delegate agy account add [<id>] [--label TEXT] [--overwrite] [--agy-bin PATH] [--json]",
        &main_args,
        &["--overwrite", "--json"],
        &["--id", "--label", "--agy-bin", "--login-arg"],
    )?;
    if pos.len() > 1 {
        return Err("add accepts at most one positional <id>".into());
    }
    let explicit_id = value(&kv, "--id")
        .or_else(|| pos.first().map(String::as_str))
        .map(str::to_string);
    if let Some(ref id) = explicit_id {
        validate_id(id)?;
    }
    let label = value(&kv, "--label").unwrap_or("").to_string();
    let overwrite = has(&flags, "--overwrite");
    let json_out = has(&flags, "--json");
    let agy_bin = resolve_agy_bin(value(&kv, "--agy-bin"))?;

    let mut login_args: Vec<String> = kv
        .iter()
        .filter(|(k, _)| k == "--login-arg")
        .map(|(_, v)| v.clone())
        .collect();
    login_args.extend(passthrough_args);

    let real_home = home();
    let root = accounts_root();

    // Early check before running interactive login if explicit id already exists and !overwrite.
    if let Some(ref id) = explicit_id {
        let state = load_state(&root);
        if state["accounts"].get(id).is_some() && !overwrite {
            return Err(format!(
                "account slot {id:?} already exists (pass --overwrite to replace)"
            ));
        }
    }

    let temp_home = TempHome::create()?;
    for marker in KEYRING_MARKERS {
        let src = real_home.join(marker);
        if src.is_file() {
            let dst = temp_home.path.join(marker);
            if let Some(parent) = dst.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::copy(&src, &dst);
        }
    }

    if !json_out {
        eprintln!(
            "delegate agy account add: launching {} in isolated temporary HOME (current active account is untouched).",
            agy_bin.display()
        );
        eprintln!("Complete agy sign-in, then exit agy to save credentials into the new slot.");
    }

    let mut cmd = Command::new(&agy_bin);
    cmd.args(&login_args)
        .env("HOME", &temp_home.path)
        .env("XDG_CONFIG_HOME", temp_home.path.join(".config"))
        .env("XDG_CACHE_HOME", temp_home.path.join(".cache"))
        .env("XDG_STATE_HOME", temp_home.path.join(".local/state"))
        .env("AGY_SWAP_SKIP_SYNC", "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    preserve_real_toolchain_envs(&mut cmd, &real_home);
    let status = cmd
        .status()
        .map_err(|e| format!("failed to run {}: {e}", agy_bin.display()))?;

    let has_core = CRED_FILES.iter().any(|cf| {
        cf.is_core_token
            && fs::metadata(temp_home.path.join(cf.home_rel))
                .is_ok_and(|m| m.is_file() && m.len() > 0)
    });
    if !has_core {
        return Err(format!(
            "agy exited ({}) without writing OAuth credential files in temporary HOME; no account saved",
            status
        ));
    }

    let captured_email = live_email(&temp_home.path).unwrap_or_default();

    let _lock = acquire_accounts_lock(&root)?;
    let mut state = load_state(&root);
    let existing_map = state["accounts"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let id = match explicit_id {
        Some(id) => {
            if existing_map.contains_key(&id) && !overwrite {
                return Err(format!(
                    "account slot {id:?} already exists (pass --overwrite to replace)"
                ));
            }
            id
        }
        None => derive_id(&captured_email, &existing_map),
    };
    validate_id(&id)?;

    let dst_slot = slot_dir(&root, &id);
    if overwrite && dst_slot.exists() {
        let _ = fs::remove_dir_all(&dst_slot);
    }
    let copied = save_live_to_slot(&temp_home.path, &dst_slot)?;
    if copied == 0 || !slot_has_core_token(&dst_slot) {
        let _ = fs::remove_dir_all(&dst_slot);
        return Err("no valid agy credential files were captured".into());
    }

    let now = iso();
    if !state["accounts"].is_object() {
        state["accounts"] = json!({});
    }
    state["schemaVersion"] = json!(SCHEMA_VERSION);
    state["accounts"][&id] = json!({
        "id": id,
        "email": captured_email,
        "label": label,
        "addedAt": now,
        "lastUsedAt": "",
        "lastSyncedAt": now,
        "source": "login",
    });
    save_state(&root, &state)?;

    let files = slot_present_files(&dst_slot);
    if json_out {
        println!(
            "{}",
            json!({
                "ok": true,
                "id": id,
                "email": captured_email,
                "label": label,
                "files": files,
                "fileCount": files.len(),
                "active": state["active"],
            })
        );
    } else {
        println!(
            "saved agy account slot {id} ({}, {} files); active account unchanged ({}).",
            if captured_email.is_empty() {
                "email unknown"
            } else {
                &captured_email
            },
            files.len(),
            state["active"].as_str().unwrap_or("none")
        );
        println!("Next: delegate agy account use {id}");
    }
    Ok(0)
}

fn cmd_use(args: &[String]) -> Res<i32> {
    let (pos, flags, _) = parse_simple(
        "delegate agy account use <id> [--force] [--json]",
        args,
        &["--force", "--json"],
        &[],
    )?;
    if pos.len() != 1 {
        return Err("usage: delegate agy account use <id> [--force] [--json]".into());
    }
    let target_id = &pos[0];
    validate_id(target_id)?;
    let force = has(&flags, "--force");
    let json_out = has(&flags, "--json");

    let root = accounts_root();
    let home_dir = home();
    let _lock = acquire_accounts_lock(&root)?;
    let mut state = load_state(&root);

    if state["accounts"].get(target_id).is_none() {
        return Err(format!(
            "unknown agy account slot {target_id:?}; run `delegate agy account list`"
        ));
    }
    let target_slot = slot_dir(&root, target_id);
    if !slot_has_core_token(&target_slot) {
        return Err(format!(
            "account slot {target_id:?} is missing core OAuth credential files"
        ));
    }

    // Detect any running agy processes (for informational reporting only; switching is allowed
    // while agy runs — running processes and existing sessions continue on their original account,
    // while new runs pick up the newly active account).
    let running = detect_running_agy_for_home(&home_dir);

    let prev_active = state["active"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let current_live_email = live_email(&home_dir).unwrap_or_default();

    // Determine which slot owns the live credentials on disk so refreshed tokens are saved back.
    let save_back_owner: Option<String> = if let Some(ref act) = prev_active {
        let act_email = s(&state["accounts"][act], "email");
        if current_live_email.is_empty()
            || act_email.is_empty()
            || act_email.eq_ignore_ascii_case(&current_live_email)
        {
            Some(act.clone())
        } else {
            find_slot_by_email(&state, &current_live_email)
        }
    } else {
        find_slot_by_email(&state, &current_live_email)
    };

    if save_back_owner.is_none() && home_has_any_live_creds(&home_dir) && !force {
        let msg = if current_live_email.is_empty() {
            "live ~/.gemini credentials exist but do not match any saved slot; import/save them first or pass --force".to_string()
        } else {
            format!(
                "live ~/.gemini account ({current_live_email}) is not tracked in any saved slot; import/save it first or pass --force"
            )
        };
        if json_out {
            println!(
                "{}",
                json!({
                    "ok": false,
                    "error": "untracked_live_credentials",
                    "message": msg,
                    "liveEmail": current_live_email,
                })
            );
        } else {
            eprintln!("delegate agy account use: refused — {msg}");
        }
        return Ok(1);
    }

    let now = iso();
    let mut saved_back_files = 0usize;
    let mut saved_back_id: Option<String> = None;
    if let Some(owner_id) = save_back_owner {
        let owner_slot = slot_dir(&root, &owner_id);
        let owner_email = s(&state["accounts"][&owner_id], "email").to_string();
        saved_back_files = save_live_to_slot_checked(&home_dir, &owner_slot, &owner_email)?;
        if let Some(obj) = state["accounts"][&owner_id].as_object_mut() {
            obj.insert("lastSyncedAt".into(), json!(now));
            if obj.get("email").and_then(Value::as_str).unwrap_or("").is_empty()
                && !current_live_email.is_empty()
            {
                obj.insert("email".into(), json!(current_live_email));
            }
        }
        saved_back_id = Some(owner_id);
    }

    let installed = install_slot_to_home(&target_slot, &home_dir)?;

    state["schemaVersion"] = json!(SCHEMA_VERSION);
    state["active"] = json!(target_id);
    if let Some(obj) = state["accounts"][target_id].as_object_mut() {
        obj.insert("lastUsedAt".into(), json!(now));
        obj.insert("lastSyncedAt".into(), json!(now));
    }
    save_state(&root, &state)?;

    let target_email = s(&state["accounts"][target_id], "email").to_string();
    let running_json: Vec<Value> = running.iter().map(running_agy_to_json).collect();
    if json_out {
        println!(
            "{}",
            json!({
                "ok": true,
                "active": target_id,
                "email": target_email,
                "previousActive": prev_active,
                "savedBackSlot": saved_back_id,
                "savedBackFiles": saved_back_files,
                "installedFiles": installed,
                "lastUsedAt": now,
                "runningAgyCount": running.len(),
                "runningAgy": running_json,
            })
        );
    } else {
        let saved_note = match saved_back_id {
            Some(ref prev) if saved_back_files > 0 => {
                format!(" (saved back {saved_back_files} live files to {prev})")
            }
            _ => String::new(),
        };
        println!(
            "switched agy account to {target_id} ({target_email}, {installed} files installed){saved_note}."
        );
        if !running.is_empty() {
            println!(
                "note: {} running agy process(es) and existing sessions will continue on their original account; new runs will use {target_id}.",
                running.len()
            );
            for p in &running {
                println!("{}", format_running_proc_line(p));
            }
        } else {
            println!(
                "note: running processes and existing sessions continue on their original account; new runs will use {target_id}."
            );
        }
    }
    Ok(0)
}

fn cmd_remove(args: &[String]) -> Res<i32> {
    let (pos, flags, _) = parse_simple(
        "delegate agy account remove <id> [--force] [--json]",
        args,
        &["--force", "--json"],
        &[],
    )?;
    if pos.len() != 1 {
        return Err("usage: delegate agy account remove <id> [--force] [--json]".into());
    }
    let id = &pos[0];
    validate_id(id)?;
    let force = has(&flags, "--force");
    let json_out = has(&flags, "--json");

    let root = accounts_root();
    let _lock = acquire_accounts_lock(&root)?;
    let mut state = load_state(&root);

    if state["accounts"].get(id).is_none() {
        return Err(format!("unknown agy account slot {id:?}"));
    }
    let is_active = state["active"].as_str() == Some(id.as_str());
    if is_active && !force {
        return Err(format!(
            "cannot remove active account {id:?}; switch to another account first or pass --force"
        ));
    }

    let sdir = slot_dir(&root, id);
    if sdir.exists() {
        fs::remove_dir_all(&sdir).map_err(|e| format!("remove {}: {e}", sdir.display()))?;
    }
    if let Some(map) = state["accounts"].as_object_mut() {
        map.remove(id);
    }
    if is_active {
        state["active"] = Value::Null;
    }
    save_state(&root, &state)?;

    if json_out {
        println!(
            "{}",
            json!({
                "ok": true,
                "removed": id,
                "wasActive": is_active,
            })
        );
    } else {
        println!("removed agy account slot {id} (live ~/.gemini files untouched).");
    }
    Ok(0)
}

/// Minimal standard/URL-safe base64 decoder (ignores ASCII whitespace, optional `=` padding).
pub fn decode_base64(input: &str) -> Res<Vec<u8>> {
    let mut vals: Vec<u8> = Vec::with_capacity(input.len());
    let mut padding = 0usize;
    for b in input.bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            padding += 1;
            continue;
        }
        if padding > 0 {
            return Err("invalid base64: data after padding".into());
        }
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return Err(format!("invalid base64 byte: 0x{b:02x}")),
        };
        vals.push(v);
    }
    if vals.len() % 4 == 1 {
        return Err("invalid base64 length".into());
    }
    let mut out = Vec::with_capacity((vals.len() * 3) / 4);
    for chunk in vals.chunks(4) {
        let a = chunk[0] as u32;
        let b = chunk[1] as u32;
        let c = chunk.get(2).copied().unwrap_or(0) as u32;
        let d = chunk.get(3).copied().unwrap_or(0) as u32;
        let n = (a << 18) | (b << 12) | (c << 6) | d;
        out.push(((n >> 16) & 0xff) as u8);
        if chunk.len() > 2 {
            out.push(((n >> 8) & 0xff) as u8);
        }
        if chunk.len() > 3 {
            out.push((n & 0xff) as u8);
        }
    }
    Ok(out)
}

/// Decode one `agy-swap` `vault.json` value into its underlying JSON object.
/// Supports `go-keyring-base64:<b64>`, plain JSON strings, and raw base64 JSON.
pub fn decode_vault_entry(raw_val: &str) -> Res<Value> {
    let trimmed = raw_val.trim();
    if let Some(b64) = trimmed.strip_prefix("go-keyring-base64:") {
        let bytes = decode_base64(b64)?;
        let text = String::from_utf8(bytes).map_err(|_| "vault entry is not valid UTF-8")?;
        return serde_json::from_str::<Value>(&text)
            .map_err(|_| "vault entry decoded payload is not valid JSON".into());
    }
    if trimmed.starts_with('{') {
        return serde_json::from_str::<Value>(trimmed)
            .map_err(|_| "vault entry is not valid JSON".into());
    }
    let bytes = decode_base64(trimmed)?;
    let text = String::from_utf8(bytes).map_err(|_| "vault entry is not valid UTF-8")?;
    serde_json::from_str::<Value>(&text)
        .map_err(|_| "vault entry decoded payload is not valid JSON".into())
}

fn synthesize_oauth_creds(vault_obj: &Value) -> Value {
    let tok = &vault_obj["token"];
    let access_token = s(tok, "access_token");
    let refresh_token = s(tok, "refresh_token");
    let id_token = s(tok, "id_token");
    let scope = s(tok, "scope");
    let token_type = {
        let t = s(tok, "token_type");
        if t.is_empty() {
            "Bearer"
        } else {
            t
        }
    };
    let expiry_date = tok["expiry_date"].as_i64().unwrap_or(0);
    json!({
        "access_token": access_token,
        "expiry_date": expiry_date,
        "id_token": id_token,
        "refresh_token": refresh_token,
        "scope": scope,
        "token_type": token_type,
    })
}

fn cmd_import_agy_swap(args: &[String]) -> Res<i32> {
    let usage = "delegate agy account import-agy-swap [--dry-run] [--source DIR] [--overwrite] [--json]";
    let (pos, flags, kv) = parse_simple(
        usage,
        args,
        &["--dry-run", "--overwrite", "--json"],
        &["--source"],
    )?;
    if !pos.is_empty() {
        return Err(unrecognized_arg(
            &pos.join(" "),
            &["--dry-run", "--overwrite", "--json", "--source"],
            usage,
        ));
    }
    let dry_run = has(&flags, "--dry-run");
    let overwrite = has(&flags, "--overwrite");
    let json_out = has(&flags, "--json");

    let home_dir = home();
    let source_dir = value(&kv, "--source")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir.join(".gemini/agy-swap"));
    let vault_path = source_dir.join("vault.json");
    if !vault_path.is_file() {
        return Err(format!(
            "agy-swap vault not found: {}",
            vault_path.display()
        ));
    }
    let vault_raw = fs::read_to_string(&vault_path)
        .map_err(|e| format!("read {}: {e}", vault_path.display()))?;
    let vault: Value = serde_json::from_str(&vault_raw)
        .map_err(|_| format!("invalid JSON in {}", vault_path.display()))?;
    let vault_map = vault
        .as_object()
        .ok_or_else(|| format!("{} must be a JSON object", vault_path.display()))?;

    let swap_accounts = json(source_dir.join("accounts.json"));
    let current_live_email = live_email(&home_dir).unwrap_or_default();
    let live_scope_path = home_dir.join(".config/antigravity/credential-user-scope");

    // Collect entries deterministically sorted by email.
    let mut parsed_entries: BTreeMap<String, (Value, String)> = BTreeMap::new();
    for (k, v) in vault_map {
        let Some(key_email) = k.strip_prefix("account:") else {
            continue;
        };
        let Some(raw_str) = v.as_str() else {
            return Err(format!("vault entry for {key_email} is not a string"));
        };
        let decoded = decode_vault_entry(raw_str)
            .map_err(|e| format!("failed to decode vault entry for {key_email}: {e}"))?;
        let tok = &decoded["token"];
        if s(tok, "refresh_token").is_empty() && s(tok, "access_token").is_empty() {
            return Err(format!(
                "vault entry for {key_email} has neither refresh_token nor access_token"
            ));
        }
        let email = {
            let e = s(&decoded, "id_token_email");
            if !e.is_empty() {
                e.to_string()
            } else {
                key_email.to_string()
            }
        };
        let label = swap_accounts
            .get(&email)
            .or_else(|| swap_accounts.get(key_email))
            .map(|meta| s(meta, "name").to_string())
            .unwrap_or_default();
        parsed_entries.insert(email, (decoded, label));
    }

    if parsed_entries.is_empty() {
        return Err(format!(
            "no `account:<email>` entries found in {}",
            vault_path.display()
        ));
    }

    let root = accounts_root();
    let _lock = if dry_run {
        None
    } else {
        Some(acquire_accounts_lock(&root)?)
    };
    let mut state = load_state(&root);
    if !state["accounts"].is_object() {
        state["accounts"] = json!({});
    }

    let mut planned_existing = state["accounts"].as_object().cloned().unwrap_or_default();
    let mut report: Vec<Value> = Vec::new();
    let mut imported_count = 0usize;
    let mut skipped_count = 0usize;
    let now = iso();

    for (email, (decoded_tok, label)) in &parsed_entries {
        let existing_id = find_slot_by_email(&state, email);
        let (slot_id, action) = match existing_id {
            Some(eid) if !overwrite => (eid, "skip"),
            Some(eid) => (eid, "overwrite"),
            None => {
                let id = derive_id(email, &planned_existing);
                planned_existing.insert(id.clone(), json!({"email": email}));
                (id, "create")
            }
        };

        let matches_live =
            !current_live_email.is_empty() && current_live_email.eq_ignore_ascii_case(email);
        let include_scope = matches_live && live_scope_path.is_file();
        let mut files = vec![
            "oauth_creds.json",
            "google_accounts.json",
            "jetski-standalone-oauth-token",
            "antigravity-cli-oauth-token",
            "antigravity-oauth-token",
        ];
        if include_scope {
            files.push("credential-user-scope");
        }

        if !dry_run {
            if action == "skip" {
                skipped_count += 1;
            } else {
                let sdir = slot_dir(&root, &slot_id);
                ensure_dir_mode(&sdir, 0o700)?;

                let jetski_bytes = format!(
                    "{}\n",
                    serde_json::to_string_pretty(decoded_tok).map_err(|e| e.to_string())?
                );
                let oauth_bytes = format!(
                    "{}\n",
                    serde_json::to_string_pretty(&synthesize_oauth_creds(decoded_tok))
                        .map_err(|e| e.to_string())?
                );
                let google_accounts_bytes = format!(
                    "{}\n",
                    serde_json::to_string_pretty(&json!({"active": email, "old": []}))
                        .map_err(|e| e.to_string())?
                );

                write_secret_0600(
                    &sdir.join("jetski-standalone-oauth-token"),
                    jetski_bytes.as_bytes(),
                    true,
                )?;
                write_secret_0600(
                    &sdir.join("antigravity-cli-oauth-token"),
                    jetski_bytes.as_bytes(),
                    true,
                )?;
                write_secret_0600(
                    &sdir.join("antigravity-oauth-token"),
                    jetski_bytes.as_bytes(),
                    true,
                )?;
                write_secret_0600(&sdir.join("oauth_creds.json"), oauth_bytes.as_bytes(), true)?;
                write_secret_0600(
                    &sdir.join("google_accounts.json"),
                    google_accounts_bytes.as_bytes(),
                    true,
                )?;
                if include_scope {
                    let scope_bytes = fs::read(&live_scope_path)
                        .map_err(|e| format!("read {}: {e}", live_scope_path.display()))?;
                    write_secret_0600(&sdir.join("credential-user-scope"), &scope_bytes, true)?;
                }

                // If this account is currently live, also capture any newer live token files without mutating live $HOME.
                if matches_live {
                    let _ = save_live_to_slot_checked(&home_dir, &sdir, email)?;
                }

                let existing_added = state["accounts"][&slot_id]["addedAt"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| now.clone());
                let last_used = if matches_live {
                    now.clone()
                } else {
                    state["accounts"][&slot_id]["lastUsedAt"]
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                };
                state["accounts"][&slot_id] = json!({
                    "id": slot_id,
                    "email": email,
                    "label": label,
                    "addedAt": existing_added,
                    "lastUsedAt": last_used,
                    "lastSyncedAt": now,
                    "source": "import-agy-swap",
                });
                if matches_live && state["active"].as_str().is_none_or(str::is_empty) {
                    state["active"] = json!(slot_id);
                }
                imported_count += 1;
            }
        }

        report.push(json!({
            "id": slot_id,
            "email": email,
            "label": label,
            "action": action,
            "matchesLive": matches_live,
            "files": files,
            "fileCount": files.len(),
        }));
    }

    if !dry_run {
        state["schemaVersion"] = json!(SCHEMA_VERSION);
        save_state(&root, &state)?;
    }

    if json_out {
        println!(
            "{}",
            json!({
                "ok": true,
                "dryRun": dry_run,
                "source": source_dir.to_string_lossy(),
                "destination": root.to_string_lossy(),
                "active": state["active"],
                "imported": imported_count,
                "skipped": skipped_count,
                "slots": report,
            })
        );
    } else if dry_run {
        println!(
            "dry-run: would import {} agy-swap account(s) from {} into {}:",
            report.len(),
            source_dir.display(),
            root.display()
        );
        for item in &report {
            let live_tag = if b(item, "matchesLive") {
                " [matches live ~/.gemini]"
            } else {
                ""
            };
            let lbl = s(item, "label");
            let lbl_part = if lbl.is_empty() {
                String::new()
            } else {
                format!(" ({lbl})")
            };
            println!(
                "  [{}] {}  {}{}{} ({} files)",
                s(item, "action"),
                s(item, "id"),
                s(item, "email"),
                lbl_part,
                live_tag,
                item["fileCount"].as_u64().unwrap_or(0)
            );
        }
        println!("No files were modified and no tokens were printed.");
        println!("Next: delegate agy account import-agy-swap");
    } else {
        println!(
            "imported {} slot(s) ({} skipped) into {} (active: {}, agy-swap source left intact).",
            imported_count,
            skipped_count,
            root.display(),
            state["active"].as_str().unwrap_or("none")
        );
        for item in &report {
            println!(
                "  [{}] {}  {} ({} files)",
                s(item, "action"),
                s(item, "id"),
                s(item, "email"),
                item["fileCount"].as_u64().unwrap_or(0)
            );
        }
        println!("Next: delegate agy account list | delegate agy account restore-entry --dry-run");
    }
    Ok(0)
}

fn classify_entry(entry: &Path) -> String {
    match fs::symlink_metadata(entry) {
        Err(_) => "missing".to_string(),
        Ok(meta) if meta.file_type().is_symlink() => {
            let target = fs::read_link(entry)
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            format!("symlink -> {target}")
        }
        Ok(_) => {
            let text = read(entry);
            if text.contains("agy-swap") {
                "agy-swap wrapper".to_string()
            } else if text.contains("Managed by delegate") {
                "delegate wrapper".to_string()
            } else if text.starts_with("#!") {
                "shell wrapper".to_string()
            } else {
                "binary".to_string()
            }
        }
    }
}

fn cmd_restore_entry(args: &[String]) -> Res<i32> {
    let usage = "delegate agy account restore-entry [--dry-run] [--force] [--mode wrapper|symlink] [--entry PATH] [--real-bin PATH] [--json]";
    let (pos, flags, kv) = parse_simple(
        usage,
        args,
        &["--dry-run", "--force", "--json"],
        &["--mode", "--entry", "--real-bin"],
    )?;
    if !pos.is_empty() {
        return Err(unrecognized_arg(
            &pos.join(" "),
            &["--dry-run", "--force", "--json", "--mode", "--entry", "--real-bin"],
            usage,
        ));
    }
    let dry_run = has(&flags, "--dry-run");
    let force = has(&flags, "--force");
    let json_out = has(&flags, "--json");
    let mode = value(&kv, "--mode").unwrap_or("wrapper");
    if !matches!(mode, "wrapper" | "symlink") {
        return Err(format!("--mode must be `wrapper` or `symlink`, got {mode:?}"));
    }

    let home_dir = home();
    let entry = value(&kv, "--entry")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir.join(".local/bin/agy"));
    let real_bin = value(&kv, "--real-bin")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir.join(".local/libexec/agy"));

    if !is_executable_file(&real_bin) {
        return Err(format!(
            "real agy binary not found or not executable: {}",
            real_bin.display()
        ));
    }

    let current_kind = classify_entry(&entry);
    let backup = PathBuf::from(format!("{}.bak-agy-swap", entry.display()));
    let default_real = home_dir.join(".local/libexec/agy");
    let wrapper_exec_target = if real_bin == default_real {
        "\"$HOME/.local/libexec/agy\"".to_string()
    } else {
        shell_quote(&real_bin.to_string_lossy())
    };
    let wrapper_content = format!(
        "#!/bin/sh\n# Managed by delegate (direct exec, no agy-swap)\nexec {wrapper_exec_target} \"$@\"\n"
    );

    if dry_run {
        if json_out {
            println!(
                "{}",
                json!({
                    "ok": true,
                    "dryRun": true,
                    "mode": mode,
                    "entry": entry.to_string_lossy(),
                    "realBin": real_bin.to_string_lossy(),
                    "currentState": current_kind,
                    "backupPath": backup.to_string_lossy(),
                })
            );
        } else {
            println!("dry-run: restore agy entry point");
            println!("  entry:    {} (currently: {current_kind})", entry.display());
            println!("  real bin: {}", real_bin.display());
            println!("  mode:     {mode}");
            println!("  backup:   {}", backup.display());
            println!("Next: delegate agy account restore-entry --mode {mode}");
        }
        return Ok(0);
    }

    let running = detect_running_agy_for_home(&home_dir);
    if !running.is_empty() && !force {
        if json_out {
            let procs: Vec<Value> = running
                .iter()
                .map(|p| json!({"pid": p.pid, "run": p.delegate_run, "cmdline": p.cmdline}))
                .collect();
            println!(
                "{}",
                json!({
                    "ok": false,
                    "error": "running_agy_processes",
                    "message": format!("refused while {} agy process(es) are running (pass --force to override)", running.len()),
                    "runningAgy": procs,
                })
            );
        } else {
            eprintln!(
                "delegate agy account restore-entry: refused — {} agy process(es) running in this HOME:",
                running.len()
            );
            for p in &running {
                eprintln!("{}", format_running_proc_line(p));
            }
            eprintln!("Wait for them to finish or pass --force.");
        }
        return Ok(1);
    }

    let root = accounts_root();
    let _lock = acquire_accounts_lock(&root)?;

    let mut backed_up = false;
    if fs::symlink_metadata(&entry).is_ok()
        && current_kind != "delegate wrapper"
        && !backup.exists()
    {
        if fs::symlink_metadata(&entry)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            if let Ok(target) = fs::read_link(&entry) {
                let _ = std::os::unix::fs::symlink(target, &backup);
                backed_up = true;
            }
        } else {
            fs::copy(&entry, &backup)
                .map_err(|e| format!("backup {} -> {}: {e}", entry.display(), backup.display()))?;
            backed_up = true;
        }
    }

    if mode == "wrapper" {
        write_executable_0755(&entry, wrapper_content.as_bytes())?;
    } else {
        if let Some(parent) = entry.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let tmp = atomic_tmp_path(&entry);
        std::os::unix::fs::symlink(&real_bin, &tmp)
            .map_err(|e| format!("symlink {}: {e}", tmp.display()))?;
        if let Err(err) = fs::rename(&tmp, &entry) {
            let _ = fs::remove_file(&tmp);
            return Err(format!("rename {} -> {}: {err}", tmp.display(), entry.display()));
        }
    }

    if json_out {
        println!(
            "{}",
            json!({
                "ok": true,
                "mode": mode,
                "entry": entry.to_string_lossy(),
                "realBin": real_bin.to_string_lossy(),
                "previousState": current_kind,
                "backedUp": backed_up,
                "backupPath": backup.to_string_lossy(),
            })
        );
    } else {
        println!(
            "restored {} ({mode} -> {})",
            entry.display(),
            real_bin.display()
        );
        if backed_up {
            println!("backup saved: {}", backup.display());
        }
        println!("rollback: mv -f {} {}", backup.display(), entry.display());
    }
    Ok(0)
}

fn parse_pct_flag(raw: &str, flag: &str) -> Res<f64> {
    let trimmed = raw.trim().trim_end_matches('%');
    let v = trimmed
        .parse::<f64>()
        .map_err(|_| format!("{flag} must be a percentage in 0..=100, got {raw:?}"))?;
    if !v.is_finite() || !(0.0..=100.0).contains(&v) {
        return Err(format!("{flag} must be in 0..=100, got {raw:?}"));
    }
    Ok((v * 100.0).round() / 100.0)
}

fn cmd_note(args: &[String]) -> Res<i32> {
    let (pos, flags, kv) = parse_simple(
        "delegate agy account note <id> [--weekly PCT] [--five-hour PCT] [--clear] [--json]",
        args,
        &["--clear", "--json"],
        &["--weekly", "--five-hour"],
    )?;
    if pos.len() != 1 {
        return Err(
            "usage: delegate agy account note <id> [--weekly PCT] [--five-hour PCT] [--clear] [--json]"
                .into(),
        );
    }
    let id = &pos[0];
    validate_id(id)?;
    let clear = has(&flags, "--clear");
    let json_out = has(&flags, "--json");
    let weekly = match value(&kv, "--weekly") {
        Some(raw) => Some(parse_pct_flag(raw, "--weekly")?),
        None => None,
    };
    let five_hour = match value(&kv, "--five-hour") {
        Some(raw) => Some(parse_pct_flag(raw, "--five-hour")?),
        None => None,
    };
    if !clear && weekly.is_none() && five_hour.is_none() {
        return Err("provide at least one of --weekly <PCT>, --five-hour <PCT>, or --clear".into());
    }

    let root = accounts_root();
    let _lock = acquire_accounts_lock(&root)?;
    let mut state = load_state(&root);
    if state["accounts"].get(id).is_none() {
        return Err(format!(
            "unknown agy account slot {id:?}; run `delegate agy account list`"
        ));
    }

    let now = iso();
    let now_ep = epoch() as i64;
    if let Some(obj) = state["accounts"][id].as_object_mut() {
        if clear {
            obj.remove("quotaNotes");
        } else {
            let mut notes = obj
                .get("quotaNotes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut entry = json!({
                "at": now,
                "epoch": now_ep,
            });
            if let Some(w) = weekly {
                entry["weekly"] = json!(w);
            }
            if let Some(fh) = five_hour {
                entry["fiveHour"] = json!(fh);
            }
            notes.push(entry);
            obj.insert("quotaNotes".into(), Value::Array(notes));
        }
    }
    state["schemaVersion"] = json!(SCHEMA_VERSION);
    save_state(&root, &state)?;
    drop(_lock);

    let snap = snapshot_json();
    let acct_view = snap["accounts"]
        .as_array()
        .and_then(|arr| arr.iter().find(|a| s(a, "id") == id))
        .cloned()
        .unwrap_or(Value::Null);

    if json_out {
        println!(
            "{}",
            json!({
                "ok": true,
                "id": id,
                "cleared": clear,
                "quotaNote": acct_view["quotaNote"],
                "quotaEstimate": acct_view["quotaEstimate"],
                "usage": acct_view["usage"],
            })
        );
        return Ok(0);
    }

    if clear {
        println!("cleared quota snapshots for agy account slot {id}.");
        return Ok(0);
    }

    let mut parts = Vec::new();
    if let Some(w) = weekly {
        parts.push(format!("weekly={w:.2}%"));
    }
    if let Some(fh) = five_hour {
        parts.push(format!("five-hour={fh:.2}%"));
    }
    println!(
        "recorded quota snapshot for {id}: {} @ {now} (advisory only; no automatic switching).",
        parts.join(", ")
    );
    for (label, key) in [("weekly", "weekly"), ("five-hour", "fiveHour")] {
        let est = &acct_view["quotaEstimate"][key];
        if est.is_object() {
            let tpp = crate::usage::format_token_compact(n(est, "tokensPerPct"));
            let rem_h = est["estimatedRemainingHours"]
                .as_f64()
                .map(crate::usage::format_hours_compact)
                .unwrap_or_else(|| "-".to_string());
            let method = s(est, "method");
            println!("  {label} estimate ({method}): ~{tpp} tok / 1% · est remaining at current pace: {rem_h}");
        }
    }
    println!("Next: delegate agy account status | delegate usage --account {id}");
    Ok(0)
}

fn print_account_help() {
    println!(
        "Usage: delegate agy account <subcommand> [options]\n\n\
         Manage local Antigravity CLI (agy) accounts manually (~/.config/delegate/agy-accounts/).\n\
         No automatic rotation; real quota uses `agy -p /usage </dev/null` only (no internal HTTP APIs).\n\n\
         Subcommands:\n\
         \x20 list [--quota] [--agy-bin PATH] [--json]\n\
         \x20                                      List saved account slots, active marker, 5h/7d usage & quota\n\
         \x20 status [--quota] [--agy-bin PATH] [--json]\n\
         \x20                                      Show active account, live email, quota, usage, running agy\n\
         \x20 add [<id>] [--label TEXT] [--overwrite] [--agy-bin PATH] [-- <agy-args>...]\n\
         \x20                                      Run interactive agy login in a temp HOME and save slot\n\
         \x20 use <id> [--force] [--json]          Save back refreshed tokens and switch (running processes & existing sessions stay on original account; new runs use new account)\n\
         \x20 note <id> [--weekly PCT] [--five-hour PCT] [--clear] [--json]\n\
         \x20                                      Record manual remaining-% snapshot (fallback when real quota absent)\n\
         \x20 remove <id> [--force] [--json]       Delete a saved account slot (alias: rm)\n\
         \x20 import-agy-swap [--dry-run] [--source DIR] [--overwrite] [--json]\n\
         \x20                                      Import accounts from ~/.gemini/agy-swap without printing tokens\n\
         \x20 restore-entry [--dry-run] [--mode wrapper|symlink] [--entry PATH] [--real-bin PATH] [--force] [--json]\n\
         \x20                                      Replace agy-swap wrapper with direct exec wrapper or symlink"
    );
}

pub fn command(args: &[String]) -> Res<i32> {
    let Some(first) = args.first().map(String::as_str) else {
        print_account_help();
        return Ok(0);
    };
    if matches!(first, "-h" | "--help" | "help") {
        print_account_help();
        return Ok(0);
    }
    if !matches!(first, "account" | "accounts") {
        return Err(format!(
            "unknown agy subcommand {first:?} (expected: account)"
        ));
    }
    let rest = &args[1..];
    let Some(sub) = rest.first().map(String::as_str) else {
        return cmd_list(&[]);
    };
    if matches!(sub, "-h" | "--help" | "help") {
        print_account_help();
        return Ok(0);
    }
    let sub_args = &rest[1..];
    if sub_args
        .first()
        .is_some_and(|a| matches!(a.as_str(), "-h" | "--help"))
    {
        print_account_help();
        return Ok(0);
    }
    match sub {
        "list" | "ls" => cmd_list(sub_args),
        "status" => cmd_status(sub_args),
        "add" => cmd_add(sub_args),
        "use" => cmd_use(sub_args),
        "note" => cmd_note(sub_args),
        "remove" | "rm" => cmd_remove(sub_args),
        "import-agy-swap" => cmd_import_agy_swap(sub_args),
        "restore-entry" => cmd_restore_entry(sub_args),
        other => Err(format!(
            "unknown `delegate agy account` subcommand {other:?} (try --help)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decodes_standard_and_vault_prefix() {
        // "eyJmb28iOiJiYXIifQ==" -> {"foo":"bar"}
        let raw = decode_base64("eyJmb28iOiJiYXIifQ==").unwrap();
        assert_eq!(String::from_utf8(raw).unwrap(), r#"{"foo":"bar"}"#);

        let v = decode_vault_entry("go-keyring-base64:eyJmb28iOiJiYXIifQ==").unwrap();
        assert_eq!(v["foo"], "bar");

        let plain = decode_vault_entry(r#"{"foo":"baz"}"#).unwrap();
        assert_eq!(plain["foo"], "baz");

        assert!(decode_base64("not!valid").is_err());
    }

    #[test]
    fn validate_and_derive_account_ids() {
        assert!(validate_id("work").is_ok());
        assert!(validate_id("acct_1.test-a").is_ok());
        for bad in ["", ".", "..", ".hidden", "a/b", "a..b", "a b"] {
            assert!(validate_id(bad).is_err(), "expected error for {bad:?}");
        }

        let mut existing = Map::new();
        let id1 = derive_id("Alice.Dev+1@example.test", &existing);
        assert_eq!(id1, "alice-dev-1");
        existing.insert(id1.clone(), json!({}));
        let id2 = derive_id("Alice.Dev+1@other.test", &existing);
        assert_eq!(id2, "alice-dev-1-2");
    }

    #[test]
    fn agy_process_matcher_excludes_wrapper_tools_and_delegate() {
        assert!(is_agy_argv(
            &["/home/u/.local/libexec/agy".into(), "--model".into()],
            None
        ));
        assert!(is_agy_argv(
            &["/bin/sh".into(), "/home/u/.local/bin/agy".into(), "--print".into()],
            None
        ));
        assert!(is_agy_argv(
            &["custom".into()],
            Some(Path::new("/home/u/.local/libexec/agy"))
        ));
        assert!(!is_agy_argv(&["agy-swap".into()], None));
        assert!(!is_agy_argv(
            &["/home/u/.local/bin/delegate".into(), "agy".into(), "account".into(), "use".into()],
            None
        ));
        assert!(!is_agy_argv(&["grep".into(), "agy".into()], None));
    }

    #[test]
    fn parse_agy_usage_handles_standard_ansi_missing_and_unknown_lines() {
        let raw = "\
\x1b[1mAccount Usage\x1b[0m
Gemini Models\tWeekly Limit Remaining\t97%\t2026-11-30T20:09:03Z
Claude and GPT models\tFive Hour Limit Remaining\t92%\t2026-11-24T07:32:40Z
Gemini Models\tFive Hour Limit Remaining\t88.5%
Experimental Pool\tDaily Limit Remaining\t60%\t2026-11-25T00:00:00Z
Some unexpected warning line without tabs
BadRow\tWeekly Limit Remaining\tnot-a-pct\t2026-11-30T00:00:00Z
";
        let parsed = parse_agy_usage_output(raw);
        assert_eq!(parsed.entries.len(), 4);
        assert_eq!(parsed.entries[0].pool, "Gemini Models");
        assert_eq!(parsed.entries[0].window, "weekly");
        assert_eq!(parsed.entries[0].remaining_pct, 97.0);
        assert_eq!(parsed.entries[0].reset_at, "2026-11-30T20:09:03Z");

        assert_eq!(parsed.entries[1].pool, "Claude and GPT models");
        assert_eq!(parsed.entries[1].window, "fiveHour");
        assert_eq!(parsed.entries[1].remaining_pct, 92.0);
        assert_eq!(parsed.entries[1].reset_at, "2026-11-24T07:32:40Z");

        // Missing 4th column (reset timestamp) is tolerated without error.
        assert_eq!(parsed.entries[2].pool, "Gemini Models");
        assert_eq!(parsed.entries[2].window, "fiveHour");
        assert_eq!(parsed.entries[2].remaining_pct, 88.5);
        assert_eq!(parsed.entries[2].reset_at, "");

        // Unknown window label still parsed with window = "other".
        assert_eq!(parsed.entries[3].pool, "Experimental Pool");
        assert_eq!(parsed.entries[3].window, "other");
        assert_eq!(parsed.entries[3].remaining_pct, 60.0);

        // Unknown / unparseable lines are preserved verbatim without error.
        assert_eq!(parsed.unknown_lines.len(), 3);
        assert_eq!(parsed.unknown_lines[0], "Account Usage");
        assert_eq!(parsed.unknown_lines[1], "Some unexpected warning line without tabs");
        assert!(parsed.unknown_lines[2].contains("not-a-pct"));

        // Empty / missing-all-rows input produces empty vectors without error.
        let empty = parse_agy_usage_output("\n   \n");
        assert!(empty.entries.is_empty());
        assert!(empty.unknown_lines.is_empty());
    }

    #[test]
    fn enrich_quota_computes_age_and_reset_countdown() {
        let now = iso_epoch("2026-11-24T10:00:00Z").unwrap();
        let fetched = now - 185; // 3m 5s ago
        let raw = json!({
            "fetchedAt": "2026-11-24T09:56:55Z",
            "fetchedEpoch": fetched,
            "ok": true,
            "entries": [
                {
                    "pool": "Gemini Models",
                    "window": "fiveHour",
                    "windowLabel": "Five Hour Limit Remaining",
                    "remainingPct": 91.0,
                    "resetAt": "2026-11-24T13:30:00Z"
                }
            ],
            "unknownLines": ["Note: beta quota"]
        });
        let enriched = enrich_quota_for_snapshot(&raw, now).unwrap();
        assert_eq!(enriched["ageSeconds"], 185);
        assert_eq!(enriched["entries"][0]["resetInSeconds"], 3 * 3600 + 30 * 60);
        assert_eq!(enriched["unknownLines"][0], "Note: beta quota");
        assert_eq!(format_age_compact(185), "3m前");
        assert_eq!(format_countdown_compact(3 * 3600 + 30 * 60), "3h30m");
    }

    fn make_test_token_json(email: &str, access: &str, expiry: &str) -> Vec<u8> {
        serde_json::to_vec_pretty(&json!({
            "auth_method": "consumer",
            "id_token_email": email,
            "token": {
                "access_token": access,
                "refresh_token": format!("REF-{access}"),
                "expiry": expiry,
                "token_type": "Bearer"
            }
        }))
        .unwrap()
    }

    #[test]
    fn cred_overlay_isolates_refresh_on_switch_and_saves_back_to_own_slot() {
        let base = TempHome::create().unwrap();
        let real_home = base.path.join("home");
        let real_cfg = real_home.join(".config");
        let root = real_cfg.join("delegate/agy-accounts");
        fs::create_dir_all(real_home.join(".gemini/antigravity-cli/conversations")).unwrap();
        fs::write(real_home.join(".gemini/settings.json"), r#"{"theme":"dark"}"#).unwrap();
        fs::write(real_home.join(".bashrc"), "# test\n").unwrap();
        fs::write(
            real_home.join(".gemini/antigravity-cli/conversations/c1.db"),
            "conv-data",
        )
        .unwrap();

        let alpha_slot = slot_dir(&root, "alpha");
        let beta_slot = slot_dir(&root, "beta");
        ensure_dir_mode(&alpha_slot, 0o700).unwrap();
        ensure_dir_mode(&beta_slot, 0o700).unwrap();
        let alpha_v1 = make_test_token_json("alpha@example.test", "ALPHA-V1", "2026-11-25T10:00:00Z");
        let beta_v1 = make_test_token_json("beta@example.test", "BETA-V1", "2026-11-25T10:00:00Z");
        write_secret_0600(&alpha_slot.join("antigravity-cli-oauth-token"), &alpha_v1, true).unwrap();
        write_secret_0600(&beta_slot.join("antigravity-cli-oauth-token"), &beta_v1, true).unwrap();
        install_slot_to_home(&alpha_slot, &real_home).unwrap();
        save_state(
            &root,
            &json!({
                "schemaVersion": SCHEMA_VERSION,
                "active": "alpha",
                "accounts": {
                    "alpha": {"id": "alpha", "email": "alpha@example.test"},
                    "beta": {"id": "beta", "email": "beta@example.test"},
                }
            }),
        )
        .unwrap();

        let (acct, overlay_opt) =
            AgyCredOverlay::prepare_active_for(&root, &real_home, &real_cfg).unwrap();
        assert_eq!(acct.as_deref(), Some("alpha"));
        let mut overlay = overlay_opt.expect("overlay should be created");
        let ov_path = overlay.overlay_home().to_path_buf();
        assert_ne!(ov_path, real_home);

        // Non-credential entries are symlinked back to real HOME.
        assert!(fs::symlink_metadata(ov_path.join(".bashrc"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(fs::symlink_metadata(ov_path.join(".gemini/settings.json"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(
            fs::symlink_metadata(ov_path.join(".gemini/antigravity-cli/conversations"))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        // Credential file is a private regular 0600 file, NOT a symlink.
        let ov_tok = ov_path.join(".gemini/antigravity-cli/antigravity-oauth-token");
        let ov_tok_meta = fs::symlink_metadata(&ov_tok).unwrap();
        assert!(ov_tok_meta.file_type().is_file());
        assert_eq!(ov_tok_meta.permissions().mode() & 0o777, 0o600);

        // Simulate `delegate agy account use beta` while alpha's process is running.
        install_slot_to_home(&beta_slot, &real_home).unwrap();
        let mut st = load_state(&root);
        st["active"] = json!("beta");
        save_state(&root, &st).unwrap();

        // Simulate running alpha process refreshing its token inside its private overlay.
        let alpha_v2 = make_test_token_json("alpha@example.test", "ALPHA-V2", "2026-11-25T11:00:00Z");
        fs::write(&ov_tok, &alpha_v2).unwrap();

        // Process exits -> save back to alpha's slot WITHOUT touching live beta credentials.
        let saved = overlay.finish();
        assert_eq!(saved, 1);
        drop(overlay);
        assert!(!ov_path.exists(), "overlay HOME must be cleaned on drop");

        let live_doc: Value = serde_json::from_slice(
            &fs::read(real_home.join(".gemini/antigravity-cli/antigravity-oauth-token")).unwrap(),
        )
        .unwrap();
        assert_eq!(live_doc["id_token_email"], "beta@example.test");
        assert_eq!(live_doc["token"]["access_token"], "BETA-V1");

        let alpha_slot_doc: Value = serde_json::from_slice(
            &fs::read(alpha_slot.join("antigravity-cli-oauth-token")).unwrap(),
        )
        .unwrap();
        assert_eq!(alpha_slot_doc["token"]["access_token"], "ALPHA-V2");

        let beta_slot_doc: Value = serde_json::from_slice(
            &fs::read(beta_slot.join("antigravity-cli-oauth-token")).unwrap(),
        )
        .unwrap();
        assert_eq!(beta_slot_doc["token"]["access_token"], "BETA-V1");
    }

    #[test]
    fn cred_overlay_mirrors_to_live_when_still_active_and_rejects_corrupt_or_mismatched() {
        let base = TempHome::create().unwrap();
        let real_home = base.path.join("home");
        let real_cfg = real_home.join(".config");
        let root = real_cfg.join("delegate/agy-accounts");
        fs::create_dir_all(real_home.join(".gemini/antigravity-cli")).unwrap();

        let alpha_slot = slot_dir(&root, "alpha");
        ensure_dir_mode(&alpha_slot, 0o700).unwrap();
        let alpha_v1 = make_test_token_json("alpha@example.test", "ALPHA-V1", "2026-11-25T10:00:00Z");
        write_secret_0600(&alpha_slot.join("antigravity-cli-oauth-token"), &alpha_v1, true).unwrap();
        install_slot_to_home(&alpha_slot, &real_home).unwrap();
        save_state(
            &root,
            &json!({
                "schemaVersion": SCHEMA_VERSION,
                "active": "alpha",
                "accounts": {
                    "alpha": {"id": "alpha", "email": "alpha@example.test"}
                }
            }),
        )
        .unwrap();

        // 1. Corrupt/truncated JSON or mismatched email must NOT overwrite the slot.
        let (_, mut ov_bad) = AgyCredOverlay::prepare_active_for(&root, &real_home, &real_cfg).unwrap();
        let bad = ov_bad.as_mut().unwrap();
        let bad_tok = bad.overlay_home().join(".gemini/antigravity-cli/antigravity-oauth-token");
        fs::write(&bad_tok, b"{truncated").unwrap();
        assert_eq!(bad.finish(), 0);
        drop(ov_bad);

        let (_, mut ov_mismatch) = AgyCredOverlay::prepare_active_for(&root, &real_home, &real_cfg).unwrap();
        let mm = ov_mismatch.as_mut().unwrap();
        let mm_tok = mm.overlay_home().join(".gemini/antigravity-cli/antigravity-oauth-token");
        fs::write(
            &mm_tok,
            make_test_token_json("other@example.test", "OTHER-V2", "2026-11-25T12:00:00Z"),
        )
        .unwrap();
        assert_eq!(mm.finish(), 0);
        drop(ov_mismatch);

        // 2. Valid refresh while still active updates both slot and live ~/.gemini.
        let (_, mut ov_ok) = AgyCredOverlay::prepare_active_for(&root, &real_home, &real_cfg).unwrap();
        let ok = ov_ok.as_mut().unwrap();
        let ok_tok = ok.overlay_home().join(".gemini/antigravity-cli/antigravity-oauth-token");
        fs::write(
            &ok_tok,
            make_test_token_json("alpha@example.test", "ALPHA-V3", "2026-11-25T12:00:00Z"),
        )
        .unwrap();
        assert_eq!(ok.finish(), 1);
        drop(ov_ok);

        let slot_doc: Value = serde_json::from_slice(
            &fs::read(alpha_slot.join("antigravity-cli-oauth-token")).unwrap(),
        )
        .unwrap();
        let live_doc: Value = serde_json::from_slice(
            &fs::read(real_home.join(".gemini/antigravity-cli/antigravity-oauth-token")).unwrap(),
        )
        .unwrap();
        assert_eq!(slot_doc["token"]["access_token"], "ALPHA-V3");
        assert_eq!(live_doc["token"]["access_token"], "ALPHA-V3");
    }

    #[test]
    fn parse_delegate_run_dir_distinguishes_author_and_reviewer_rounds() {
        let author = Path::new("/repo/.local/run/delegate/20261125-113402-sit-chair-fix");
        let (rdir, role, round) = parse_delegate_run_dir(author);
        assert_eq!(rdir, author);
        assert_eq!(role.as_deref(), Some("author"));
        assert_eq!(round, None);

        let rev = Path::new(
            "/repo/.local/run/delegate/20261125-112949-org-step4-signup/review/round-1",
        );
        let (rdir2, role2, round2) = parse_delegate_run_dir(rev);
        assert_eq!(rdir2, Path::new("/repo/.local/run/delegate/20261125-112949-org-step4-signup"));
        assert_eq!(role2.as_deref(), Some("reviewer"));
        assert_eq!(round2.as_deref(), Some("round-1"));
    }

    #[test]
    fn cred_overlay_apply_env_anchors_toolchain_and_xdg_data_to_real_home() {
        let base = TempHome::create().unwrap();
        let real_home = base.path.join("home");
        let real_cfg = real_home.join(".config");
        let root = real_cfg.join("delegate/agy-accounts");
        fs::create_dir_all(real_home.join(".gemini/antigravity-cli")).unwrap();

        let alpha_slot = slot_dir(&root, "alpha");
        ensure_dir_mode(&alpha_slot, 0o700).unwrap();
        let alpha_v1 = make_test_token_json("alpha@example.test", "ALPHA-V1", "2026-11-25T10:00:00Z");
        write_secret_0600(&alpha_slot.join("antigravity-cli-oauth-token"), &alpha_v1, true).unwrap();
        install_slot_to_home(&alpha_slot, &real_home).unwrap();
        save_state(
            &root,
            &json!({
                "schemaVersion": SCHEMA_VERSION,
                "active": "alpha",
                "accounts": {
                    "alpha": {"id": "alpha", "email": "alpha@example.test"}
                }
            }),
        )
        .unwrap();

        let (_, overlay_opt) =
            AgyCredOverlay::prepare_active_for(&root, &real_home, &real_cfg).unwrap();
        let overlay = overlay_opt.expect("overlay should be created");
        let ov_path = overlay.overlay_home().to_path_buf();
        assert_ne!(ov_path, real_home);

        let collect_envs = |cmd: &Command| -> BTreeMap<String, Option<PathBuf>> {
            cmd.get_envs()
                .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(PathBuf::from)))
                .collect()
        };

        // 1. When parent env is unset or empty, toolchain & non-credential XDG paths anchor to real_home.
        let mut cmd_unset = Command::new("true");
        overlay.apply_env_with(&mut cmd_unset, |k| {
            if k == "CARGO_HOME" {
                Some(std::ffi::OsString::new())
            } else {
                None
            }
        });
        let envs_unset = collect_envs(&cmd_unset);
        assert_eq!(envs_unset.get("HOME").and_then(Option::as_deref), Some(ov_path.as_path()));
        assert_eq!(
            envs_unset.get("XDG_CONFIG_HOME").and_then(Option::as_deref),
            Some(ov_path.join(".config").as_path())
        );
        for (key, rel) in [
            ("RUSTUP_HOME", ".rustup"),
            ("CARGO_HOME", ".cargo"),
            ("XDG_DATA_HOME", ".local/share"),
            ("XDG_STATE_HOME", ".local/state"),
            ("XDG_CACHE_HOME", ".cache"),
        ] {
            let val = envs_unset
                .get(key)
                .and_then(Option::as_deref)
                .unwrap_or_else(|| panic!("{key} should be set when parent env is unset/empty"));
            assert_eq!(val, real_home.join(rel).as_path(), "{key} must point to real_home/{rel}");
            assert!(
                !val.starts_with(&ov_path),
                "{key} ({}) must not point inside temporary overlay HOME ({})",
                val.display(),
                ov_path.display()
            );
        }

        // 2. When parent env already sets these variables, apply_env preserves parent values.
        let mut cmd_preset = Command::new("true");
        overlay.apply_env_with(&mut cmd_preset, |k| {
            Some(std::ffi::OsString::from(format!("/custom/{k}")))
        });
        let envs_preset = collect_envs(&cmd_preset);
        assert_eq!(envs_preset.get("HOME").and_then(Option::as_deref), Some(ov_path.as_path()));
        assert_eq!(
            envs_preset.get("XDG_CONFIG_HOME").and_then(Option::as_deref),
            Some(ov_path.join(".config").as_path())
        );
        for key in [
            "RUSTUP_HOME",
            "CARGO_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ] {
            assert!(
                !envs_preset.contains_key(key),
                "{key} should not be overridden when already set in parent env"
            );
        }

        // 3. Real apply_env delegates consistently against the live process environment.
        let mut cmd_live = Command::new("true");
        overlay.apply_env(&mut cmd_live);
        let envs_live = collect_envs(&cmd_live);
        assert_eq!(envs_live.get("HOME").and_then(Option::as_deref), Some(ov_path.as_path()));
        assert_eq!(
            envs_live.get("XDG_CONFIG_HOME").and_then(Option::as_deref),
            Some(ov_path.join(".config").as_path())
        );
        for (key, rel) in [
            ("RUSTUP_HOME", ".rustup"),
            ("CARGO_HOME", ".cargo"),
            ("XDG_DATA_HOME", ".local/share"),
        ] {
            if env::var_os(key).is_none_or(|v| v.is_empty()) {
                let val = envs_live
                    .get(key)
                    .and_then(Option::as_deref)
                    .unwrap_or_else(|| panic!("{key} should be set by apply_env"));
                assert_eq!(val, real_home.join(rel).as_path());
                assert!(!val.starts_with(&ov_path));
            } else {
                assert!(!envs_live.contains_key(key));
            }
        }
    }

    #[test]
    fn live_email_prioritizes_antigravity_cli_token_over_stale_google_accounts() {
        let base = TempHome::create().unwrap();
        let h = &base.path;
        fs::create_dir_all(h.join(".gemini/antigravity-cli")).unwrap();
        fs::write(
            h.join(".gemini/google_accounts.json"),
            r#"{"active":"jzy@example.test","old":[]}"#,
        )
        .unwrap();
        // Before cli token exists, falls back to google_accounts.json.
        assert_eq!(live_email(h).as_deref(), Some("jzy@example.test"));

        // When a long-lived agy process overwrites antigravity-cli/antigravity-oauth-token
        // with another account without touching google_accounts.json, live_email must report it.
        fs::write(
            h.join(".gemini/antigravity-cli/antigravity-oauth-token"),
            make_test_token_json("guimond@example.test", "AT-G", "2026-11-25T19:50:07Z"),
        )
        .unwrap();
        assert_eq!(live_email(h).as_deref(), Some("guimond@example.test"));
        let all = collect_home_cred_emails(h);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].1, "guimond@example.test");
        assert_eq!(all[1].1, "jzy@example.test");
    }

    #[test]
    fn verify_slot_identity_checks_creds_output_and_agy_logs() {
        let out = "\
Account: beta@example.test
Gemini Models\tWeekly Limit Remaining\t50%\t2026-11-30T20:09:03Z
Post \"https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary\"
";
        let out_emails = extract_identity_emails_from_output(out);
        assert_eq!(out_emails, vec!["beta@example.test".to_string()]);

        let base = TempHome::create().unwrap();
        let log_dir = base.path.join(".gemini/antigravity-cli/log");
        fs::create_dir_all(&log_dir).unwrap();
        fs::write(
            log_dir.join("cli-test.log"),
            "I1125 19:02:06 server_oauth.go:221] applyAuthResult: email=beta@example.test, authMethod=consumer\n\
             I1125 19:02:06 server_oauth.go:226] OAuth: authenticated successfully as beta@example.test\n",
        )
        .unwrap();
        let log_emails = extract_identity_emails_from_agy_logs(&base.path);
        assert_eq!(log_emails, vec!["beta@example.test".to_string()]);

        let alpha_creds = vec![(
            ".gemini/antigravity-cli/antigravity-oauth-token",
            "alpha@example.test".to_string(),
        )];
        assert!(verify_slot_identity("alpha@example.test", &alpha_creds, &[], &[]).is_ok());
        // Output or log reporting beta when target is alpha must fail with observed = beta.
        assert_eq!(
            verify_slot_identity("alpha@example.test", &alpha_creds, &out_emails, &[]),
            Err("beta@example.test".to_string())
        );
        assert_eq!(
            verify_slot_identity("alpha@example.test", &alpha_creds, &[], &log_emails),
            Err("beta@example.test".to_string())
        );
        // Zero observed identities when target email is known must fail closed.
        assert_eq!(
            verify_slot_identity("alpha@example.test", &[], &[], &[]),
            Err(String::new())
        );
    }

    #[test]
    fn find_duplicate_quota_slot_groups_requires_exact_match_including_reset_at() {
        let same_entries = json!([
            {"pool":"Gemini Models","window":"weekly","windowLabel":"Weekly Limit Remaining","remainingPct":50.0,"resetAt":"2026-11-30T20:09:03Z"},
            {"pool":"Claude and GPT models","window":"weekly","windowLabel":"Weekly Limit Remaining","remainingPct":90.0,"resetAt":"2026-11-27T02:47:42Z"}
        ]);
        let diff_reset = json!([
            {"pool":"Gemini Models","window":"weekly","windowLabel":"Weekly Limit Remaining","remainingPct":50.0,"resetAt":"2026-11-30T20:03:02Z"},
            {"pool":"Claude and GPT models","window":"weekly","windowLabel":"Weekly Limit Remaining","remainingPct":90.0,"resetAt":"2026-11-27T02:47:42Z"}
        ]);

        let dup_accounts = vec![
            json!({"id":"guimond","quota":{"ok":true,"entries":same_entries}}),
            json!({"id":"jzy","quota":{"ok":true,"entries":same_entries}}),
        ];
        assert_eq!(
            find_duplicate_quota_slot_groups(&dup_accounts),
            vec![vec!["guimond".to_string(), "jzy".to_string()]]
        );

        // Same percentages but different resetAt must NOT trigger duplicate warning.
        let distinct_accounts = vec![
            json!({"id":"guimond","quota":{"ok":true,"entries":same_entries}}),
            json!({"id":"jzy","quota":{"ok":true,"entries":diff_reset}}),
        ];
        assert!(find_duplicate_quota_slot_groups(&distinct_accounts).is_empty());

        // Empty entries must NOT trigger duplicate warning.
        let empty_accounts = vec![
            json!({"id":"guimond","quota":{"ok":false,"entries":[]}}),
            json!({"id":"jzy","quota":{"ok":false,"entries":[]}}),
        ];
        assert!(find_duplicate_quota_slot_groups(&empty_accounts).is_empty());
    }

    #[test]
    fn detect_quota_depletion_warnings_flags_only_when_earlier_than_reset() {
        let snap = json!({
            "accounts": [
                {
                    "id": "safe-acct",
                    "active": false,
                    "quota": {
                        "ok": true,
                        "entries": [
                            {"pool": "Gemini Models", "window": "weekly", "remainingPct": 90.0, "resetInSeconds": 432000}
                        ]
                    },
                    "quotaEstimate": {
                        "pools": [
                            {"pool": "Gemini Models", "weekly": {"estimatedRemainingHours": 333.33}}
                        ]
                    }
                },
                {
                    "id": "hot-acct",
                    "active": true,
                    "quota": {
                        "ok": true,
                        "entries": [
                            {"pool": "Gemini Models", "window": "weekly", "remainingPct": 42.0, "resetInSeconds": 432000}
                        ]
                    },
                    "quotaEstimate": {
                        "pools": [
                            {"pool": "Gemini Models", "weekly": {"estimatedRemainingHours": 11.0}}
                        ]
                    }
                }
            ]
        });
        let warns = detect_quota_depletion_warnings(&snap);
        assert_eq!(warns.len(), 1);
        assert_eq!(warns[0].account_id, "hot-acct");
        assert!(warns[0].active);
        assert_eq!(warns[0].pool, "Gemini");
        let line = format_status_quota_warning_line(&warns).expect("warning line");
        assert!(line.contains("⚠ 额度预警"), "{line}");
        assert!(line.contains("hot-acct"), "{line}");
        assert!(line.contains("11 小时"), "{line}");
        assert!(line.contains("不自动换号，请手动切换"), "{line}");
    }

    #[test]
    fn sticky_session_keeps_pinned_slot_and_only_falls_back_to_active_when_unavailable() {
        let base = TempHome::create().unwrap();
        let real_home = base.path.join("home");
        let real_cfg = real_home.join(".config");
        let root = real_cfg.join("delegate/agy-accounts");
        fs::create_dir_all(real_home.join(".gemini/antigravity-cli")).unwrap();

        for (id, email, tok) in [
            ("alpha", "alpha@example.test", "ALPHA-V1"),
            ("beta", "beta@example.test", "BETA-V1"),
            ("gamma", "gamma@example.test", "GAMMA-V1"),
        ] {
            let sdir = slot_dir(&root, id);
            ensure_dir_mode(&sdir, 0o700).unwrap();
            let bytes = make_test_token_json(email, tok, "2026-11-26T20:00:00Z");
            write_secret_0600(&sdir.join("antigravity-cli-oauth-token"), &bytes, true).unwrap();
        }
        // Live ~/.gemini holds beta (current active).
        install_slot_to_home(&slot_dir(&root, "beta"), &real_home).unwrap();
        save_state(
            &root,
            &json!({
                "schemaVersion": SCHEMA_VERSION,
                "active": "beta",
                "accounts": {
                    "alpha": {"id": "alpha", "email": "alpha@example.test"},
                    "beta": {"id": "beta", "email": "beta@example.test"},
                    "gamma": {"id": "gamma", "email": "gamma@example.test"}
                }
            }),
        )
        .unwrap();

        let now_ep = iso_epoch("2026-11-26T16:00:00Z").unwrap();
        let empty_quota = json!({"accounts": {}});

        // 1. Pinned alpha stays on alpha even though active is beta; refresh writes back to alpha only.
        let prep = AgyCredOverlay::prepare_for_session_in(
            &root,
            &real_home,
            &real_cfg,
            Some("alpha"),
            "agy/gemini-4-neon",
            &empty_quota,
            now_ep,
        )
        .unwrap();
        assert_eq!(prep.account_id.as_deref(), Some("alpha"));
        assert!(prep.switched.is_none());
        let mut ov = prep.overlay.expect("overlay for alpha");
        let refreshed_alpha =
            make_test_token_json("alpha@example.test", "ALPHA-V2", "2026-11-26T22:00:00Z");
        write_secret_0600(
            &ov.overlay_home()
                .join(".gemini/antigravity-cli/antigravity-oauth-token"),
            &refreshed_alpha,
            false,
        )
        .unwrap();
        assert_eq!(ov.finish(), 1);
        let alpha_slot_doc: Value = serde_json::from_slice(
            &fs::read(slot_dir(&root, "alpha").join("antigravity-cli-oauth-token")).unwrap(),
        )
        .unwrap();
        assert_eq!(alpha_slot_doc["token"]["access_token"], "ALPHA-V2");
        let live_doc: Value = serde_json::from_slice(
            &fs::read(real_home.join(".gemini/antigravity-cli/antigravity-oauth-token")).unwrap(),
        )
        .unwrap();
        assert_eq!(live_doc["token"]["access_token"], "BETA-V1");

        // 2. Fresh session (pinned = None) uses active beta.
        let fresh = AgyCredOverlay::prepare_for_session_in(
            &root,
            &real_home,
            &real_cfg,
            None,
            "agy/gemini-4-neon",
            &empty_quota,
            now_ep,
        )
        .unwrap();
        assert_eq!(fresh.account_id.as_deref(), Some("beta"));
        assert!(fresh.switched.is_none());

        // 3. Quota exhausted on Gemini pool (resetAt in future) -> falls back to active beta, not gamma.
        let exhausted_quota = json!({
            "accounts": {
                "alpha": {
                    "ok": true,
                    "entries": [
                        {"pool": "Gemini Models", "window": "fiveHour", "remainingPct": 0.0, "resetAt": "2026-11-26T18:00:00Z"},
                        {"pool": "Claude and GPT models", "window": "fiveHour", "remainingPct": 85.0, "resetAt": "2026-11-26T18:00:00Z"}
                    ]
                }
            }
        });
        let fb_gemini = AgyCredOverlay::prepare_for_session_in(
            &root,
            &real_home,
            &real_cfg,
            Some("alpha"),
            "agy/gemini-4-neon",
            &exhausted_quota,
            now_ep,
        )
        .unwrap();
        assert_eq!(fb_gemini.account_id.as_deref(), Some("beta"));
        let sw = fb_gemini.switched.expect("should report switch");
        assert_eq!(sw.from, "alpha");
        assert_eq!(sw.to, "beta");
        assert_eq!(sw.reason, "额度已耗尽");

        // 4. Same quota cache, but Claude model -> Claude pool is 85%, so stays on alpha.
        let keep_claude = AgyCredOverlay::prepare_for_session_in(
            &root,
            &real_home,
            &real_cfg,
            Some("alpha"),
            "agy/claude-opus-4-6",
            &exhausted_quota,
            now_ep,
        )
        .unwrap();
        assert_eq!(keep_claude.account_id.as_deref(), Some("alpha"));
        assert!(keep_claude.switched.is_none());

        // 5. Past resetAt -> stale 0% is ignored, stays on alpha.
        let past_reset_quota = json!({
            "accounts": {
                "alpha": {
                    "ok": true,
                    "entries": [
                        {"pool": "Gemini Models", "window": "fiveHour", "remainingPct": 0.0, "resetAt": "2026-11-26T15:00:00Z"}
                    ]
                }
            }
        });
        let keep_past = AgyCredOverlay::prepare_for_session_in(
            &root,
            &real_home,
            &real_cfg,
            Some("alpha"),
            "agy/gemini-4-neon",
            &past_reset_quota,
            now_ep,
        )
        .unwrap();
        assert_eq!(keep_past.account_id.as_deref(), Some("alpha"));
        assert!(keep_past.switched.is_none());

        // 6. Active is STILL alpha while alpha is exhausted -> NEVER auto-rotates to beta or gamma.
        save_state(
            &root,
            &json!({
                "schemaVersion": SCHEMA_VERSION,
                "active": "alpha",
                "accounts": {
                    "alpha": {"id": "alpha", "email": "alpha@example.test"},
                    "beta": {"id": "beta", "email": "beta@example.test"},
                    "gamma": {"id": "gamma", "email": "gamma@example.test"}
                }
            }),
        )
        .unwrap();
        let no_auto_rotate = AgyCredOverlay::prepare_for_session_in(
            &root,
            &real_home,
            &real_cfg,
            Some("alpha"),
            "agy/gemini-4-neon",
            &exhausted_quota,
            now_ep,
        )
        .unwrap();
        assert_eq!(no_auto_rotate.account_id.as_deref(), Some("alpha"));
        assert!(no_auto_rotate.switched.is_none());

        // 7. Deleted pinned slot while active is beta -> falls back to active beta, never gamma.
        save_state(
            &root,
            &json!({
                "schemaVersion": SCHEMA_VERSION,
                "active": "beta",
                "accounts": {
                    "beta": {"id": "beta", "email": "beta@example.test"},
                    "gamma": {"id": "gamma", "email": "gamma@example.test"}
                }
            }),
        )
        .unwrap();
        let _ = fs::remove_dir_all(slot_dir(&root, "alpha"));
        let fb_deleted = AgyCredOverlay::prepare_for_session_in(
            &root,
            &real_home,
            &real_cfg,
            Some("alpha"),
            "agy/gemini-4-neon",
            &empty_quota,
            now_ep,
        )
        .unwrap();
        assert_eq!(fb_deleted.account_id.as_deref(), Some("beta"));
        let sw_del = fb_deleted.switched.expect("switch on deleted slot");
        assert_eq!(sw_del.from, "alpha");
        assert_eq!(sw_del.to, "beta");
        assert_eq!(sw_del.reason, "原槽位已删除");
    }
}
