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
use crate::config::{load_agy_auto_switch_config, AgyAutoSwitchConfig};
use crate::{has, parse_simple, value};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub const SCHEMA_VERSION: u64 = 1;
pub const AUTO_ACCOUNT_ID: &str = "auto";
pub const QUOTA_CACHE_SCHEMA_VERSION: u64 = 1;
const QUOTA_HISTORY_LIMIT: usize = 700;
const QUOTA_HISTORY_RETENTION_SECS: i64 = 7 * 86400;
const DEFAULT_QUOTA_TIMEOUT_SECS: u64 = 30;
pub const RESET_AT_BUFFER_SECS: i64 = 60;
pub const FIVE_HOUR_STALE_SECS: i64 = 2 * 3600;
pub const WEEKLY_STALE_SECS: i64 = 24 * 3600;
const ETA_WEEKLY_STALE_SECS: i64 = 12 * 3600;
const FIVE_HOUR_WINDOW_SECS: i64 = 5 * 3600;
const WEEKLY_WINDOW_SECS: i64 = 7 * 86400;
const SAME_CYCLE_TOLERANCE_SECS: i64 = 120;
pub const PASSIVE_SYNTH_RESET_SECS: i64 = 15 * 60;
pub const REFRESH_FAILURE_BACKOFF_SECS: i64 = 5 * 60;
pub const DEFAULT_MIN_REFRESH_SECS: i64 = 15 * 60;
pub const YELLOW_FIVE_HOUR_MIN_PCT: f64 = 5.0;
pub const YELLOW_WEEKLY_MIN_PCT: f64 = 2.0;
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
    let base = if base.is_empty() || base.eq_ignore_ascii_case(AUTO_ACCOUNT_ID) {
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
    snapshot_json_opts(RunningAgyScanMode::FullProc, None)
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

pub fn quota_cache_path_in(state_root: &Path) -> PathBuf {
    state_root.join("agy-quota.json")
}

/// Lock-free read of `<state>/agy-quota.json`.
pub fn load_quota_cache() -> Value {
    load_quota_cache_in(&state_dir())
}

pub fn load_quota_cache_in(state_root: &Path) -> Value {
    let v = json(quota_cache_path_in(state_root));
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

fn save_quota_cache_in(state_root: &Path, doc: &Value) -> Res<()> {
    let path = quota_cache_path_in(state_root);
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
    crate::common::strip_antigravity_env(&mut cmd);
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
        let reset_ep = (!e.reset_at.is_empty()).then(|| iso_epoch(&e.reset_at)).flatten();
        match e.window.as_str() {
            "weekly" => {
                min_weekly = Some(min_weekly.map_or(e.remaining_pct, |p| p.min(e.remaining_pct)));
                let entry = pools.entry(e.pool.clone()).or_default();
                entry.insert("weekly".into(), json!(e.remaining_pct));
                if let Some(rep) = reset_ep {
                    entry.insert("weeklyResetEp".into(), json!(rep));
                }
            }
            "fiveHour" => {
                min_five_hour =
                    Some(min_five_hour.map_or(e.remaining_pct, |p| p.min(e.remaining_pct)));
                let entry = pools.entry(e.pool.clone()).or_default();
                entry.insert("fiveHour".into(), json!(e.remaining_pct));
                if let Some(rep) = reset_ep {
                    entry.insert("fiveHourResetEp".into(), json!(rep));
                }
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

#[derive(Debug, Clone)]
pub struct QuotaRefreshOptions {
    pub force: bool,
    pub min_interval_secs: i64,
    pub failure_backoff_secs: i64,
    pub lock_wait_timeout: Duration,
    pub slot_filter: Option<Vec<String>>,
}

impl QuotaRefreshOptions {
    pub fn forced() -> Self {
        Self {
            force: true,
            min_interval_secs: 0,
            failure_backoff_secs: 0,
            lock_wait_timeout: Duration::from_secs(30),
            slot_filter: None,
        }
    }

    pub fn auto_stale(min_interval_secs: i64, slot_filter: Option<Vec<String>>) -> Self {
        Self {
            force: false,
            min_interval_secs: min_interval_secs.max(60),
            failure_backoff_secs: REFRESH_FAILURE_BACKOFF_SECS,
            lock_wait_timeout: Duration::from_secs(15),
            slot_filter,
        }
    }
}

struct QuotaRefreshLockGuard {
    file: File,
}

impl Drop for QuotaRefreshLockGuard {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn acquire_quota_refresh_lock(state_root: &Path, timeout: Duration) -> Option<QuotaRefreshLockGuard> {
    let _ = fs::create_dir_all(state_root);
    let path = state_root.join("agy-quota-refresh.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Some(QuotaRefreshLockGuard { file });
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn slot_needs_refresh(
    root: &Path,
    cache: &Value,
    id: &str,
    now_ep: i64,
    opts: &QuotaRefreshOptions,
) -> bool {
    if opts.force {
        return true;
    }
    if !slot_has_core_token(&slot_dir(root, id)) {
        return false;
    }
    let acct = &cache["accounts"][id];
    let fetched_ep = acct["fetchedEpoch"].as_i64().unwrap_or(0);
    let last_attempt_ep = acct["lastAttemptEpoch"].as_i64().unwrap_or(0);
    if last_attempt_ep > 0
        && !b(acct, "ok")
        && now_ep - last_attempt_ep < opts.failure_backoff_secs
    {
        return false;
    }
    if fetched_ep > 0 && now_ep - fetched_ep < opts.min_interval_secs {
        return false;
    }
    true
}

/// Core testable singleflight quota refresh:
/// - Skips lock & subprocesses if no target slot is stale (`!force`).
/// - Serializes concurrent callers via `<state>/agy-quota-refresh.lock` with double-check.
/// - Preserves prior entries & `fetchedEpoch` on transient/network failure (setting
///   `lastAttemptEpoch` for 5-minute failure backoff), and marks `authFailedEpoch` on 401.
fn refresh_quotas_singleflight_in<F, N>(
    root: &Path,
    state_root: &Path,
    opts: &QuotaRefreshOptions,
    now_fn: N,
    fetch_fn: F,
) -> Res<Value>
where
    F: Fn(&str, &str) -> SlotQuotaFetch + Sync,
    N: Fn() -> i64 + Sync,
{
    let state = load_state(root);
    let mut all_targets: Vec<(String, String)> = Vec::new();
    if let Some(map) = state["accounts"].as_object() {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for id in keys {
            if let Some(ref filter) = opts.slot_filter {
                if !filter.iter().any(|f| f == id) {
                    continue;
                }
            }
            let entry = &map[id];
            let email = s(entry, "email").to_string();
            all_targets.push((id.clone(), email));
        }
    }
    if all_targets.is_empty() {
        return Ok(load_quota_cache_in(state_root));
    }

    // Fast lock-free pre-check.
    let pre_cache = load_quota_cache_in(state_root);
    let pre_now = now_fn();
    if !opts.force
        && !all_targets
            .iter()
            .any(|(id, _)| slot_needs_refresh(root, &pre_cache, id, pre_now, opts))
    {
        return Ok(pre_cache);
    }

    let Some(_refresh_guard) = acquire_quota_refresh_lock(state_root, opts.lock_wait_timeout) else {
        // Lock busy past timeout -> degrade cleanly to existing cache.
        return Ok(load_quota_cache_in(state_root));
    };

    // Double-check after acquiring the singleflight lock.
    let mid_cache = load_quota_cache_in(state_root);
    let mid_now = now_fn();
    let targets: Vec<(String, String)> = all_targets
        .into_iter()
        .filter(|(id, _)| slot_needs_refresh(root, &mid_cache, id, mid_now, opts))
        .collect();
    if targets.is_empty() {
        return Ok(mid_cache);
    }

    let mut results: Vec<SlotQuotaFetch> = Vec::with_capacity(targets.len());
    thread::scope(|sc| {
        let handles: Vec<_> = targets
            .iter()
            .map(|(id, email)| {
                let id_str = id.as_str();
                let email_str = email.as_str();
                let fetch_ref = &fetch_fn;
                sc.spawn(move || fetch_ref(id_str, email_str))
            })
            .collect();
        for h in handles {
            if let Ok(r) = h.join() {
                results.push(r);
            }
        }
    });

    let now_ep = now_fn();
    let now_iso = format_epoch(now_ep, false, "%Y-%m-%dT%H:%M:%SZ");
    let cutoff = now_ep - QUOTA_HISTORY_RETENTION_SECS;

    let _lock = crate::locks::acquire_state(state_root).ok();
    let mut cache = load_quota_cache_in(state_root);
    if !cache["accounts"].is_object() {
        cache["accounts"] = json!({});
    }
    cache["schemaVersion"] = json!(QUOTA_CACHE_SCHEMA_VERSION);
    let any_ok = results.iter().any(|r| r.ok && !r.identity_mismatch);
    if any_ok || cache["updatedEpoch"].as_i64().unwrap_or(0) == 0 {
        cache["updatedAt"] = json!(now_iso);
        cache["updatedEpoch"] = json!(now_ep);
    }

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
                "lastAttemptEpoch": now_ep,
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

        if !res.ok {
            // Query failed (timeout, network error, 401, etc.):
            // preserve previous entries & `fetchedEpoch` and enter 5m failure backoff.
            let prev_entries = prev_acct["entries"].as_array().cloned().unwrap_or_default();
            let prev_unknown = prev_acct["unknownLines"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let prev_fetched_at = s(&prev_acct, "fetchedAt").to_string();
            let prev_fetched_ep = prev_acct["fetchedEpoch"].as_i64().unwrap_or(0);
            let err_msg = res.error.clone().unwrap_or_else(|| "quota refresh failed".into());
            let mut acct_doc = json!({
                "fetchedAt": prev_fetched_at,
                "fetchedEpoch": prev_fetched_ep,
                "lastAttemptEpoch": now_ep,
                "lastError": err_msg,
                "ok": false,
                "error": err_msg,
                "entries": prev_entries,
                "unknownLines": prev_unknown,
                "refreshedCreds": 0,
                "history": history,
            });
            if is_auth_failure_error(&err_msg) {
                acct_doc["authFailedAt"] = json!(now_iso);
                acct_doc["authFailedEpoch"] = json!(now_ep);
            } else if prev_acct["authFailedEpoch"].as_i64().unwrap_or(0) > 0 {
                acct_doc["authFailedAt"] = prev_acct["authFailedAt"].clone();
                acct_doc["authFailedEpoch"] = prev_acct["authFailedEpoch"].clone();
            }
            cache["accounts"][&res.id] = acct_doc;
            continue;
        }

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

        let acct_doc = json!({
            "fetchedAt": now_iso,
            "fetchedEpoch": now_ep,
            "lastAttemptEpoch": now_ep,
            "ok": true,
            "entries": entries_json,
            "unknownLines": res.parsed.unknown_lines,
            "refreshedCreds": res.refreshed_creds,
            "history": history,
        });
        cache["accounts"][&res.id] = acct_doc;
    }

    save_quota_cache_in(state_root, &cache)?;
    drop(_lock);
    crate::quota_rate::refresh_and_emit_in(state_root, now_ep);
    Ok(cache)
}

pub fn refresh_quotas_singleflight(
    agy_bin_opt: Option<&str>,
    opts: &QuotaRefreshOptions,
) -> Res<Value> {
    let root = accounts_root();
    let state_root = state_dir();
    // Avoid resolving `agy` binary when lock-free check shows cache is already fresh.
    if !opts.force {
        let pre_cache = load_quota_cache_in(&state_root);
        let state = load_state(&root);
        let now_ep = epoch() as i64;
        let any_stale = state["accounts"]
            .as_object()
            .into_iter()
            .flat_map(|m| m.keys())
            .filter(|id| {
                opts.slot_filter
                    .as_ref()
                    .is_none_or(|f| f.iter().any(|x| x == *id))
            })
            .any(|id| slot_needs_refresh(&root, &pre_cache, id, now_ep, opts));
        if !any_stale {
            return Ok(pre_cache);
        }
    }
    let agy_bin = resolve_agy_bin(agy_bin_opt)?;
    let real_home = home();
    let timeout_secs = quota_timeout_secs();
    refresh_quotas_singleflight_in(
        &root,
        &state_root,
        opts,
        || epoch() as i64,
        |id, email| fetch_quota_for_slot(&agy_bin, &real_home, &root, id, email, timeout_secs),
    )
}

/// Refresh real quota for all configured `agy` account slots via `agy -p /usage </dev/null`
/// (forced, used by manual `--quota` / TUI `Q`).
pub fn refresh_all_quotas(agy_bin_opt: Option<&str>) -> Res<Value> {
    refresh_quotas_singleflight(agy_bin_opt, &QuotaRefreshOptions::forced())
}

/// Singleflight refresh only for slots whose cached `fetchedEpoch` is older than `refreshMinutes`
/// (default 15m), used by TUI periodic refresh and new-session auto-switch.
pub fn refresh_stale_quotas(
    agy_bin_opt: Option<&str>,
    slot_filter: Option<Vec<String>>,
) -> Res<Value> {
    let cfg = load_agy_auto_switch_config();
    let min_secs = (cfg.refresh_minutes as i64 * 60).max(DEFAULT_MIN_REFRESH_SECS);
    refresh_quotas_singleflight(
        agy_bin_opt,
        &QuotaRefreshOptions::auto_stale(min_secs, slot_filter),
    )
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunningAgyScanMode {
    FullProc,
    KnownRunsOnly,
    Skip,
}

fn read_pid_file(path: &Path) -> Option<u32> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&p| p > 0)
}

/// Fast running-agy lookup for `delegate snapshot --json`: checks only active runs recorded in
/// `<state>/*.slot` + their recorded `pid` files (O(active runs), zero `/proc` directory scan).
pub fn detect_running_agy_from_known_runs(state_root: &Path) -> Vec<RunningAgy> {
    let mut found = Vec::new();
    let mut seen_pids = std::collections::HashSet::new();
    for run in crate::launch::machine_runs(state_root) {
        if run.join(".waiting").exists() {
            continue;
        }
        let meta = json(run.join("meta.json"));
        let model = s(&meta, "model");
        if !crate::agy::is_agy(model) && s(&meta, "agent") != crate::agy::BIN {
            continue;
        }
        let pid = read_pid_file(&run.join("agent.pid"))
            .or_else(|| read_pid_file(&run.join("pid")));
        let Some(pid) = pid else {
            continue;
        };
        if !crate::cleanup::alive(pid as i32) || !seen_pids.insert(pid) {
            continue;
        }
        let delegate_run = run
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let run_name = {
            let n = s(&meta, "name");
            (!n.is_empty()).then(|| n.to_string())
        };
        let account = {
            let a = s(&meta, "agyAccount").trim();
            (!a.is_empty()).then(|| a.to_string())
        };
        let cmdline = fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .map(|raw| {
                raw.split(|&b| b == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "agy (run {})",
                    delegate_run.as_deref().unwrap_or("unknown")
                )
            });
        found.push(RunningAgy {
            pid,
            delegate_run,
            run_name,
            role: Some("author".to_string()),
            round: None,
            account,
            cmdline,
        });
    }
    found.sort_by_key(|r| r.pid);
    found
}

/// Fast snapshot for `delegate snapshot --json`: never full-scans `/proc`; when local
/// `agy-accounts/accounts.json` exists, checks only recorded active run PIDs.
#[allow(dead_code)]
pub fn snapshot_json_fast() -> Value {
    snapshot_json_fast_with_usage(None)
}

pub fn snapshot_json_fast_with_usage(
    precollected_usage: Option<&crate::usage::PrecollectedUsage>,
) -> Value {
    let has_accounts = accounts_meta_path(&accounts_root()).is_file();
    let mode = if has_accounts {
        RunningAgyScanMode::KnownRunsOnly
    } else {
        RunningAgyScanMode::Skip
    };
    snapshot_json_opts(mode, precollected_usage)
}

/// Advisory-only quota depletion warning when projected depletion precedes window reset.
/// Only emitted for currently selected accounts/pools (in `auto` mode, only when all
/// available accounts in the pool are non-Green and combined capacity cannot reach the earliest reset).
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaDepletionWarning {
    pub account_id: String,
    pub active: bool,
    pub auto_all_low: bool,
    pub pool_summary: String,
    pub pool: String,
    pub window: String,
    pub window_label: String,
    pub fast_minutes: u64,
    pub remaining_minutes: u64,
    pub slow_minutes: Option<u64>,
    pub reset_in_seconds: u64,
    pub remaining_pct: f64,
    pub running_count: usize,
    pub basis: String,
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

/// Round ETA minutes to human-honest granularity (never overly precise like `3 小时 38 分钟`):
/// - `< 1h`: nearest 5 minutes (min 5m for non-zero)
/// - `1h..6h`: nearest 15 minutes
/// - `6h..48h`: nearest 1 hour
/// - `>= 48h`: nearest 1 day
pub fn round_eta_minutes(mins: u64) -> u64 {
    if mins == 0 {
        return 0;
    }
    if mins < 60 {
        (((mins + 2) / 5) * 5).max(5)
    } else if mins < 6 * 60 {
        ((mins + 7) / 15) * 15
    } else if mins < 48 * 60 {
        ((mins + 30) / 60) * 60
    } else {
        ((mins + 720) / 1440).max(2) * 1440
    }
}

fn format_hours_half_zh(mins: u64) -> Option<String> {
    if !(60..48 * 60).contains(&mins) || !mins.is_multiple_of(30) {
        return None;
    }
    let h = mins / 60;
    if mins.is_multiple_of(60) {
        Some(format!("{h}"))
    } else {
        Some(format!("{h}.5"))
    }
}

pub fn format_minutes_zh_short(mins: u64) -> String {
    if mins < 60 {
        return format!("{mins} 分钟");
    }
    let hours = mins / 60;
    let rem_m = mins % 60;
    if hours < 48 {
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

/// Format an ETA interval `[fast_m, mid_m, slow_m]` capped strictly below `reset_secs`:
/// - `mid_m == 0` -> `"即将用完"`
/// - wide interval (`(slow - fast) / mid > 25%`) -> `"约 1–1.5 小时后用完"` or `"约 1.5 小时后到重置前之间用完"`
/// - narrow interval -> rounded single value `"约 1 小时 30 分钟后用完"`
pub fn format_eta_zh_phrase(
    fast_m: u64,
    mid_m: u64,
    slow_m: Option<u64>,
    reset_secs: u64,
) -> String {
    let reset_mins = (reset_secs / 60).max(1);
    if mid_m == 0 || reset_mins <= 1 {
        return "即将用完".to_string();
    }
    let max_m = reset_mins.saturating_sub(1).max(1);
    let slow_eff = slow_m.unwrap_or(reset_mins);
    let is_wide = (slow_eff.saturating_sub(fast_m)) as f64 / (mid_m as f64) > 0.25;

    if is_wide {
        let fast_r = round_eta_minutes(fast_m).max(5).min(max_m).max(1);
        let slow_r_opt = slow_m
            .map(round_eta_minutes)
            .filter(|&sr| sr < reset_mins);
        match slow_r_opt {
            None => {
                if let Some(fh) = format_hours_half_zh(fast_r) {
                    format!("约 {fh} 小时后到重置前之间用完")
                } else {
                    format!("约 {}后到重置前之间用完", format_minutes_zh_short(fast_r))
                }
            }
            Some(slow_r) if slow_r <= fast_r => {
                format!("约 {}后用完", format_minutes_zh_short(fast_r))
            }
            Some(slow_r) => {
                if fast_r < 60 && slow_r < 60 {
                    format!("约 {fast_r}–{slow_r} 分钟后用完")
                } else if let (Some(fh), Some(sh)) =
                    (format_hours_half_zh(fast_r), format_hours_half_zh(slow_r))
                {
                    format!("约 {fh}–{sh} 小时后用完")
                } else if fast_r >= 48 * 60 && slow_r >= 48 * 60 && fast_r.is_multiple_of(1440) && slow_r.is_multiple_of(1440) {
                    format!("约 {}–{} 天后用完", fast_r / 1440, slow_r / 1440)
                } else {
                    format!(
                        "约 {}–{}后用完",
                        format_minutes_zh_short(fast_r),
                        format_minutes_zh_short(slow_r)
                    )
                }
            }
        }
    } else {
        let mid_r = round_eta_minutes(mid_m).min(max_m).max(1);
        format!("约 {}后用完", format_minutes_zh_short(mid_r))
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

fn find_acct_pool_window_entry(
    acct: &Value,
    window_key: &str,
    pool_opt: Option<&str>,
) -> Option<(u64, f64)> {
    find_acct_pool_window_entry_meta(acct, window_key, pool_opt).map(|(rs, pct, _)| (rs, pct))
}

fn find_acct_pool_window_entry_meta(
    acct: &Value,
    window_key: &str,
    pool_opt: Option<&str>,
) -> Option<(u64, f64, bool)> {
    let entries = acct["quota"]["entries"].as_array()?;
    let target_pool = pool_opt.and_then(classify_quota_pool_name);
    for e in entries {
        if s(e, "window") != window_key {
            continue;
        }
        if let Some(tp) = target_pool {
            if classify_quota_pool_name(s(e, "pool")) != Some(tp) {
                continue;
            }
        } else if let Some(p) = pool_opt.filter(|p| !p.is_empty()) {
            if s(e, "pool") != p {
                continue;
            }
        }
        if let Some(reset_s) = e["resetInSeconds"].as_u64() {
            if reset_s > 0 {
                let pct = e["remainingPct"].as_f64().unwrap_or(0.0);
                let is_passive = s(e, "source") == "passive_error";
                return Some((reset_s, pct, is_passive));
            }
        }
    }
    None
}

fn acct_pool_tier(acct: &Value, pool: QuotaPool) -> Option<String> {
    if let Some(pools) = acct["quotaEstimate"]["pools"].as_array() {
        for pe in pools {
            if classify_quota_pool_name(s(pe, "pool")) == Some(pool) {
                let t = s(pe, "tier");
                if !t.is_empty() {
                    return Some(t.to_string());
                }
                for wk in ["fiveHour", "weekly"] {
                    let wt = s(&pe[wk], "tier");
                    if !wt.is_empty() {
                        return Some(wt.to_string());
                    }
                }
            }
        }
    }
    None
}

struct ExtractedWindowAlert {
    pool_raw: String,
    fast_m: u64,
    mid_m: u64,
    slow_m: Option<u64>,
    reset_secs: u64,
    rem_pct: f64,
    running_count: usize,
    basis: String,
}

fn extract_window_alert_from_obj(
    acct: &Value,
    est_obj: &Value,
    window_key: &str,
    pool_hint: Option<&str>,
) -> Option<ExtractedWindowAlert> {
    if !est_obj.is_object() {
        return None;
    }
    if s(est_obj, "status") != "depletes" || !b(est_obj, "alert") {
        return None;
    }
    let mid_m = est_obj["midMinutes"].as_u64()?;
    let fast_m = est_obj["fastMinutes"].as_u64().unwrap_or(mid_m).min(mid_m);
    let slow_m = est_obj["slowMinutes"].as_u64();
    let pool_raw = pool_hint
        .filter(|p| !p.is_empty())
        .or_else(|| {
            let p = s(est_obj, "pool");
            (!p.is_empty()).then_some(p)
        })
        .unwrap_or("");
    let (reset_secs, rem_pct) = find_acct_pool_window_entry(acct, window_key, Some(pool_raw))
        .or_else(|| {
            let rs = est_obj["resetInSeconds"].as_u64().filter(|&s| s > 0)?;
            let pct = est_obj["remainingPct"].as_f64().unwrap_or(0.0);
            Some((rs, pct))
        })?;
    if mid_m.saturating_mul(60) >= reset_secs {
        return None;
    }
    let running_count = est_obj["runningCount"].as_u64().unwrap_or(0) as usize;
    let basis = s(est_obj, "basis").to_string();
    Some(ExtractedWindowAlert {
        pool_raw: pool_raw.to_string(),
        fast_m,
        mid_m,
        slow_m,
        reset_secs,
        rem_pct,
        running_count,
        basis,
    })
}

/// Extract quota depletion-before-reset warnings from an `agyAccounts` snapshot JSON object.
/// Only alerts on currently selected accounts & pools:
/// - Fixed mode (`mode != "auto"`): checks ONLY the active account (standby accounts never alert).
/// - `auto` mode (`mode == "auto"`): checks each pool's selected account; suppresses warning if
///   another healthy account in that pool is Green or resets before current depletion; warns only
///   when all available accounts in the pool are non-Green and combined capacity cannot reach
///   the earliest `resetAt`.
pub fn detect_quota_depletion_warnings(snap: &Value) -> Vec<QuotaDepletionWarning> {
    let mut warnings = Vec::new();
    let Some(accounts) = snap["accounts"].as_array() else {
        return warnings;
    };
    let is_auto = s(snap, "mode") == AUTO_ACCOUNT_ID || b(&snap["autoSwitch"], "enabled");
    let snap_active = s(snap, "active");

    if !is_auto {
        for acct in accounts {
            let id = s(acct, "id").to_string();
            if id.is_empty() {
                continue;
            }
            let active = b(acct, "active") || (!snap_active.is_empty() && id == snap_active);
            if !active {
                continue;
            }
            let quota = &acct["quota"];
            if !b(quota, "ok") || b(quota, "identityMismatch") {
                continue;
            }
            let qest = &acct["quotaEstimate"];
            if !qest.is_object() {
                continue;
            }
            let mut seen: std::collections::HashSet<(String, String)> =
                std::collections::HashSet::new();
            let mut push_win = |est_obj: &Value, window_key: &str, pool_hint: Option<&str>| {
                let Some(ExtractedWindowAlert {
                    pool_raw,
                    fast_m,
                    mid_m,
                    slow_m,
                    reset_secs,
                    rem_pct,
                    running_count,
                    basis,
                }) = extract_window_alert_from_obj(acct, est_obj, window_key, pool_hint)
                else {
                    return;
                };
                let pool_disp = short_pool_label(&pool_raw);
                if !seen.insert((pool_disp.clone(), window_key.to_string())) {
                    return;
                }
                let window_label = match window_key {
                    "fiveHour" => "5 小时".to_string(),
                    _ => "本周".to_string(),
                };
                warnings.push(QuotaDepletionWarning {
                    account_id: id.clone(),
                    active: true,
                    auto_all_low: false,
                    pool_summary: String::new(),
                    pool: pool_disp,
                    window: window_key.to_string(),
                    window_label,
                    fast_minutes: fast_m,
                    remaining_minutes: mid_m,
                    slow_minutes: slow_m,
                    reset_in_seconds: reset_secs,
                    remaining_pct: rem_pct,
                    running_count,
                    basis,
                });
            };
            if let Some(pools) = qest["pools"].as_array() {
                for pe in pools {
                    let pname = s(pe, "pool");
                    push_win(&pe["fiveHour"], "fiveHour", Some(pname));
                    push_win(&pe["weekly"], "weekly", Some(pname));
                }
            }
            push_win(&qest["fiveHour"], "fiveHour", None);
            push_win(&qest["weekly"], "weekly", None);
        }
    } else {
        // `auto` mode: evaluate per pool.
        let healthy_accts: Vec<&Value> = accounts
            .iter()
            .filter(|a| {
                !s(a, "id").is_empty()
                    && b(&a["quota"], "ok")
                    && !b(&a["quota"], "identityMismatch")
            })
            .collect();

        for pool in QuotaPool::ALL {
            let pkey = pool.key();
            let sel_id = {
                let ps = s(&snap["autoSwitch"]["poolSummary"][pkey], "selected");
                if !ps.is_empty() {
                    ps.to_string()
                } else {
                    let pc = s(&snap["autoSwitch"]["poolCurrent"], pkey);
                    if !pc.is_empty() {
                        pc.to_string()
                    } else {
                        snap_active.to_string()
                    }
                }
            };
            let Some(sel_acct) = healthy_accts
                .iter()
                .copied()
                .find(|a| s(a, "id") == sel_id)
                .or_else(|| healthy_accts.first().copied())
            else {
                continue;
            };
            let sel_acct_id = s(sel_acct, "id").to_string();

            for window_key in ["fiveHour", "weekly"] {
                // Find the selected account's estimate for this (pool, window).
                let est_obj = sel_acct["quotaEstimate"]["pools"]
                    .as_array()
                    .and_then(|arr| {
                        arr.iter()
                            .find(|pe| classify_quota_pool_name(s(pe, "pool")) == Some(pool))
                            .map(|pe| &pe[window_key])
                    })
                    .unwrap_or(&Value::Null);
                let Some(ExtractedWindowAlert {
                    fast_m,
                    mid_m,
                    slow_m,
                    reset_secs: cur_reset_secs,
                    rem_pct: cur_pct,
                    running_count,
                    basis,
                    ..
                }) = extract_window_alert_from_obj(
                    sel_acct,
                    est_obj,
                    window_key,
                    Some(pool.label()),
                )
                else {
                    continue;
                };

                // Suppress if ANY other healthy account in this pool is Green or resets before current depletion
                // (ignoring synthetic `passive_error` resets and accounts whose weekly pool is already Red).
                let other_can_takeover = healthy_accts.iter().any(|other| {
                    if s(other, "id") == sel_acct_id {
                        return false;
                    }
                    if acct_pool_tier(other, pool).as_deref() == Some("green") {
                        return true;
                    }
                    if let Some((other_reset_s, _, is_passive)) =
                        find_acct_pool_window_entry_meta(other, window_key, Some(pool.label()))
                    {
                        let weekly_blocked = window_key == "fiveHour"
                            && find_acct_pool_window_entry(other, "weekly", Some(pool.label()))
                                .is_some_and(|(_, wpct)| wpct < YELLOW_WEEKLY_MIN_PCT);
                        if !is_passive && !weekly_blocked && other_reset_s <= mid_m.saturating_mul(60) {
                            return true;
                        }
                    }
                    false
                });
                if other_can_takeover {
                    continue;
                }

                // All accounts in this pool are non-Green (or single account): check combined capacity vs earliest reset.
                let mut earliest_reset = cur_reset_secs;
                let mut combined_pct = cur_pct.max(0.0);
                let mut parts: Vec<String> = Vec::new();
                let win_short = if window_key == "fiveHour" { "5h" } else { "周" };
                for a in &healthy_accts {
                    let aid = s(a, "id");
                    if let Some((rs, pct, is_passive)) =
                        find_acct_pool_window_entry_meta(a, window_key, Some(pool.label()))
                    {
                        let weekly_blocked = window_key == "fiveHour"
                            && find_acct_pool_window_entry(a, "weekly", Some(pool.label()))
                                .is_some_and(|(_, wpct)| wpct < YELLOW_WEEKLY_MIN_PCT);
                        if !is_passive && !weekly_blocked && rs > 0 && rs < earliest_reset {
                            earliest_reset = rs;
                        }
                        if aid != sel_acct_id && !weekly_blocked && !is_passive {
                            combined_pct += pct.max(0.0);
                        }
                        parts.push(format!("{aid} {win_short} {:.0}%", pct));
                    }
                }
                let scale = if cur_pct > 0.5 {
                    (combined_pct / cur_pct).max(1.0)
                } else {
                    1.0
                };
                let combined_mid_m = ((mid_m as f64) * scale).round() as u64;
                if combined_mid_m.saturating_mul(60) >= earliest_reset {
                    continue;
                }
                let combined_fast_m = ((fast_m as f64) * scale).round() as u64;
                let combined_slow_m = slow_m
                    .map(|sm| ((sm as f64) * scale).round() as u64)
                    .filter(|&sm| sm.saturating_mul(60) < earliest_reset);

                let window_label = match window_key {
                    "fiveHour" => "5 小时".to_string(),
                    _ => "本周".to_string(),
                };
                warnings.push(QuotaDepletionWarning {
                    account_id: sel_acct_id.clone(),
                    active: true,
                    auto_all_low: healthy_accts.len() > 1,
                    pool_summary: parts.join(" / "),
                    pool: pool.short_label().to_string(),
                    window: window_key.to_string(),
                    window_label,
                    fast_minutes: combined_fast_m.min(combined_mid_m),
                    remaining_minutes: combined_mid_m,
                    slow_minutes: combined_slow_m,
                    reset_in_seconds: earliest_reset,
                    remaining_pct: cur_pct,
                    running_count,
                    basis,
                });
            }
        }
    }

    warnings.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then_with(|| a.remaining_minutes.cmp(&b.remaining_minutes))
            .then_with(|| a.account_id.cmp(&b.account_id))
    });
    warnings
}

fn eta_window_priority(est: &Value) -> (u8, u64) {
    match s(est, "status") {
        "depletes" if b(est, "alert") => (0, est["midMinutes"].as_u64().unwrap_or(u64::MAX)),
        "depletes" => (1, est["midMinutes"].as_u64().unwrap_or(u64::MAX)),
        "insufficient" if s(est, "reason") == "too_early" => (2, 0),
        "safe" => (
            3,
            (est["remainingPct"].as_f64().unwrap_or(100.0) * 100.0).round() as u64,
        ),
        "idle" => (
            4,
            (est["remainingPct"].as_f64().unwrap_or(100.0) * 100.0).round() as u64,
        ),
        _ => (5, 0),
    }
}

fn build_account_quota_estimate_json(
    acct_quota: &Value,
    cfg: &AgyAutoSwitchConfig,
    running_tasks: &BTreeMap<(String, QuotaPool), RunningPoolCounts>,
    acct_id: &str,
    selected_by_pool: &BTreeMap<QuotaPool, String>,
    now_ep: i64,
) -> Value {
    if !b(acct_quota, "ok") || b(acct_quota, "identityMismatch") {
        return Value::Null;
    }
    let mut pool_names: Vec<(String, QuotaPool)> = Vec::new();
    if let Some(arr) = acct_quota["entries"].as_array() {
        for e in arr {
            let pname = s(e, "pool");
            if pname.is_empty() {
                continue;
            }
            if let Some(pool) = classify_quota_pool_name(pname) {
                if !pool_names.iter().any(|(_, p)| *p == pool) {
                    pool_names.push((pname.to_string(), pool));
                }
            }
        }
    }
    if pool_names.is_empty() {
        return Value::Null;
    }

    let mut pool_estimates: Vec<Value> = Vec::new();
    let mut best_five_hour: Option<((u8, u64), Value)> = None;
    let mut best_weekly: Option<((u8, u64), Value)> = None;

    for (pname, pool) in &pool_names {
        let counts = running_tasks
            .get(&(acct_id.to_string(), *pool))
            .copied()
            .unwrap_or_default();
        let is_selected = selected_by_pool
            .get(pool)
            .is_some_and(|sid| sid == acct_id);
        let st = evaluate_pool_quota(
            acct_quota,
            *pool,
            cfg,
            counts.writers,
            counts.total,
            is_selected,
            now_ep,
        );
        let pfh = st.window_eta_json(pname, "fiveHour");
        let pw = st.window_eta_json(pname, "weekly");
        if let Some(ref fhv) = pfh {
            let prio = eta_window_priority(fhv);
            if best_five_hour.as_ref().is_none_or(|(bp, _)| prio < *bp) {
                best_five_hour = Some((prio, fhv.clone()));
            }
        }
        if let Some(ref wv) = pw {
            let prio = eta_window_priority(wv);
            if best_weekly.as_ref().is_none_or(|(bp, _)| prio < *bp) {
                best_weekly = Some((prio, wv.clone()));
            }
        }
        if pfh.is_some() || pw.is_some() {
            let mut pe = json!({
                "pool": pname,
                "tier": st.tier.as_str(),
            });
            if let Some(fh) = pfh {
                pe["fiveHour"] = fh;
            }
            if let Some(w) = pw {
                pe["weekly"] = w;
            }
            pool_estimates.push(pe);
        }
    }

    if best_five_hour.is_none() && best_weekly.is_none() && pool_estimates.is_empty() {
        return Value::Null;
    }
    let mut obj = Map::new();
    if let Some((_, fh)) = best_five_hour {
        obj.insert("fiveHour".into(), fh);
    }
    if let Some((_, w)) = best_weekly {
        obj.insert("weekly".into(), w);
    }
    if !pool_estimates.is_empty() {
        obj.insert("pools".into(), Value::Array(pool_estimates));
    }
    Value::Object(obj)
}

/// Build a lightweight lock-free accounts/quota snapshot for advisory alerts in `state`
/// without scanning `/proc` (isolated-state safe for unit/integration tests).
pub fn quota_snapshot_for_alerts_in(state: &Path, now_ep: i64) -> Option<Value> {
    let qpath = state.join("agy-quota.json");
    let root = accounts_root();
    let use_real_accounts = state == state_dir() && accounts_meta_path(&root).is_file();
    if use_real_accounts {
        return Some(snapshot_json_opts(RunningAgyScanMode::Skip, None));
    }
    if !qpath.is_file() {
        return None;
    }
    let quota_cache = json(&qpath);
    if !quota_cache.is_object() {
        return None;
    }
    // Isolated state dir (e.g. unit/integration tests): build synthetic snapshot purely from `state`.
    let mut acct_ids: Vec<String> = Vec::new();
    let mut active_id = String::new();
    if let Some(accts) = quota_cache["accounts"].as_object() {
        for (id, v) in accts {
            if b(v, "active") && active_id.is_empty() {
                active_id = id.clone();
            }
            acct_ids.push(id.clone());
        }
    }
    if acct_ids.is_empty() {
        return None;
    }
    acct_ids.sort();
    if active_id.is_empty() {
        active_id = acct_ids[0].clone();
    }
    let cfg = AgyAutoSwitchConfig::default();
    let running_tasks = count_running_pool_tasks(state, None);
    let mut selected_by_pool = BTreeMap::new();
    for pool in QuotaPool::ALL {
        selected_by_pool.insert(pool, active_id.clone());
    }
    let mut items = Vec::new();
    for id in &acct_ids {
        let mut item = json!({
            "id": id,
            "active": id == &active_id,
        });
        if let Some(q) = enrich_quota_for_snapshot(&quota_cache["accounts"][id], now_ep) {
            item["quota"] = q;
        }
        let qest = build_account_quota_estimate_json(
            &quota_cache["accounts"][id],
            &cfg,
            &running_tasks,
            id,
            &selected_by_pool,
            now_ep,
        );
        if !qest.is_null() {
            item["quotaEstimate"] = qest;
        }
        items.push(item);
    }
    let mut snap = json!({ "active": active_id, "mode": "fixed", "accounts": items });
    let rate_shifts = crate::quota_rate::load_cached_shifts_in(state, now_ep);
    if !rate_shifts.is_empty() {
        snap["rateShifts"] = Value::Array(rate_shifts);
    }
    Some(snap)
}

/// Maximum quota cache age (seconds) for shadow-mode headroom checks; older caches are treated as unknown.
pub const SHADOW_HEADROOM_MAX_CACHE_AGE_SECS: i64 = 20 * 60;
const MEDIAN_WRITE_LOOKBACK_SECS: i64 = 14 * 86400;
const SHADOW_DEFAULT_WINDOW_MINS: u64 = 120;
const SHADOW_MIN_WINDOW_MINS: u64 = 30;
const SHADOW_MAX_WINDOW_MINS: u64 = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadroomUnknownReason {
    NoSnapshot,
    NoAccount,
    StaleCache,
    NoEstimate,
    NoWriteMedian,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadroomDecision {
    pub pool: QuotaPool,
    pub account_id: String,
    pub window: String,
    pub remaining_minutes: u64,
    pub required_minutes: u64,
    pub median_write_minutes: u64,
    pub active_writes: usize,
    pub cache_age_seconds: i64,
}

impl HeadroomDecision {
    pub fn event_payload(
        &self,
        source: &str,
        work_id: Option<&str>,
        queue_name: Option<&str>,
    ) -> Value {
        let mut obj = json!({
            "pool": self.pool.key(),
            "poolLabel": self.pool.short_label(),
            "account": self.account_id,
            "window": self.window,
            "remainingMinutes": self.remaining_minutes,
            "requiredMinutes": self.required_minutes,
            "medianWriteMinutes": self.median_write_minutes,
            "activeWrites": self.active_writes,
            "cacheAgeSeconds": self.cache_age_seconds,
            "source": source,
        });
        if let Some(wid) = work_id.filter(|s| !s.is_empty()) {
            obj["workId"] = json!(wid);
        }
        if let Some(qn) = queue_name.filter(|s| !s.is_empty()) {
            obj["queueName"] = json!(qn);
        }
        obj
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadroomCheck {
    Fits,
    WouldHold(HeadroomDecision),
    Unknown(HeadroomUnknownReason),
}

/// Whether `start_args` describe a read-only task (`--read-only` or `--tier scout|review`).
pub fn is_readonly_start_args(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--read-only" {
            return true;
        }
        if let Some(val) = a.strip_prefix("--tier=") {
            if matches!(val, "scout" | "review") {
                return true;
            }
        } else if a == "--tier" {
            if let Some(val) = args.get(i + 1) {
                if matches!(val.as_str(), "scout" | "review") {
                    return true;
                }
            }
            i += 1;
        }
        i += 1;
    }
    false
}

/// Zero-subprocess resolution of target `agy` quota pool from `start_args` and optional `repo`.
/// Never invokes `pi --list-models` or any child process.
pub fn resolve_start_args_quota_pool(
    start_args: &[String],
    repo: Option<&Path>,
) -> Option<QuotaPool> {
    let mut model_arg: Option<&str> = None;
    let mut tier_arg: Option<&str> = None;
    let mut i = 0;
    while i < start_args.len() {
        let a = start_args[i].as_str();
        if let Some(val) = a.strip_prefix("--model=") {
            if !val.is_empty() {
                model_arg = Some(val);
            }
        } else if a == "--model" {
            if let Some(val) = start_args.get(i + 1) {
                if !val.is_empty() {
                    model_arg = Some(val.as_str());
                }
            }
            i += 1;
        } else if let Some(val) = a.strip_prefix("--tier=") {
            if !val.is_empty() {
                tier_arg = Some(val);
            }
        } else if a == "--tier" {
            if let Some(val) = start_args.get(i + 1) {
                if !val.is_empty() {
                    tier_arg = Some(val.as_str());
                }
            }
            i += 1;
        }
        i += 1;
    }

    let cfg = crate::config::load(repo).ok()?.value;
    let resolved_model = if let Some(m) = model_arg {
        crate::models::resolve_model(m, &cfg, &[])
            .ok()
            .or_else(|| crate::agy::is_agy(m).then(|| m.to_string()))?
    } else {
        let tier = tier_arg.unwrap_or("write");
        crate::models::pick(
            &cfg,
            tier,
            None,
            &[],
            crate::agents::local_clock(),
            &|_| 0,
        )
        .ok()
        .map(|p| p.model)
        .or_else(|| {
            crate::models::tier_rules(&cfg, tier)
                .first()
                .and_then(|r| {
                    let name = r.as_str().unwrap_or_else(|| s(r, "model"));
                    (!name.is_empty()).then_some(name)
                })
                .and_then(|name| crate::models::resolve_model(name, &cfg, &[]).ok())
        })?
    };

    if !crate::agy::is_agy(&resolved_model) {
        return None;
    }
    classify_model_pool(&resolved_model)
}

/// Compute the median write-task duration (in minutes, min 1) over the past 14 days from
/// `<state>/history/*/history.jsonl`. Returns `None` when there are no write samples.
pub fn median_write_duration_mins_in(state: &Path, now_ep: i64) -> Option<u64> {
    let cutoff = now_ep.saturating_sub(MEDIAN_WRITE_LOOKBACK_SECS);
    let records = crate::history::read_recent_window_all_repos(state, cutoff);
    let mut secs: Vec<i64> = records
        .values()
        .filter(|rec| {
            let ts = if rec.finished_epoch > 0 {
                rec.finished_epoch
            } else {
                rec.started_epoch
            };
            rec.mode == "write"
                && rec.elapsed_seconds > 0
                && ts >= cutoff
                && matches!(rec.state.as_str(), "delivered" | "failed")
        })
        .map(|rec| rec.elapsed_seconds)
        .collect();
    if secs.is_empty() {
        return None;
    }
    secs.sort_unstable();
    let mid_secs = if secs.len() % 2 == 1 {
        secs[secs.len() / 2]
    } else {
        (secs[secs.len() / 2 - 1] + secs[secs.len() / 2]) / 2
    };
    Some(((mid_secs + 30) / 60).max(1) as u64)
}

fn quota_cache_age_secs(acct: &Value, now_ep: i64) -> Option<i64> {
    let q = &acct["quota"];
    if !q.is_object() {
        return None;
    }
    if let Some(fep) = q["fetchedEpoch"].as_i64().filter(|&e| e > 0) {
        return Some((now_ep - fep).max(0));
    }
    q["ageSeconds"].as_i64().filter(|&a| a >= 0)
}

fn find_pool_window_estimate<'a>(
    acct: &'a Value,
    pool: QuotaPool,
    window_key: &str,
) -> &'a Value {
    acct["quotaEstimate"]["pools"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|pe| classify_quota_pool_name(s(pe, "pool")) == Some(pool))
                .map(|pe| &pe[window_key])
        })
        .unwrap_or(&Value::Null)
}

/// Pure, zero-subprocess check of whether `pool` has enough estimated remaining time
/// to fit `active_writes + 1` write tasks of duration `median_write_mins`.
pub fn check_write_quota_headroom_from_snapshot(
    snap: &Value,
    pool: QuotaPool,
    running_writers: &BTreeMap<(String, QuotaPool), usize>,
    median_write_mins: Option<u64>,
    now_ep: i64,
) -> HeadroomCheck {
    let Some(accounts) = snap["accounts"].as_array() else {
        return HeadroomCheck::Unknown(HeadroomUnknownReason::NoSnapshot);
    };
    let is_auto = s(snap, "mode") == AUTO_ACCOUNT_ID || b(&snap["autoSwitch"], "enabled");
    let snap_active = s(snap, "active");

    if !is_auto {
        let Some(acct) = accounts.iter().find(|a| {
            let id = s(a, "id");
            !id.is_empty() && (b(a, "active") || (!snap_active.is_empty() && id == snap_active))
        }) else {
            return HeadroomCheck::Unknown(HeadroomUnknownReason::NoAccount);
        };
        if !b(&acct["quota"], "ok") || b(&acct["quota"], "identityMismatch") {
            return HeadroomCheck::Unknown(HeadroomUnknownReason::NoAccount);
        }
        let Some(cache_age_secs) = quota_cache_age_secs(acct, now_ep) else {
            return HeadroomCheck::Unknown(HeadroomUnknownReason::StaleCache);
        };
        if cache_age_secs > SHADOW_HEADROOM_MAX_CACHE_AGE_SECS {
            return HeadroomCheck::Unknown(HeadroomUnknownReason::StaleCache);
        }
        let Some(median_mins) = median_write_mins.filter(|&m| m > 0) else {
            return HeadroomCheck::Unknown(HeadroomUnknownReason::NoWriteMedian);
        };
        let acct_id = s(acct, "id").to_string();
        let active_writes = running_writers
            .get(&(acct_id.clone(), pool))
            .copied()
            .unwrap_or(0);
        let required_mins = median_mins.saturating_mul(active_writes as u64 + 1);

        let mut tightest: Option<(&'static str, u64)> = None;
        let mut saw_known_ok = false;
        for window_key in ["fiveHour", "weekly"] {
            let est = find_pool_window_estimate(acct, pool, window_key);
            if !est.is_object() {
                continue;
            }
            match s(est, "status") {
                "depletes" => {
                    if let Some(mid_m) = est["midMinutes"].as_u64() {
                        if tightest.is_none_or(|(_, prev)| mid_m < prev) {
                            tightest = Some((window_key, mid_m));
                        }
                    }
                }
                "safe" | "idle" => {
                    saw_known_ok = true;
                }
                _ => {}
            }
        }
        match tightest {
            Some((win, rem_m)) if rem_m < required_mins => {
                HeadroomCheck::WouldHold(HeadroomDecision {
                    pool,
                    account_id: acct_id,
                    window: win.to_string(),
                    remaining_minutes: rem_m,
                    required_minutes: required_mins,
                    median_write_minutes: median_mins,
                    active_writes,
                    cache_age_seconds: cache_age_secs,
                })
            }
            Some(_) => HeadroomCheck::Fits,
            None if saw_known_ok => HeadroomCheck::Fits,
            None => HeadroomCheck::Unknown(HeadroomUnknownReason::NoEstimate),
        }
    } else {
        let healthy_accts: Vec<&Value> = accounts
            .iter()
            .filter(|a| {
                !s(a, "id").is_empty()
                    && b(&a["quota"], "ok")
                    && !b(&a["quota"], "identityMismatch")
            })
            .collect();
        let pkey = pool.key();
        let sel_id = {
            let ps = s(&snap["autoSwitch"]["poolSummary"][pkey], "selected");
            if !ps.is_empty() {
                ps.to_string()
            } else {
                let pc = s(&snap["autoSwitch"]["poolCurrent"], pkey);
                if !pc.is_empty() {
                    pc.to_string()
                } else {
                    snap_active.to_string()
                }
            }
        };
        let Some(sel_acct) = healthy_accts
            .iter()
            .copied()
            .find(|a| s(a, "id") == sel_id)
            .or_else(|| healthy_accts.first().copied())
        else {
            return HeadroomCheck::Unknown(HeadroomUnknownReason::NoAccount);
        };
        let Some(cache_age_secs) = quota_cache_age_secs(sel_acct, now_ep) else {
            return HeadroomCheck::Unknown(HeadroomUnknownReason::StaleCache);
        };
        if cache_age_secs > SHADOW_HEADROOM_MAX_CACHE_AGE_SECS {
            return HeadroomCheck::Unknown(HeadroomUnknownReason::StaleCache);
        }
        let Some(median_mins) = median_write_mins.filter(|&m| m > 0) else {
            return HeadroomCheck::Unknown(HeadroomUnknownReason::NoWriteMedian);
        };
        let sel_acct_id = s(sel_acct, "id").to_string();
        let active_writes: usize = healthy_accts
            .iter()
            .map(|a| {
                running_writers
                    .get(&(s(a, "id").to_string(), pool))
                    .copied()
                    .unwrap_or(0)
            })
            .sum();
        let required_mins = median_mins.saturating_mul(active_writes as u64 + 1);

        let mut tightest: Option<(&'static str, u64)> = None;
        let mut saw_known_ok = false;

        for window_key in ["fiveHour", "weekly"] {
            let est = find_pool_window_estimate(sel_acct, pool, window_key);
            if !est.is_object() {
                continue;
            }
            match s(est, "status") {
                "safe" | "idle" => {
                    saw_known_ok = true;
                }
                "depletes" => {
                    let Some(mid_m) = est["midMinutes"].as_u64() else {
                        continue;
                    };
                    let other_can_takeover = healthy_accts.iter().any(|other| {
                        if s(other, "id") == sel_acct_id {
                            return false;
                        }
                        if acct_pool_tier(other, pool).as_deref() == Some("green") {
                            return true;
                        }
                        if let Some((other_reset_s, _, is_passive)) =
                            find_acct_pool_window_entry_meta(other, window_key, Some(pool.label()))
                        {
                            let weekly_blocked = window_key == "fiveHour"
                                && find_acct_pool_window_entry(other, "weekly", Some(pool.label()))
                                    .is_some_and(|(_, wpct)| wpct < YELLOW_WEEKLY_MIN_PCT);
                            if !is_passive
                                && !weekly_blocked
                                && other_reset_s <= mid_m.saturating_mul(60)
                            {
                                return true;
                            }
                        }
                        false
                    });
                    if other_can_takeover {
                        saw_known_ok = true;
                        continue;
                    }

                    let (cur_reset_secs, cur_pct) =
                        find_acct_pool_window_entry(sel_acct, window_key, Some(pool.label()))
                            .unwrap_or_else(|| {
                                (
                                    est["resetInSeconds"].as_u64().unwrap_or(0),
                                    est["remainingPct"].as_f64().unwrap_or(0.0),
                                )
                            });
                    let mut earliest_reset = cur_reset_secs;
                    let mut combined_pct = cur_pct.max(0.0);
                    for a in &healthy_accts {
                        let aid = s(a, "id");
                        if let Some((rs, pct, is_passive)) =
                            find_acct_pool_window_entry_meta(a, window_key, Some(pool.label()))
                        {
                            let weekly_blocked = window_key == "fiveHour"
                                && find_acct_pool_window_entry(a, "weekly", Some(pool.label()))
                                    .is_some_and(|(_, wpct)| wpct < YELLOW_WEEKLY_MIN_PCT);
                            if !is_passive
                                && !weekly_blocked
                                && rs > 0
                                && (earliest_reset == 0 || rs < earliest_reset)
                            {
                                earliest_reset = rs;
                            }
                            if aid != sel_acct_id && !weekly_blocked && !is_passive {
                                combined_pct += pct.max(0.0);
                            }
                        }
                    }
                    let scale = if cur_pct > 0.5 {
                        (combined_pct / cur_pct).max(1.0)
                    } else {
                        1.0
                    };
                    let combined_mid_m = ((mid_m as f64) * scale).round() as u64;
                    if earliest_reset > 0 && combined_mid_m.saturating_mul(60) >= earliest_reset {
                        saw_known_ok = true;
                        continue;
                    }
                    if tightest.is_none_or(|(_, prev)| combined_mid_m < prev) {
                        tightest = Some((window_key, combined_mid_m));
                    }
                }
                _ => {}
            }
        }

        match tightest {
            Some((win, rem_m)) if rem_m < required_mins => {
                HeadroomCheck::WouldHold(HeadroomDecision {
                    pool,
                    account_id: sel_acct_id,
                    window: win.to_string(),
                    remaining_minutes: rem_m,
                    required_minutes: required_mins,
                    median_write_minutes: median_mins,
                    active_writes,
                    cache_age_seconds: cache_age_secs,
                })
            }
            Some(_) => HeadroomCheck::Fits,
            None if saw_known_ok => HeadroomCheck::Fits,
            None => HeadroomCheck::Unknown(HeadroomUnknownReason::NoEstimate),
        }
    }
}

/// Cache-only, zero-subprocess write-task quota headroom check for `pool` in `state`.
pub fn check_write_quota_headroom_in(
    state: &Path,
    pool: QuotaPool,
    now_ep: i64,
) -> HeadroomCheck {
    let Some(snap) = quota_snapshot_for_alerts_in(state, now_ep) else {
        return HeadroomCheck::Unknown(HeadroomUnknownReason::NoSnapshot);
    };
    let running_writers = count_running_writers(state, None);
    let median_mins = median_write_duration_mins_in(state, now_ep);
    check_write_quota_headroom_from_snapshot(&snap, pool, &running_writers, median_mins, now_ep)
}

#[derive(Debug, Clone)]
pub struct QuotaShadowPoolStats {
    pub pool: QuotaPool,
    pub holds: usize,
    pub true_positive_holds: usize,
    pub false_positive_holds: usize,
    pub false_positive_rate: Option<f64>,
    pub failures: usize,
    pub predicted_failures: usize,
    pub missed_failures: usize,
    pub recall: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct QuotaShadowReport {
    pub since_label: String,
    pub since_epoch: i64,
    pub window_override_mins: Option<u64>,
    pub total_holds: usize,
    pub true_positive_holds: usize,
    pub false_positive_holds: usize,
    pub false_positive_rate: Option<f64>,
    pub total_failures: usize,
    pub predicted_failures: usize,
    pub missed_failures: usize,
    pub recall: Option<f64>,
    pub by_pool: Vec<QuotaShadowPoolStats>,
}

#[derive(Debug, Clone)]
struct ShadowHoldRecord {
    at_ep: i64,
    run: String,
    pool: QuotaPool,
    horizon_secs: i64,
}

#[derive(Debug, Clone)]
struct QuotaFailureRecord {
    at_ep: i64,
    run: String,
    pool: Option<QuotaPool>,
}

fn hold_matches_failure(h: &ShadowHoldRecord, f: &QuotaFailureRecord) -> bool {
    if f.at_ep < h.at_ep {
        return false;
    }
    if !h.run.is_empty() && h.run == f.run {
        return true;
    }
    f.pool == Some(h.pool) && f.at_ep <= h.at_ep.saturating_add(h.horizon_secs)
}

/// Pure reconciliation of `quota_would_hold` shadow events against subsequent real
/// quota-exhaustion failures (`failed` events whose error matches `is_quota_exhausted_error`).
pub fn compute_quota_shadow_report(
    events: &[Value],
    since_label: &str,
    since_ep: i64,
    window_override_mins: Option<u64>,
) -> QuotaShadowReport {
    let mut run_pool: HashMap<String, QuotaPool> = HashMap::new();
    for ev in events {
        let kind = s(ev, "kind");
        let run = s(ev, "run");
        if run.is_empty() {
            continue;
        }
        if kind == "started" {
            let model = s(&ev["payload"], "model");
            if let Some(pool) = classify_model_pool(model) {
                run_pool.insert(run.to_string(), pool);
            }
        } else if kind == "quota_would_hold" {
            if let Some(pool) = classify_quota_pool_name(s(&ev["payload"], "pool")) {
                run_pool.entry(run.to_string()).or_insert(pool);
            }
        }
    }

    let mut all_holds: Vec<ShadowHoldRecord> = Vec::new();
    let mut window_holds: Vec<ShadowHoldRecord> = Vec::new();
    let mut all_failures: Vec<QuotaFailureRecord> = Vec::new();
    let mut window_failures: Vec<QuotaFailureRecord> = Vec::new();

    for ev in events {
        let Some(at_ep) = iso_epoch(s(ev, "at")) else {
            continue;
        };
        let kind = s(ev, "kind");
        let run = s(ev, "run").to_string();
        let p = &ev["payload"];
        match kind {
            "quota_would_hold" => {
                let Some(pool) = classify_quota_pool_name(s(p, "pool"))
                    .or_else(|| classify_quota_pool_name(s(p, "poolLabel")))
                else {
                    continue;
                };
                let horizon_mins = window_override_mins.unwrap_or_else(|| {
                    p["requiredMinutes"]
                        .as_u64()
                        .filter(|&m| m > 0)
                        .unwrap_or(SHADOW_DEFAULT_WINDOW_MINS)
                        .clamp(SHADOW_MIN_WINDOW_MINS, SHADOW_MAX_WINDOW_MINS)
                });
                let rec = ShadowHoldRecord {
                    at_ep,
                    run,
                    pool,
                    horizon_secs: (horizon_mins as i64).saturating_mul(60),
                };
                if at_ep >= since_ep {
                    window_holds.push(rec.clone());
                }
                all_holds.push(rec);
            }
            "failed" => {
                let err = s(p, "error");
                if !is_quota_exhausted_error(err) {
                    continue;
                }
                let pool = (!run.is_empty())
                    .then(|| run_pool.get(&run).copied())
                    .flatten()
                    .or_else(|| classify_quota_pool_name(s(p, "pool")));
                let rec = QuotaFailureRecord { at_ep, run, pool };
                if at_ep >= since_ep {
                    window_failures.push(rec.clone());
                }
                all_failures.push(rec);
            }
            _ => {}
        }
    }

    let total_holds = window_holds.len();
    let true_positive_holds = window_holds
        .iter()
        .filter(|h| all_failures.iter().any(|f| hold_matches_failure(h, f)))
        .count();
    let false_positive_holds = total_holds.saturating_sub(true_positive_holds);
    let false_positive_rate = (total_holds > 0)
        .then(|| false_positive_holds as f64 / total_holds as f64);

    let total_failures = window_failures.len();
    let predicted_failures = window_failures
        .iter()
        .filter(|f| all_holds.iter().any(|h| hold_matches_failure(h, f)))
        .count();
    let missed_failures = total_failures.saturating_sub(predicted_failures);
    let recall = (total_failures > 0)
        .then(|| predicted_failures as f64 / total_failures as f64);

    let mut by_pool = Vec::new();
    for pool in QuotaPool::ALL {
        let p_holds: Vec<&ShadowHoldRecord> =
            window_holds.iter().filter(|h| h.pool == pool).collect();
        let p_fails: Vec<&QuotaFailureRecord> = window_failures
            .iter()
            .filter(|f| f.pool == Some(pool))
            .collect();
        if p_holds.is_empty() && p_fails.is_empty() {
            continue;
        }
        let tp = p_holds
            .iter()
            .filter(|h| all_failures.iter().any(|f| hold_matches_failure(h, f)))
            .count();
        let fp = p_holds.len().saturating_sub(tp);
        let pred = p_fails
            .iter()
            .filter(|f| all_holds.iter().any(|h| hold_matches_failure(h, f)))
            .count();
        let missed = p_fails.len().saturating_sub(pred);
        by_pool.push(QuotaShadowPoolStats {
            pool,
            holds: p_holds.len(),
            true_positive_holds: tp,
            false_positive_holds: fp,
            false_positive_rate: (!p_holds.is_empty()).then(|| fp as f64 / p_holds.len() as f64),
            failures: p_fails.len(),
            predicted_failures: pred,
            missed_failures: missed,
            recall: (!p_fails.is_empty()).then(|| pred as f64 / p_fails.len() as f64),
        });
    }

    QuotaShadowReport {
        since_label: since_label.to_string(),
        since_epoch: since_ep,
        window_override_mins,
        total_holds,
        true_positive_holds,
        false_positive_holds,
        false_positive_rate,
        total_failures,
        predicted_failures,
        missed_failures,
        recall,
        by_pool,
    }
}

impl QuotaShadowReport {
    pub fn to_json(&self) -> Value {
        let round4 = |v: f64| (v * 10000.0).round() / 10000.0;
        let pools: Vec<Value> = self
            .by_pool
            .iter()
            .map(|p| {
                json!({
                    "pool": p.pool.key(),
                    "label": p.pool.short_label(),
                    "holds": p.holds,
                    "truePositiveHolds": p.true_positive_holds,
                    "falsePositiveHolds": p.false_positive_holds,
                    "falsePositiveRate": p.false_positive_rate.map(round4),
                    "quotaFailures": p.failures,
                    "predictedFailures": p.predicted_failures,
                    "missedFailures": p.missed_failures,
                    "recall": p.recall.map(round4),
                })
            })
            .collect();
        json!({
            "since": self.since_label,
            "sinceEpoch": self.since_epoch,
            "windowMinutes": self.window_override_mins,
            "holds": self.total_holds,
            "truePositiveHolds": self.true_positive_holds,
            "falsePositiveHolds": self.false_positive_holds,
            "falsePositiveRate": self.false_positive_rate.map(round4),
            "quotaFailures": self.total_failures,
            "predictedFailures": self.predicted_failures,
            "missedFailures": self.missed_failures,
            "recall": self.recall.map(round4),
            "byPool": pools,
        })
    }

    pub fn format_human(&self) -> String {
        let fmt_pct = |v: Option<f64>| match v {
            Some(r) => format!("{:.1}%", r * 100.0),
            None => "N/A".to_string(),
        };
        let win_desc = match self.window_override_mins {
            Some(m) => format!("固定窗 {m}m"),
            None => format!(
                "事件 requiredMinutes（限 {}–{}m，默认 {}m）",
                SHADOW_MIN_WINDOW_MINS, SHADOW_MAX_WINDOW_MINS, SHADOW_DEFAULT_WINDOW_MINS
            ),
        };
        let mut lines = Vec::new();
        lines.push(format!(
            "额度预报影子模式对账（近 {} · 匹配窗：{}）",
            self.since_label, win_desc
        ));
        lines.push(format!(
            "  召回率：{}（提前命中 {} / 真实额度失败 {}，漏报 {}）",
            fmt_pct(self.recall),
            self.predicted_failures,
            self.total_failures,
            self.missed_failures,
        ));
        lines.push(format!(
            "  误报率：{}（后续无失败 {} / 影子触发 {}，命中 {}）",
            fmt_pct(self.false_positive_rate),
            self.false_positive_holds,
            self.total_holds,
            self.true_positive_holds,
        ));
        for p in &self.by_pool {
            lines.push(format!(
                "  - {}：召回 {}（{}/{}），误报 {}（{}/{}）",
                p.pool.short_label(),
                fmt_pct(p.recall),
                p.predicted_failures,
                p.failures,
                fmt_pct(p.false_positive_rate),
                p.false_positive_holds,
                p.holds,
            ));
        }
        let verdict = match (self.recall, self.false_positive_rate) {
            (Some(rec), Some(fpr)) if rec >= 0.70 && fpr < 0.20 => {
                "参考门槛（召回 ≥ 70% 且误报 < 20%）：已达标，可评估转为真拦截"
            }
            (Some(_), Some(_)) => {
                "参考门槛（召回 ≥ 70% 且误报 < 20%）：暂未达标，继续观察或调参"
            }
            _ => "参考门槛（召回 ≥ 70% 且误报 < 20%）：样本不足（需同时有影子事件与真实 429 失败）",
        };
        lines.push(format!("  {verdict}"));
        lines.join("\n")
    }
}

/// CLI entrypoint for `delegate quota-shadow [--since 72h|DURATION|all] [--window MINS] [--json]`.
pub fn quota_shadow_command(args: &[String]) -> Res<i32> {
    let mut since_raw = "72h".to_string();
    let mut window_mins: Option<u64> = None;
    let mut json_mode = crate::output::json_enabled();

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (key, inline) = if a.starts_with("--") {
            a.split_once('=')
                .map_or((a.as_str(), None), |(k, v)| (k, Some(v)))
        } else {
            (a.as_str(), None)
        };
        let take_val = |k: &str, idx: &mut usize| -> Res<String> {
            match inline {
                Some(v) => Ok(v.to_string()),
                None => {
                    *idx += 1;
                    args.get(*idx)
                        .cloned()
                        .ok_or_else(|| format!("argument {k}: expected one argument"))
                }
            }
        };
        match key {
            "--since" => since_raw = take_val(key, &mut i)?,
            "--window" => {
                let raw = take_val(key, &mut i)?;
                let parsed = raw
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .filter(|&m| m > 0)
                    .ok_or_else(|| format!("invalid --window '{raw}' (expected positive integer minutes)"))?;
                window_mins = Some(parsed);
            }
            "--json" => json_mode = true,
            _ => {
                return Err(unrecognized_arg(
                    a,
                    &["--since", "--window", "--json"],
                    "delegate quota-shadow [--since 72h|DURATION|all] [--window MINS] [--json]",
                ));
            }
        }
        i += 1;
    }

    let (since_label, since_secs) = crate::fill::parse_since_arg(&since_raw)?;
    let now_ep = epoch() as i64;
    let since_ep = since_secs.map(|s| now_ep.saturating_sub(s)).unwrap_or(0);

    let state = state_dir();
    let evs = crate::events::read_since_in(&state, 0);
    let report = compute_quota_shadow_report(&evs, &since_label, since_ep, window_mins);

    if json_mode {
        println!("{}", report.to_json());
    } else {
        println!("{}", report.format_human());
    }
    Ok(0)
}

/// Format a prominent single-line warning for `delegate status` (advisory only).
pub fn format_status_quota_warning_line(warnings: &[QuotaDepletionWarning]) -> Option<String> {
    let top = warnings.first()?;
    let eta = format_eta_zh_phrase(
        top.fast_minutes,
        top.remaining_minutes,
        top.slow_minutes,
        top.reset_in_seconds,
    );
    let reset = format_reset_zh_short(top.reset_in_seconds);
    let extra = if warnings.len() > 1 {
        format!("；另有 {} 项也预计提前用完", warnings.len() - 1)
    } else {
        String::new()
    };
    if top.auto_all_low && !top.pool_summary.is_empty() {
        Some(format!(
            "⚠ 额度预警：auto 模式 {} 池所有可用号均偏低（{}），{}额度{}（最早 {}后重置）{extra}",
            top.pool, top.pool_summary, top.window_label, eta, reset
        ))
    } else {
        let role = if top.active { "当前账号 " } else { "账号 " };
        Some(format!(
            "⚠ 额度预警：{role}{} 的 {} {}额度{}（{}后重置；可手动切号或启用 auto）{extra}",
            top.account_id, top.pool, top.window_label, eta, reset
        ))
    }
}

fn format_cli_quota_estimate_line(label: &str, est: &Value) -> Option<String> {
    if !est.is_object() {
        return None;
    }
    let pool = short_pool_label(s(est, "pool"));
    let basis = s(est, "basis");
    let running = est["runningCount"].as_u64().unwrap_or(0);
    let run_note = if running > 0 {
        format!(", running {running}")
    } else {
        String::new()
    };
    let status = s(est, "status");
    let detail = match status {
        "depletes" => {
            let mid_m = est["midMinutes"].as_u64().unwrap_or(0);
            let fast_m = est["fastMinutes"].as_u64().unwrap_or(mid_m);
            let slow_m = est["slowMinutes"].as_u64();
            let reset_s = est["resetInSeconds"].as_u64().unwrap_or(u64::MAX);
            let phrase = format_eta_zh_phrase(fast_m, mid_m, slow_m, reset_s);
            let basis_note = if basis.is_empty() {
                String::new()
            } else {
                format!("basis {basis}{run_note}")
            };
            if basis_note.is_empty() {
                phrase
            } else {
                format!("{phrase} ({basis_note})")
            }
        }
        "safe" => {
            if basis.is_empty() {
                "重置前够用".to_string()
            } else {
                format!("重置前够用 (basis {basis}{run_note})")
            }
        }
        "idle" => "空闲（无在跑任务）".to_string(),
        "insufficient" => {
            let reason = s(est, "reason");
            match reason {
                "too_early" => {
                    let em = est["elapsedMinutes"].as_u64().unwrap_or(0);
                    let pct = est["remainingPct"].as_f64().unwrap_or(100.0);
                    format!(
                        "刚重置 {}（剩 {pct:.0}%），数据不足暂不外推",
                        format_minutes_zh_short(em)
                    )
                }
                "not_started" => "尚未使用（剩 100%），暂不外推".to_string(),
                "stale" => "缓存已过期，暂不外推".to_string(),
                "reset_passed" => "已过重置时间，待下次刷新".to_string(),
                _ => "数据不足，暂不外推".to_string(),
            }
        }
        _ => return None,
    };
    Some(format!("  {label} estimate ({pool}): {detail}"))
}

fn snapshot_json_opts(
    scan_mode: RunningAgyScanMode,
    precollected_usage: Option<&crate::usage::PrecollectedUsage>,
) -> Value {
    let root = accounts_root();
    let home_dir = home();
    let state_root = state_dir();
    let state = load_state(&root);
    let active_id = state["active"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let live = live_email(&home_dir).unwrap_or_default();
    let running = match scan_mode {
        RunningAgyScanMode::FullProc => detect_running_agy_for_home(&home_dir),
        RunningAgyScanMode::KnownRunsOnly => detect_running_agy_from_known_runs(&state_root),
        RunningAgyScanMode::Skip => Vec::new(),
    };

    let now_ep = epoch() as i64;
    let quota_cache = load_quota_cache();
    let empty_map = Map::new();
    let accounts_map = state["accounts"].as_object().unwrap_or(&empty_map);
    let usage_snap = if !accounts_map.is_empty() || crate::usage::ledger_path().is_file() {
        Some(match precollected_usage {
            Some(pre) => crate::usage::accounts_usage_snapshot_from_precollected(accounts_map, pre),
            None => crate::usage::accounts_usage_snapshot(accounts_map),
        })
    } else {
        None
    };

    let auto_cfg = load_agy_auto_switch_config();
    let auto_mode = is_auto_mode_state(&state);
    let pool_cur = load_pool_current_in(&state_root);
    let running_tasks = count_running_pool_tasks(&state_root, None);
    let running_writers: BTreeMap<(String, QuotaPool), usize> = running_tasks
        .iter()
        .filter_map(|(k, c)| (c.writers > 0).then_some((k.clone(), c.writers)))
        .collect();

    let mut pool_cur_json = Map::new();
    let mut pool_summary_json = Map::new();
    let mut selected_by_pool: BTreeMap<QuotaPool, String> = BTreeMap::new();
    let mut auto_eval_state = state.clone();
    auto_eval_state["mode"] = json!(AUTO_ACCOUNT_ID);
    for pool in QuotaPool::ALL {
        let cur = pool_cur
            .get(&pool)
            .filter(|id| state["accounts"].get(id.as_str()).is_some())
            .cloned()
            .or_else(|| active_id.clone());
        if let Some(id) = cur {
            pool_cur_json.insert(pool.key().to_string(), json!(id));
        }
        let probe_model = match pool {
            QuotaPool::Gemini => "agy/gemini",
            QuotaPool::ClaudeGpt => "agy/claude",
        };
        let dec = select_account_for_new_session(
            &root,
            &auto_eval_state,
            &quota_cache,
            &pool_cur,
            &running_writers,
            probe_model,
            &auto_cfg,
            now_ep,
        );
        let cur_id = dec.current_before.clone().unwrap_or_default();
        let sel_id = dec.selected_id.clone().unwrap_or_default();
        let effective_sel = if auto_mode {
            (!sel_id.is_empty()).then_some(sel_id.clone()).or_else(|| active_id.clone())
        } else {
            active_id.clone()
        };
        if let Some(sid) = effective_sel {
            selected_by_pool.insert(pool, sid);
        }
        let sel_eval = if !sel_id.is_empty() {
            let counts = running_tasks
                .get(&(sel_id.clone(), pool))
                .copied()
                .unwrap_or_default();
            Some(evaluate_pool_quota(
                &quota_cache["accounts"][&sel_id],
                pool,
                &auto_cfg,
                counts.writers,
                counts.total,
                true,
                now_ep,
            ))
        } else {
            None
        };
        pool_summary_json.insert(
            pool.key().to_string(),
            json!({
                "pool": pool.key(),
                "label": pool.short_label(),
                "current": cur_id,
                "selected": sel_id,
                "tier": sel_eval.as_ref().map(|e| e.tier.as_str()).unwrap_or("green"),
                "tierLabel": sel_eval.as_ref().map(|e| e.tier.label_zh()).unwrap_or("绿"),
                "wouldSwitch": dec.switched.is_some(),
                "reason": dec.reason,
            }),
        );
    }

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
            }
            let qest = build_account_quota_estimate_json(
                &quota_cache["accounts"][id],
                &auto_cfg,
                &running_tasks,
                id,
                &selected_by_pool,
                now_ep,
            );
            if !qest.is_null() {
                item["quotaEstimate"] = qest;
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

    let mut auto_switch_json = auto_cfg.to_json();
    auto_switch_json["enabled"] = json!(auto_mode);
    auto_switch_json["mode"] = json!(if auto_mode { "auto" } else { "fixed" });
    auto_switch_json["poolCurrent"] = Value::Object(pool_cur_json);
    auto_switch_json["poolSummary"] = Value::Object(pool_summary_json);
    let selected_str: Option<String> = if auto_mode {
        Some(AUTO_ACCOUNT_ID.to_string())
    } else {
        active_id.clone()
    };

    let mut out = json!({
        "root": root.to_string_lossy(),
        "active": active_id,
        "mode": if auto_mode { "auto" } else { "fixed" },
        "selected": selected_str,
        "liveEmail": live,
        "liveDrifted": live_drifted,
        "runningAgy": running_json,
        "accounts": items,
        "quotaUpdatedAt": quota_updated_at,
        "quotaAgeSeconds": quota_age_secs,
        "autoSwitch": auto_switch_json,
    });
    if !dup_groups.is_empty() {
        out["duplicateQuotaGroups"] = json!(dup_groups);
    }
    if let Some(snap) = usage_snap {
        out["unassignedUsage"] = snap.unassigned;
    }
    let rate_shifts = crate::quota_rate::load_cached_shifts_in(&state_dir(), now_ep);
    if !rate_shifts.is_empty() {
        out["rateShifts"] = Value::Array(rate_shifts);
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

/// Base directory for per-task ephemeral `agy` HOME overlays.
fn temp_home_base_dir() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(env::temp_dir)
}

/// RAII temporary HOME directory cleaned up automatically on drop.
struct TempHome {
    path: PathBuf,
}

impl TempHome {
    fn create() -> Res<Self> {
        let base = temp_home_base_dir();
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

/// Result of sweeping orphan `delegate-agy-home-*` directories.
#[derive(Debug, Default, Clone)]
pub struct AgyHomeCleanupReport {
    pub removed: usize,
    pub removed_names: Vec<String>,
    pub failed: usize,
}

impl AgyHomeCleanupReport {
    pub fn json(&self) -> Value {
        json!({
            "removed": self.removed,
            "removedNames": self.removed_names,
            "failed": self.failed,
        })
    }
}

/// Parse `<pid>` from a strict `delegate-agy-home-<pid>-<seq>-<16hex>` directory name.
fn parse_delegate_agy_home_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("delegate-agy-home-")?;
    let mut parts = rest.split('-');
    let pid_str = parts.next()?;
    let seq_str = parts.next()?;
    let hex_str = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if pid_str.is_empty() || !pid_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if seq_str.is_empty() || !seq_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if hex_str.len() != 16
        || !hex_str
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let pid = pid_str.parse::<u32>().ok()?;
    let _seq = seq_str.parse::<u64>().ok()?;
    (pid > 0).then_some(pid)
}

/// Confirm `path` is a real directory owned by the current uid with strict delegate naming
/// and expected overlay structure (never follows symlinks).
fn is_delegate_agy_home_dir(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let pid = parse_delegate_agy_home_pid(name)?;
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_dir() {
        return None;
    }
    let my_uid = unsafe { libc::geteuid() };
    if meta.uid() != my_uid {
        return None;
    }
    // Primary structure check: `.gemini` is a real subdirectory created by delegate.
    if fs::symlink_metadata(path.join(".gemini"))
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false)
    {
        return Some(pid);
    }
    // Secondary structure check for mid-setup kills before `.gemini` was created:
    // require strict 0700 mode AND (empty OR only `.config` real dir + symlinks).
    if (meta.mode() & 0o777) != 0o700 {
        return None;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return None;
    };
    let mut count = 0usize;
    let mut all_valid = true;
    for entry in entries.flatten() {
        count += 1;
        let Ok(em) = fs::symlink_metadata(entry.path()) else {
            all_valid = false;
            break;
        };
        if em.file_type().is_symlink() {
            continue;
        }
        if entry.file_name() == ".config" && em.file_type().is_dir() {
            continue;
        }
        all_valid = false;
        break;
    }
    if count == 0 || all_valid {
        Some(pid)
    } else {
        None
    }
}

fn pid_alive_non_zombie(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    if !proc_dir.is_dir() {
        return false;
    }
    let stat = read(proc_dir.join("stat"));
    if let Some((_, tail)) = stat.rsplit_once(") ") {
        if tail.starts_with('Z') {
            return false;
        }
    }
    true
}

/// Collect `HOME=` paths of all live processes owned by the current uid that reference a
/// `delegate-agy-home-*` directory.
fn collect_active_agy_home_paths() -> std::collections::HashSet<PathBuf> {
    let mut active = std::collections::HashSet::new();
    let my_uid = unsafe { libc::geteuid() };
    let Ok(entries) = fs::read_dir("/proc") else {
        return active;
    };
    let needle = b"delegate-agy-home-";
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(name) = fname.to_str() else {
            continue;
        };
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let proc_dir = entry.path();
        let Ok(meta) = fs::metadata(&proc_dir) else {
            continue;
        };
        if meta.uid() != my_uid {
            continue;
        }
        let stat = read(proc_dir.join("stat"));
        if let Some((_, tail)) = stat.rsplit_once(") ") {
            if tail.starts_with('Z') {
                continue;
            }
        }
        let Ok(raw_env) = fs::read(proc_dir.join("environ")) else {
            continue;
        };
        if !raw_env.windows(needle.len()).any(|w| w == needle) {
            continue;
        }
        for item in raw_env.split(|&b| b == 0) {
            if let Some(val) = item.strip_prefix(b"HOME=") {
                let p = PathBuf::from(String::from_utf8_lossy(val).into_owned());
                if let Ok(canon) = fs::canonicalize(&p) {
                    active.insert(canon);
                }
                active.insert(p);
                break;
            }
        }
    }
    active
}

/// Remove orphan `delegate-agy-home-*` directories in `base` that belong to delegate and
/// have no live creator process and no live process using them as `HOME`.
pub fn cleanup_orphan_agy_homes_in(base: &Path) -> AgyHomeCleanupReport {
    let mut report = AgyHomeCleanupReport::default();
    let Ok(entries) = fs::read_dir(base) else {
        return report;
    };
    let mut candidates: Vec<(PathBuf, String, u32)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        if let Some(pid) = is_delegate_agy_home_dir(&path) {
            candidates.push((path, name, pid));
        }
    }
    if candidates.is_empty() {
        return report;
    }
    let active_homes = collect_active_agy_home_paths();
    for (path, name, pid) in candidates {
        if pid_alive_non_zombie(pid) {
            continue;
        }
        let canon = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if active_homes.contains(&path) || active_homes.contains(&canon) {
            continue;
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => {
                report.removed += 1;
                report.removed_names.push(name);
            }
            Err(_) => {
                report.failed += 1;
            }
        }
    }
    report.removed_names.sort();
    report
}

/// Sweep orphan `delegate-agy-home-*` directories under the configured runtime/temp base dir.
pub fn cleanup_orphan_agy_homes() -> AgyHomeCleanupReport {
    cleanup_orphan_agy_homes_in(&temp_home_base_dir())
}

/// Clean up a specific run's recorded per-task `agy_home` directory (used when a run is
/// force-killed and its `TempHome::drop` could not execute).
pub fn cleanup_run_agy_home(run_dir: &Path) {
    let marker = run_dir.join("agy_home");
    let raw = read(&marker);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        let _ = fs::remove_file(&marker);
        return;
    }
    let path = PathBuf::from(trimmed);
    if is_delegate_agy_home_dir(&path).is_some() {
        let active_homes = collect_active_agy_home_paths();
        let canon = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !active_homes.contains(&path) && !active_homes.contains(&canon) {
            let _ = fs::remove_dir_all(&path);
        }
    }
    let _ = fs::remove_file(&marker);
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

/// Canonical quota pools returned by `agy -p /usage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum QuotaPool {
    Gemini,
    ClaudeGpt,
}

impl QuotaPool {
    pub const ALL: [QuotaPool; 2] = [QuotaPool::Gemini, QuotaPool::ClaudeGpt];

    pub fn key(self) -> &'static str {
        match self {
            QuotaPool::Gemini => "gemini",
            QuotaPool::ClaudeGpt => "claude_gpt",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            QuotaPool::Gemini => "Gemini Models",
            QuotaPool::ClaudeGpt => "Claude and GPT models",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            QuotaPool::Gemini => "Gemini",
            QuotaPool::ClaudeGpt => "Claude/GPT",
        }
    }
}

/// Classify a `/usage` pool name (e.g. `"Gemini Models"`, `"Claude and GPT models"`) into a canonical `QuotaPool`.
pub fn classify_quota_pool_name(pool: &str) -> Option<QuotaPool> {
    let p = pool.trim().to_ascii_lowercase();
    if p.contains("gemini") {
        Some(QuotaPool::Gemini)
    } else if p.contains("claude") || p.contains("gpt") {
        Some(QuotaPool::ClaudeGpt)
    } else {
        None
    }
}

/// Single canonical classifier mapping a model string/alias (e.g. `"agy/gemini-4-neon"`,
/// `"agy/claude-opus-4-6-thinking"`, `"carbon"`, `"hydrogen"`, `"haiku"`, `"codex"`) to its quota pool.
pub fn classify_model_pool(model: &str) -> Option<QuotaPool> {
    let m = model.trim().to_ascii_lowercase();
    if m.is_empty() {
        return None;
    }
    let id = m.rsplit('/').next().unwrap_or(&m);
    if ["claude", "gpt", "sonnet", "opus", "haiku", "codex"]
        .iter()
        .any(|k| id.contains(k) || m.contains(k))
    {
        return Some(QuotaPool::ClaudeGpt);
    }
    if m.contains("gemini")
        || ["gemini", "neon", "carbon", "hydrogen", "flash", "pro"]
            .iter()
            .any(|k| id.contains(k))
    {
        return Some(QuotaPool::Gemini);
    }
    None
}

/// Unified matcher used by `agy_accounts`, `usage`, and `review` to check whether a model
/// belongs to a `/usage` quota pool.
#[cfg(test)]
pub fn model_matches_pool(model: &str, pool: &str) -> bool {
    match (classify_model_pool(model), classify_quota_pool_name(pool)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Three-tier quota health for auto-switch hysteresis: Green > Yellow > Red.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum QuotaTier {
    Red = 0,
    Yellow = 1,
    Green = 2,
}

impl QuotaTier {
    pub fn as_str(self) -> &'static str {
        match self {
            QuotaTier::Green => "green",
            QuotaTier::Yellow => "yellow",
            QuotaTier::Red => "red",
        }
    }

    pub fn label_zh(self) -> &'static str {
        match self {
            QuotaTier::Green => "绿",
            QuotaTier::Yellow => "黄",
            QuotaTier::Red => "红",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuotaWindow {
    FiveHour,
    Weekly,
}

#[derive(Debug, Clone, PartialEq)]
pub enum QuotaEta {
    /// Window not started (100% / resetAt=1970-01-01), already reset, stale cache,
    /// missing resetAt, or too early in the current cycle to extrapolate safely.
    Insufficient {
        reason: &'static str,
        elapsed_mins: Option<u64>,
    },
    /// No active burn on this pool right now.
    Idle,
    /// Projected to last past `resetAt` (or negligible weekly drop < 3%).
    SafeBeforeReset {
        basis_label: String,
    },
    /// Projected to deplete before `resetAt`.
    DepletesIn {
        fast_m: u64,
        mid_m: u64,
        /// `None` when the slow/optimistic bound reaches or exceeds `resetAt` (`~重置前`).
        slow_m: Option<u64>,
        /// True when all Step ⑤ alarm gates pass (`k_run > 0`, `mid < 0.85 * reset`, pct/rate gate).
        alert: bool,
        basis_label: String,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunningPoolCounts {
    pub writers: usize,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PoolQuotaState {
    pub tier: QuotaTier,
    pub raw_five_hour_pct: Option<f64>,
    pub eff_five_hour_pct: Option<f64>,
    pub weekly_pct: Option<f64>,
    pub observed_five_hour_pct: Option<f64>,
    pub observed_weekly_pct: Option<f64>,
    pub reset_in_five_hour_secs: Option<i64>,
    pub reset_in_weekly_secs: Option<i64>,
    pub running_writers: usize,
    pub running_all: usize,
    pub passive_exhausted: bool,
    pub unknown: bool,
    pub cache_age_secs: Option<i64>,
    pub eta_five_hour: QuotaEta,
    pub eta_weekly: QuotaEta,
}

impl PoolQuotaState {
    pub fn to_quota_json(&self) -> Value {
        let mut obj = Map::new();
        if let Some(v) = self.eff_five_hour_pct {
            obj.insert("5h".into(), json!((v * 10.0).round() / 10.0));
        }
        if let Some(v) = self.raw_five_hour_pct {
            if self.running_writers > 0 {
                obj.insert("raw5h".into(), json!((v * 10.0).round() / 10.0));
            }
        }
        if let Some(v) = self.weekly_pct {
            obj.insert("7d".into(), json!((v * 10.0).round() / 10.0));
        }
        obj.insert("tier".into(), json!(self.tier.as_str()));
        if self.running_writers > 0 {
            obj.insert("runningWriters".into(), json!(self.running_writers));
        }
        if self.passive_exhausted {
            obj.insert("passiveExhausted".into(), json!(true));
        }
        Value::Object(obj)
    }

    pub fn window_eta_json(&self, pool_name: &str, window: &str) -> Option<Value> {
        let (eta, raw_pct, eff_pct, reset_secs) = match window {
            "fiveHour" => (
                &self.eta_five_hour,
                self.raw_five_hour_pct.or(self.observed_five_hour_pct),
                self.eff_five_hour_pct.or(self.observed_five_hour_pct),
                self.reset_in_five_hour_secs,
            ),
            "weekly" => (
                &self.eta_weekly,
                self.weekly_pct.or(self.observed_weekly_pct),
                self.weekly_pct.or(self.observed_weekly_pct),
                self.reset_in_weekly_secs,
            ),
            _ => return None,
        };
        if raw_pct.is_none() && matches!(eta, QuotaEta::Insufficient { reason: "no_data", .. }) {
            return None;
        }
        let mut obj = Map::new();
        obj.insert("pool".into(), json!(pool_name));
        obj.insert("source".into(), json!("real"));
        obj.insert("tier".into(), json!(self.tier.as_str()));
        obj.insert("runningWriters".into(), json!(self.running_writers));
        obj.insert("runningCount".into(), json!(self.running_all));
        if let Some(p) = raw_pct {
            obj.insert("remainingPct".into(), json!((p * 100.0).round() / 100.0));
        }
        if let Some(ep) = eff_pct {
            obj.insert("effectivePct".into(), json!((ep * 100.0).round() / 100.0));
        }
        if let Some(rs) = reset_secs.filter(|&s| s > 0) {
            obj.insert("resetInSeconds".into(), json!(rs));
        }
        match eta {
            QuotaEta::Insufficient {
                reason,
                elapsed_mins,
            } => {
                obj.insert("status".into(), json!("insufficient"));
                obj.insert("reason".into(), json!(reason));
                if let Some(em) = elapsed_mins {
                    obj.insert("elapsedMinutes".into(), json!(em));
                }
            }
            QuotaEta::Idle => {
                obj.insert("status".into(), json!("idle"));
            }
            QuotaEta::SafeBeforeReset { basis_label } => {
                obj.insert("status".into(), json!("safe"));
                obj.insert("basis".into(), json!(basis_label));
            }
            QuotaEta::DepletesIn {
                fast_m,
                mid_m,
                slow_m,
                alert,
                basis_label,
            } => {
                obj.insert("status".into(), json!("depletes"));
                obj.insert("alert".into(), json!(alert));
                obj.insert("fastMinutes".into(), json!(fast_m));
                obj.insert("midMinutes".into(), json!(mid_m));
                if let Some(sm) = slow_m {
                    obj.insert("slowMinutes".into(), json!(sm));
                }
                obj.insert("basis".into(), json!(basis_label));
            }
        }
        Some(Value::Object(obj))
    }

    fn format_short(&self) -> String {
        if self.passive_exhausted {
            return "被动耗尽(0%)".to_string();
        }
        if self.unknown {
            return "未知".to_string();
        }
        let mut parts = Vec::new();
        if let Some(eff) = self.eff_five_hour_pct {
            if self.running_writers > 0 {
                if let Some(raw) = self.raw_five_hour_pct {
                    parts.push(format!(
                        "有效5h {:.0}%({:.0}%-{}写手)",
                        eff, raw, self.running_writers
                    ));
                } else {
                    parts.push(format!("有效5h {:.0}%", eff));
                }
            } else {
                parts.push(format!("5h {:.0}%", eff));
            }
        }
        if let Some(wk) = self.weekly_pct {
            parts.push(format!("7d {:.0}%", wk));
        }
        if parts.is_empty() {
            "未知".to_string()
        } else {
            parts.join(" / ")
        }
    }
}

pub fn auto_switch_state_path_in(state_root: &Path) -> PathBuf {
    state_root.join("agy-auto-switch.json")
}

pub fn load_pool_current_in(state_root: &Path) -> BTreeMap<QuotaPool, String> {
    let mut out = BTreeMap::new();
    let doc = json(auto_switch_state_path_in(state_root));
    let Some(map) = doc["poolCurrent"].as_object() else {
        return out;
    };
    for pool in QuotaPool::ALL {
        if let Some(id) = map
            .get(pool.key())
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            out.insert(pool, id.to_string());
        }
    }
    out
}

pub fn save_pool_current_in(state_root: &Path, map: &BTreeMap<QuotaPool, String>) -> Res<()> {
    let _lock = crate::locks::acquire_state(state_root).ok();
    let mut pc = Map::new();
    for (pool, id) in map {
        if !id.trim().is_empty() {
            pc.insert(pool.key().to_string(), json!(id.trim()));
        }
    }
    let doc = json!({
        "schemaVersion": 1,
        "updatedAt": iso(),
        "poolCurrent": Value::Object(pc),
    });
    let path = auto_switch_state_path_in(state_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    write_json(&path, &doc)
}

pub fn set_pool_current_in(state_root: &Path, pool: QuotaPool, account_id: &str) -> Res<()> {
    let _lock = crate::locks::acquire_state(state_root).ok();
    let mut map = load_pool_current_in(state_root);
    map.insert(pool, account_id.to_string());
    let mut pc = Map::new();
    for (p, id) in &map {
        pc.insert(p.key().to_string(), json!(id));
    }
    let doc = json!({
        "schemaVersion": 1,
        "updatedAt": iso(),
        "poolCurrent": Value::Object(pc),
    });
    let path = auto_switch_state_path_in(state_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    write_json(&path, &doc)
}

pub fn set_all_pool_currents_in(state_root: &Path, account_id: &str) -> Res<()> {
    let mut map = BTreeMap::new();
    for pool in QuotaPool::ALL {
        map.insert(pool, account_id.to_string());
    }
    save_pool_current_in(state_root, &map)
}

pub fn clear_pool_current_slot_in(state_root: &Path, removed_id: &str) -> Res<()> {
    let _lock = crate::locks::acquire_state(state_root).ok();
    let mut map = load_pool_current_in(state_root);
    let before = map.len();
    map.retain(|_, v| v != removed_id);
    if map.len() != before {
        let mut pc = Map::new();
        for (p, id) in &map {
            pc.insert(p.key().to_string(), json!(id));
        }
        let doc = json!({
            "schemaVersion": 1,
            "updatedAt": iso(),
            "poolCurrent": Value::Object(pc),
        });
        let path = auto_switch_state_path_in(state_root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        write_json(&path, &doc)?;
    }
    Ok(())
}

/// Whether `accounts.json` is currently in `auto` selection mode.
/// Backward compatible: legacy files without `"mode"` default to fixed mode (`false`).
pub fn is_auto_mode_state(state: &Value) -> bool {
    s(state, "mode") == AUTO_ACCOUNT_ID || s(state, "active") == AUTO_ACCOUNT_ID
}

pub fn is_auto_mode_enabled() -> bool {
    let root = accounts_root();
    if !accounts_meta_path(&root).is_file() {
        return false;
    }
    is_auto_mode_state(&load_state(&root))
}

/// Return the current effective account slot for `model`'s quota pool:
/// uses persisted `poolCurrent[pool]` when in `auto` mode and valid, falling back to `active`.
pub fn current_account_for_model(model: &str) -> Option<String> {
    current_account_for_model_in(&accounts_root(), &state_dir(), model)
}

pub fn current_account_for_model_in(
    root: &Path,
    state_root: &Path,
    model: &str,
) -> Option<String> {
    let state = load_state(root);
    let active = state["active"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if !is_auto_mode_state(&state) {
        return active;
    }
    if let Some(pool) = classify_model_pool(model) {
        let pool_cur = load_pool_current_in(state_root);
        if let Some(id) = pool_cur.get(&pool) {
            if state["accounts"].get(id.as_str()).is_some() {
                return Some(id.clone());
            }
        }
    }
    active
}

/// Strict quota-exhaustion detector for passive marking & mid-run handoff.
/// Explicitly EXCLUDES transient HTTP 429 / RPM rate limits / capacity blips so an occasional
/// rate limit never falsely marks a healthy account as 0% exhausted.
pub fn is_quota_exhausted_error(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    let transient_rate_limit = [
        "rate limit",
        "rate_limit",
        "too many requests",
        "requests per minute",
        "per minute",
        " rpm",
        "(rpm",
        " tpm",
        "(tpm",
        "retry after",
        "try again in",
    ]
    .iter()
    .any(|m| lower.contains(m));

    // Unambiguous hard quota exhaustion markers.
    let hard_markers = [
        "insufficient_quota",
        "quota exceeded",
        "quota_exceeded",
        "quota exhausted",
        "quota_exhausted",
        "quota reached",
        "quota_reached",
        "quota limit",
        "quota will reset",
        "exceeded your current quota",
        "out of quota",
        "no quota remaining",
        "insufficient credit",
        "insufficient_credit",
        "five hour limit",
        "weekly limit",
        "usage limit",
    ];
    if hard_markers.iter().any(|m| lower.contains(m)) {
        return true;
    }
    if !transient_rate_limit
        && lower.contains("resource_exhausted")
        && (lower.contains("quota")
            || lower.contains("resets in")
            || lower.contains("upgrade your subscription"))
    {
        return true;
    }
    if !transient_rate_limit
        && !lower.contains("429")
        && (lower.contains("resource_exhausted") || lower.contains("limit reached"))
    {
        return true;
    }
    false
}

/// Detect OAuth / 401 authentication failure text from `/usage` or `agy` run errors.
pub fn is_auth_failure_error(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    let has_401 = lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w == "401");
    has_401
        || [
            "unauthorized",
            "unauthenticated",
            "invalid_grant",
            "token has been expired or revoked",
            "invalid credentials",
            "login required",
            "invalid api key",
            "authentication failed",
        ]
        .iter()
        .any(|m| lower.contains(m))
}

/// Inspect `run/events.jsonl` (for `attempt`) and `run/stderr.log` to determine whether the
/// attempt ended due to a hard quota-exhaustion error (strictly excluding transient 429).
pub fn attempt_quota_exhausted_error(run: &Path, attempt: i64) -> Option<String> {
    let evs = crate::runs::events(run);
    let mut saw_attempt = false;
    for ev in evs.iter().rev() {
        if ev.get("attempt").and_then(Value::as_i64) == Some(attempt) {
            saw_attempt = true;
            if s(ev, "e") == "turn_error" {
                let detail = s(ev, "detail");
                if is_quota_exhausted_error(detail) {
                    return Some(detail.to_string());
                }
            }
        }
    }
    if !saw_attempt {
        if let Some(ev) = evs.iter().rev().find(|ev| s(ev, "e") == "turn_error") {
            let detail = s(ev, "detail");
            if is_quota_exhausted_error(detail) {
                return Some(detail.to_string());
            }
        }
    }
    let stderr_txt = read(run.join("stderr.log"));
    let tail: String = stderr_txt
        .lines()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    if is_quota_exhausted_error(&tail) {
        return Some(clip(tail.trim(), 300));
    }
    None
}

/// Soft-mark `account_id`'s `fiveHour` entry for `model`'s quota pool as passively exhausted (0%)
/// in `<state>/agy-quota.json`. Preserves `weekly`, `history`, and account-level `fetchedEpoch`.
/// Synthesizes `resetAt = now + 15m` when no future `resetAt` is already cached.
pub fn mark_slot_quota_exhausted_passive_in(
    state_root: &Path,
    account_id: &str,
    model: &str,
    now_ep: i64,
) -> Res<()> {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return Ok(());
    }
    let Some(pool) = classify_model_pool(model) else {
        return Ok(());
    };
    let _lock = crate::locks::acquire_state(state_root).ok();
    let mut cache = load_quota_cache_in(state_root);
    if !cache["accounts"].is_object() {
        cache["accounts"] = json!({});
    }
    let acct = cache["accounts"]
        .as_object_mut()
        .unwrap()
        .entry(account_id.to_string())
        .or_insert_with(|| {
            json!({
                "fetchedAt": "",
                "fetchedEpoch": 0,
                "ok": true,
                "entries": [],
                "unknownLines": [],
                "refreshedCreds": 0,
                "history": [],
            })
        });
    if !acct["entries"].is_array() {
        acct["entries"] = json!([]);
    }
    let synth_reset = format_epoch(now_ep + PASSIVE_SYNTH_RESET_SECS, false, "%Y-%m-%dT%H:%M:%SZ");
    let entries = acct["entries"].as_array_mut().unwrap();
    let mut updated = false;
    for entry in entries.iter_mut() {
        if classify_quota_pool_name(s(entry, "pool")) == Some(pool)
            && s(entry, "window") == "fiveHour"
        {
            let existing_reset = s(entry, "resetAt").to_string();
            let keep_reset = iso_epoch(&existing_reset).is_some_and(|rep| rep > now_ep);
            entry["remainingPct"] = json!(0.0);
            entry["source"] = json!("passive_error");
            entry["fetchedEpoch"] = json!(now_ep);
            if !keep_reset {
                entry["resetAt"] = json!(&synth_reset);
            }
            updated = true;
        }
    }
    if !updated {
        entries.push(json!({
            "pool": pool.label(),
            "window": "fiveHour",
            "windowLabel": "Five Hour Limit Remaining",
            "remainingPct": 0.0,
            "resetAt": synth_reset,
            "source": "passive_error",
            "fetchedEpoch": now_ep,
        }));
    }
    save_quota_cache_in(state_root, &cache)
}

pub fn mark_slot_quota_exhausted_passive(
    account_id: &str,
    model: &str,
    now_ep: i64,
) -> Res<()> {
    mark_slot_quota_exhausted_passive_in(&state_dir(), account_id, model, now_ep)
}

/// Record OAuth/401 auth failure timestamp on `account_id` in `<state>/agy-quota.json` so
/// auto-switch excludes this broken slot until credentials are updated or `/usage` succeeds.
pub fn mark_slot_auth_failed_in(
    state_root: &Path,
    account_id: &str,
    now_ep: i64,
    reason: &str,
) -> Res<()> {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return Ok(());
    }
    let _lock = crate::locks::acquire_state(state_root).ok();
    let mut cache = load_quota_cache_in(state_root);
    if !cache["accounts"].is_object() {
        cache["accounts"] = json!({});
    }
    let now_iso = format_epoch(now_ep, false, "%Y-%m-%dT%H:%M:%SZ");
    let acct = cache["accounts"]
        .as_object_mut()
        .unwrap()
        .entry(account_id.to_string())
        .or_insert_with(|| {
            json!({
                "fetchedAt": "",
                "fetchedEpoch": 0,
                "ok": false,
                "entries": [],
                "unknownLines": [],
                "refreshedCreds": 0,
                "history": [],
            })
        });
    acct["authFailedAt"] = json!(now_iso);
    acct["authFailedEpoch"] = json!(now_ep);
    if !reason.trim().is_empty() {
        acct["lastError"] = json!(clip(reason.trim(), 240));
    }
    save_quota_cache_in(state_root, &cache)
}

pub fn mark_slot_auth_failed(account_id: &str, now_ep: i64, reason: &str) -> Res<()> {
    mark_slot_auth_failed_in(&state_dir(), account_id, now_ep, reason)
}

fn slot_is_auth_failed(sdir: &Path, acct_quota: &Value) -> bool {
    let auth_ep = acct_quota["authFailedEpoch"].as_i64().unwrap_or(0);
    if auth_ep <= 0 {
        return false;
    }
    if b(acct_quota, "ok") && acct_quota["fetchedEpoch"].as_i64().unwrap_or(0) > auth_ep {
        return false;
    }
    // If any core credential file was updated after `authFailedEpoch`, treat as recovered.
    for cf in CRED_FILES.iter().filter(|c| c.is_core_token) {
        let p = sdir.join(cf.slot_name);
        if let Ok(meta) = fs::metadata(&p) {
            if let Ok(mtime) = meta.modified() {
                if let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH) {
                    if dur.as_secs() as i64 > auth_ep {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// Check whether a slot is structurally healthy and eligible as an auto-switch candidate.
pub fn check_slot_health(
    root: &Path,
    state: &Value,
    quota_cache: &Value,
    id: &str,
) -> Result<(), String> {
    if state["accounts"].get(id).is_none() {
        return Err("原槽位已删除".to_string());
    }
    let sdir = slot_dir(root, id);
    if !sdir.is_dir() {
        return Err("原槽位已删除".to_string());
    }
    if !slot_has_core_token(&sdir) {
        return Err("凭据失效".to_string());
    }
    let expected_email = s(&state["accounts"][id], "email").trim().to_string();
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
                        return Err("身份校验失败".to_string());
                    }
                }
            }
        }
        if cf.is_core_token && cred_file_valid_for_email(cf, &bytes, &expected_email) {
            valid_core += 1;
        }
    }
    if valid_core == 0 {
        return Err("凭据失效".to_string());
    }
    let acct_quota = &quota_cache["accounts"][id];
    if acct_quota.is_object() {
        if b(acct_quota, "identityMismatch") {
            return Err("身份校验失败".to_string());
        }
        if slot_is_auth_failed(&sdir, acct_quota) {
            return Err("登录失效(401)".to_string());
        }
    }
    Ok(())
}

pub(crate) fn extract_pool_window_history(
    acct_quota: &Value,
    pool: QuotaPool,
    window: QuotaWindow,
) -> Vec<(i64, f64, Option<i64>)> {
    let (win_key, reset_key) = match window {
        QuotaWindow::FiveHour => ("fiveHour", "fiveHourResetEp"),
        QuotaWindow::Weekly => ("weekly", "weeklyResetEp"),
    };
    let mut pts: Vec<(i64, f64, Option<i64>)> = Vec::new();
    let Some(hist) = acct_quota["history"].as_array() else {
        return pts;
    };
    for item in hist {
        let ep = item["epoch"].as_i64().unwrap_or(0);
        if ep <= 0 {
            continue;
        }
        let mut pct_opt: Option<f64> = None;
        let mut rep_opt: Option<i64> = None;
        if let Some(pools_obj) = item["pools"].as_object() {
            for (pname, pval) in pools_obj {
                if classify_quota_pool_name(pname) != Some(pool) {
                    continue;
                }
                if let Some(v) = pval[win_key].as_f64().filter(|v| v.is_finite()) {
                    let take = pct_opt.is_none_or(|cur| v < cur);
                    if take {
                        pct_opt = Some(v);
                        rep_opt = pval[reset_key].as_i64();
                    }
                }
            }
        }
        if let Some(pct) = pct_opt {
            pts.push((ep, pct, rep_opt));
        }
    }
    pts.sort_by_key(|(ep, _, _)| *ep);
    pts
}

#[allow(clippy::too_many_arguments)]
fn compute_window_eta(
    acct_quota: &Value,
    pool: QuotaPool,
    window: QuotaWindow,
    obs_meta: Option<(f64, i64, Option<i64>)>,
    eff_pct_override: Option<f64>,
    passive_exhausted: bool,
    passive_reset_ep: Option<i64>,
    k_run: usize,
    is_selected_for_pool: bool,
    now_ep: i64,
) -> QuotaEta {
    if passive_exhausted && window == QuotaWindow::FiveHour {
        let reset_in = passive_reset_ep
            .map(|r| (r - now_ep).max(0))
            .unwrap_or(PASSIVE_SYNTH_RESET_SECS);
        if reset_in == 0 {
            return QuotaEta::Insufficient {
                reason: "reset_passed",
                elapsed_mins: None,
            };
        }
        return QuotaEta::DepletesIn {
            fast_m: 0,
            mid_m: 0,
            slow_m: Some(0),
            alert: true,
            basis_label: "passive".to_string(),
        };
    }

    let Some((raw_pct, entry_ep, reset_ep_opt)) = obs_meta else {
        return QuotaEta::Insufficient {
            reason: "no_data",
            elapsed_mins: None,
        };
    };

    // Step ①: resetAt truncation & freshness gate.
    let Some(rep) = reset_ep_opt else {
        return QuotaEta::Insufficient {
            reason: "no_reset_at",
            elapsed_mins: None,
        };
    };
    if rep <= 0 {
        return if k_run == 0 {
            QuotaEta::Idle
        } else {
            QuotaEta::Insufficient {
                reason: "not_started",
                elapsed_mins: Some(0),
            }
        };
    }
    if now_ep >= rep + RESET_AT_BUFFER_SECS {
        return QuotaEta::Insufficient {
            reason: "reset_passed",
            elapsed_mins: None,
        };
    }
    if now_ep >= rep {
        return QuotaEta::SafeBeforeReset {
            basis_label: "window".to_string(),
        };
    }
    let reset_in_secs = (rep - now_ep).max(0);
    if entry_ep <= 0 {
        return QuotaEta::Insufficient {
            reason: "no_data",
            elapsed_mins: None,
        };
    }
    let age_secs = (now_ep - entry_ep).max(0);
    let max_stale = match window {
        QuotaWindow::FiveHour => FIVE_HOUR_STALE_SECS,
        QuotaWindow::Weekly => ETA_WEEKLY_STALE_SECS,
    };
    if age_secs > max_stale {
        return QuotaEta::Insufficient {
            reason: "stale",
            elapsed_mins: None,
        };
    }

    // Already at 0% before reset.
    if raw_pct <= 0.0 {
        if k_run == 0 && !is_selected_for_pool {
            return QuotaEta::Idle;
        }
        return QuotaEta::DepletesIn {
            fast_m: 0,
            mid_m: 0,
            slow_m: Some(0),
            alert: k_run > 0 || is_selected_for_pool,
            basis_label: "window".to_string(),
        };
    }

    let window_secs = match window {
        QuotaWindow::FiveHour => FIVE_HOUR_WINDOW_SECS,
        QuotaWindow::Weekly => WEEKLY_WINDOW_SECS,
    };
    let t_start = rep - window_secs;
    let elapsed_obs_secs = (entry_ep - t_start).max(0);
    let elapsed_wall_mins = ((now_ep - t_start).max(0) / 60) as u64;

    // Step ②: Same-cycle history & insufficient-data gate.
    match window {
        QuotaWindow::FiveHour => {
            if elapsed_obs_secs < 25 * 60 {
                return QuotaEta::Insufficient {
                    reason: "too_early",
                    elapsed_mins: Some(elapsed_wall_mins),
                };
            }
        }
        QuotaWindow::Weekly => {
            if elapsed_obs_secs < 12 * 3600 && raw_pct > 35.0 {
                return QuotaEta::Insufficient {
                    reason: "too_early",
                    elapsed_mins: Some(elapsed_wall_mins),
                };
            }
        }
    }

    let mut obs: Vec<(i64, f64, Option<i64>)> = extract_pool_window_history(acct_quota, pool, window)
        .into_iter()
        .filter(|(ep, _, _)| *ep <= entry_ep)
        .collect();
    if let Some(last) = obs.last_mut() {
        if (entry_ep - last.0).abs() <= 10 {
            *last = (entry_ep, raw_pct, Some(rep));
        } else {
            obs.push((entry_ep, raw_pct, Some(rep)));
        }
    } else {
        obs.push((entry_ep, raw_pct, Some(rep)));
    }

    let mut cycle_rev: Vec<(i64, f64)> = vec![(entry_ep, raw_pct)];
    let mut next_pct = raw_pct;
    for &(ep, pct, pt_rep) in obs.iter().rev().skip(1) {
        if ep < t_start - SAME_CYCLE_TOLERANCE_SECS {
            break;
        }
        match pt_rep {
            Some(r) => {
                if r <= 0 || (r - rep).abs() > SAME_CYCLE_TOLERANCE_SECS {
                    break;
                }
            }
            None => {
                if ep < t_start || pct < next_pct - 1.5 {
                    break;
                }
            }
        }
        cycle_rev.push((ep, pct));
        next_pct = pct;
    }
    cycle_rev.reverse();

    let recent_step_drop = if cycle_rev.len() >= 2 {
        Some(cycle_rev[cycle_rev.len() - 2].1 - raw_pct)
    } else {
        Some((100.0 - raw_pct).max(0.0))
    };

    if cycle_rev.is_empty() || t_start < cycle_rev[0].0 {
        cycle_rev.insert(0, (t_start, 100.0));
    }

    // Step ③: Idle gate (respects multi-machine remote drop when local k_run == 0).
    let no_recent_drop = recent_step_drop.is_none_or(|d| d <= 0.0);
    if k_run == 0 && (no_recent_drop || age_secs >= 20 * 60 || !is_selected_for_pool) {
        return QuotaEta::Idle;
    }

    // Step ④: Recent-window slope + uncertainty interval [T_fast, T_slow].
    let (anc_dt, anc_pct, basis_label) = match window {
        QuotaWindow::FiveHour => {
            let best = cycle_rev
                .iter()
                .filter_map(|&(ep, pct)| {
                    let dt = entry_ep - ep;
                    (20 * 60..=60 * 60)
                        .contains(&dt)
                        .then_some(((dt - 30 * 60).abs(), dt, pct))
                })
                .min_by_key(|(dist, _, _)| *dist)
                .or_else(|| {
                    cycle_rev
                        .iter()
                        .filter_map(|&(ep, pct)| {
                            let dt = entry_ep - ep;
                            (dt >= 20 * 60).then_some((dt, dt, pct))
                        })
                        .min_by_key(|(dt, _, _)| *dt)
                });
            let Some((_, dt, pct)) = best else {
                return QuotaEta::Insufficient {
                    reason: "too_early",
                    elapsed_mins: Some(elapsed_wall_mins),
                };
            };
            let lbl = if dt <= 45 * 60 {
                "30m"
            } else if dt <= 90 * 60 {
                "1h"
            } else {
                "window"
            };
            (dt, pct, lbl.to_string())
        }
        QuotaWindow::Weekly => {
            let best = cycle_rev
                .iter()
                .filter_map(|&(ep, pct)| {
                    let dt = entry_ep - ep;
                    (dt >= 6 * 3600).then_some(((dt - 24 * 3600).abs(), dt, pct))
                })
                .min_by_key(|(dist, _, _)| *dist)
                .or_else(|| {
                    cycle_rev
                        .iter()
                        .filter_map(|&(ep, pct)| {
                            let dt = entry_ep - ep;
                            (dt >= 30 * 60).then_some((dt, dt, pct))
                        })
                        .max_by_key(|(dt, _, _)| *dt)
                });
            let Some((_, dt, pct)) = best else {
                return QuotaEta::Insufficient {
                    reason: "too_early",
                    elapsed_mins: Some(elapsed_wall_mins),
                };
            };
            let lbl = if dt >= 18 * 3600 { "24h" } else { "window" };
            (dt, pct, lbl.to_string())
        }
    };

    let drop = (anc_pct - raw_pct).max(0.0);
    if window == QuotaWindow::Weekly && drop < 3.0 {
        return QuotaEta::SafeBeforeReset { basis_label };
    }
    let dt_h = (anc_dt as f64 / 3600.0).max(1.0 / 60.0);
    let r_rec = drop / dt_h;
    if r_rec <= 0.0 || !r_rec.is_finite() {
        return QuotaEta::SafeBeforeReset { basis_label };
    }

    let elapsed_h = (elapsed_obs_secs as f64 / 3600.0).max(1.0 / 60.0);
    let r_win = (100.0 - raw_pct).max(0.0) / elapsed_h;
    let age_h = age_secs as f64 / 3600.0;
    let reset_in_h = reset_in_secs as f64 / 3600.0;

    let t_mid_h = (raw_pct / r_rec - age_h).max(0.0);
    if t_mid_h >= reset_in_h {
        return QuotaEta::SafeBeforeReset { basis_label };
    }

    let eff_pct = eff_pct_override.unwrap_or(raw_pct);
    let r_fast = (r_rec + 1.0 / dt_h).max(r_win).max(r_rec);
    let t_fast_h = (eff_pct / r_fast).max(0.0).min(t_mid_h);

    let r_slow_floor = match window {
        QuotaWindow::FiveHour => 0.2,
        QuotaWindow::Weekly => 0.05,
    };
    let r_slow_rec = ((drop - 1.0).max(0.0) / dt_h).max(r_slow_floor);
    let r_slow = if r_win > 0.0 {
        r_slow_rec.min(r_win.max(r_slow_floor))
    } else {
        r_slow_rec
    };
    let t_slow_h = (raw_pct / r_slow - age_h).max(t_mid_h);

    let reset_in_mins = (reset_in_secs / 60).max(1) as u64;
    let cap_mins = reset_in_mins.saturating_sub(1);
    let mid_m = ((t_mid_h * 60.0).round().max(0.0) as u64).min(cap_mins);
    let fast_m = ((t_fast_h * 60.0).round().max(0.0) as u64).min(mid_m);
    let slow_m = if t_slow_h >= reset_in_h {
        None
    } else {
        Some(((t_slow_h * 60.0).round().max(mid_m as f64) as u64).min(cap_mins))
    };

    // Step ⑤: High-margin anti-false-alarm gate.
    let under_reset_margin = t_mid_h < 0.85 * reset_in_h;
    let under_pct_gate = match window {
        QuotaWindow::FiveHour => eff_pct <= 50.0 || (raw_pct <= 75.0 && r_rec >= 40.0),
        QuotaWindow::Weekly => raw_pct <= 40.0,
    };
    let alert = k_run > 0 && under_reset_margin && under_pct_gate;

    QuotaEta::DepletesIn {
        fast_m,
        mid_m,
        slow_m,
        alert,
        basis_label,
    }
}

/// Unified single-pool quota evaluator shared by `auto` account selection and depletion forecasting:
/// - Auto-switch reads `tier` (`Green` / `Yellow` / `Red`), `eff_five_hour_pct`, `weekly_pct`, `unknown`
///   using the unchanged 5h/7d staleness (`2h` / `24h`), `resetAt + 60s` buffer, `passive_error`, and `5% * k_write` reserve.
/// - Depletion forecasting reads `eta_five_hour` / `eta_weekly`, computed purely from same-cycle `/usage`
///   percentage history (`resetAt`-isolated), local running counts (`k_write`, `k_run`), and `12h` weekly ETA freshness.
pub fn evaluate_pool_quota(
    acct_quota: &Value,
    pool: QuotaPool,
    cfg: &AgyAutoSwitchConfig,
    running_writers: usize,
    running_all: usize,
    is_selected_for_pool: bool,
    now_ep: i64,
) -> PoolQuotaState {
    let acct_fetched_ep = acct_quota["fetchedEpoch"].as_i64().unwrap_or(0);
    let cache_age_secs = (acct_fetched_ep > 0).then(|| (now_ep - acct_fetched_ep).max(0));

    let mut raw_5h: Option<f64> = None;
    let mut weekly: Option<f64> = None;
    let mut passive_exhausted = false;
    let mut passive_reset_ep: Option<i64> = None;

    // Track full observation metadata `(pct, entry_ep, reset_ep)` for ETA (before stale/reset skip).
    let mut eta_obs_5h: Option<(f64, i64, Option<i64>)> = None;
    let mut eta_obs_7d: Option<(f64, i64, Option<i64>)> = None;

    if let Some(entries) = acct_quota["entries"].as_array() {
        for entry in entries {
            if classify_quota_pool_name(s(entry, "pool")) != Some(pool) {
                continue;
            }
            let Some(pct) = entry["remainingPct"].as_f64().filter(|v| v.is_finite()) else {
                continue;
            };
            let is_passive = s(entry, "source") == "passive_error";
            let entry_ep = entry["fetchedEpoch"]
                .as_i64()
                .filter(|&e| e > 0)
                .unwrap_or(acct_fetched_ep);
            let reset_at = s(entry, "resetAt");
            let reset_ep = (!reset_at.is_empty()).then(|| iso_epoch(reset_at)).flatten();

            if is_passive {
                let expired = match reset_ep {
                    Some(rep) => now_ep >= rep + RESET_AT_BUFFER_SECS,
                    None => entry_ep > 0 && now_ep >= entry_ep + PASSIVE_SYNTH_RESET_SECS + RESET_AT_BUFFER_SECS,
                };
                if !expired {
                    passive_exhausted = true;
                    raw_5h = Some(0.0);
                    passive_reset_ep = reset_ep.or_else(|| {
                        (entry_ep > 0).then_some(entry_ep + PASSIVE_SYNTH_RESET_SECS)
                    });
                }
                continue;
            }

            match s(entry, "window") {
                "fiveHour" if eta_obs_5h.is_none_or(|(prev, _, _)| pct < prev) => {
                    eta_obs_5h = Some((pct, entry_ep, reset_ep));
                }
                "weekly" if eta_obs_7d.is_none_or(|(prev, _, _)| pct < prev) => {
                    eta_obs_7d = Some((pct, entry_ep, reset_ep));
                }
                _ => {}
            }

            // Past resetAt (+ 60s clock-skew buffer) -> treat window as unknown (not hardcoded 100%).
            if let Some(rep) = reset_ep {
                if now_ep >= rep + RESET_AT_BUFFER_SECS {
                    continue;
                }
            }

            // Very stale non-passive data -> degrade to unknown so old numbers never trigger switches.
            if entry_ep <= 0 {
                continue;
            }
            let age = now_ep - entry_ep;
            match s(entry, "window") {
                "fiveHour" => {
                    if age > FIVE_HOUR_STALE_SECS {
                        continue;
                    }
                    raw_5h = Some(raw_5h.map_or(pct, |c| c.min(pct)));
                }
                "weekly" => {
                    if age > WEEKLY_STALE_SECS {
                        continue;
                    }
                    weekly = Some(weekly.map_or(pct, |c| c.min(pct)));
                }
                _ => {}
            }
        }
    }

    let eff_5h = raw_5h.map(|v| {
        if passive_exhausted {
            0.0
        } else {
            (v - cfg.per_run_reserve_pct * running_writers as f64).max(0.0)
        }
    });
    let unknown = !passive_exhausted && raw_5h.is_none() && weekly.is_none();

    let tier = if passive_exhausted {
        QuotaTier::Red
    } else if eff_5h.is_none_or(|v| v >= cfg.five_hour_min_pct)
        && weekly.is_none_or(|v| v >= cfg.weekly_min_pct)
    {
        QuotaTier::Green
    } else if raw_5h.is_none_or(|v| v >= YELLOW_FIVE_HOUR_MIN_PCT)
        && weekly.is_none_or(|v| v >= YELLOW_WEEKLY_MIN_PCT)
    {
        QuotaTier::Yellow
    } else {
        QuotaTier::Red
    };

    let reset_in_five_hour_secs = if passive_exhausted {
        passive_reset_ep.map(|r| (r - now_ep).max(0))
    } else {
        eta_obs_5h
            .and_then(|(_, _, rep)| rep)
            .filter(|&r| r > 0)
            .map(|r| (r - now_ep).max(0))
    };
    let reset_in_weekly_secs = eta_obs_7d
        .and_then(|(_, _, rep)| rep)
        .filter(|&r| r > 0)
        .map(|r| (r - now_ep).max(0));

    let eff_5h_for_eta = eff_5h.or_else(|| {
        eta_obs_5h.map(|(pct, _, _)| (pct - cfg.per_run_reserve_pct * running_writers as f64).max(0.0))
    });

    let eta_five_hour = compute_window_eta(
        acct_quota,
        pool,
        QuotaWindow::FiveHour,
        eta_obs_5h,
        eff_5h_for_eta,
        passive_exhausted,
        passive_reset_ep,
        running_all,
        is_selected_for_pool,
        now_ep,
    );
    let eta_weekly = compute_window_eta(
        acct_quota,
        pool,
        QuotaWindow::Weekly,
        eta_obs_7d,
        None,
        false,
        None,
        running_all,
        is_selected_for_pool,
        now_ep,
    );

    PoolQuotaState {
        tier,
        raw_five_hour_pct: raw_5h,
        eff_five_hour_pct: eff_5h,
        weekly_pct: weekly,
        observed_five_hour_pct: raw_5h.or_else(|| eta_obs_5h.map(|(p, _, _)| p)),
        observed_weekly_pct: weekly.or_else(|| eta_obs_7d.map(|(p, _, _)| p)),
        reset_in_five_hour_secs,
        reset_in_weekly_secs,
        running_writers,
        running_all,
        passive_exhausted,
        unknown,
        cache_age_secs,
        eta_five_hour,
        eta_weekly,
    }
}

/// Convenience wrapper for auto-switch tier evaluation (uses the same `PoolQuotaState`).
pub fn evaluate_slot_pool(
    acct_quota: &Value,
    pool: QuotaPool,
    cfg: &AgyAutoSwitchConfig,
    running_writers: usize,
    now_ep: i64,
) -> PoolQuotaState {
    evaluate_pool_quota(
        acct_quota,
        pool,
        cfg,
        running_writers,
        running_writers,
        true,
        now_ep,
    )
}

/// Count currently running `agy` tasks on this machine grouped by `(account_id, pool)`:
/// - `writers`: active write runs (`mode == "write"`, excluding `.waiting` and `.scouting`) — used for 5% reserve.
/// - `total`: all active `agy` runs (excluding `.waiting`) — used for ETA idle/alert gating.
///
/// Pure helper over an explicit run directory list (read-only; never mutates `.slot` files).
pub fn count_running_pool_tasks_in(
    runs: &[PathBuf],
    exclude_run: Option<&Path>,
) -> BTreeMap<(String, QuotaPool), RunningPoolCounts> {
    let mut counts: BTreeMap<(String, QuotaPool), RunningPoolCounts> = BTreeMap::new();
    for run in runs {
        if exclude_run.is_some_and(|ex| ex == run.as_path()) {
            continue;
        }
        if run.join(".waiting").exists() {
            continue;
        }
        let is_scouting = run.join(".scouting").exists();
        let meta = json(run.join("meta.json"));
        let model = s(&meta, "model");
        if !crate::agy::is_agy(model) && s(&meta, "agent") != crate::agy::BIN {
            continue;
        }
        let acct = s(&meta, "agyAccount").trim();
        if acct.is_empty() {
            continue;
        }
        if let Some(pool) = classify_model_pool(model) {
            let entry = counts.entry((acct.to_string(), pool)).or_default();
            entry.total += 1;
            if !is_scouting && s(&meta, "mode") == "write" {
                entry.writers += 1;
            }
        }
    }
    counts
}

pub fn count_running_pool_tasks(
    state_root: &Path,
    exclude_run: Option<&Path>,
) -> BTreeMap<(String, QuotaPool), RunningPoolCounts> {
    let runs = crate::launch::machine_runs(state_root);
    count_running_pool_tasks_in(&runs, exclude_run)
}

/// Count currently running `agy` write runs on this machine grouped by `(account_id, pool)`.
pub fn count_running_writers_in(
    runs: &[PathBuf],
    exclude_run: Option<&Path>,
) -> BTreeMap<(String, QuotaPool), usize> {
    count_running_pool_tasks_in(runs, exclude_run)
        .into_iter()
        .filter_map(|(k, c)| (c.writers > 0).then_some((k, c.writers)))
        .collect()
}

pub fn count_running_writers(
    state_root: &Path,
    exclude_run: Option<&Path>,
) -> BTreeMap<(String, QuotaPool), usize> {
    let runs = crate::launch::machine_runs(state_root);
    count_running_writers_in(&runs, exclude_run)
}

/// Recorded account switch (`auto`, `handoff`, or `fallback`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgyAccountSwitch {
    pub kind: String,
    pub from: String,
    pub to: String,
    pub reason: String,
    pub pool: Option<String>,
    pub from_quota: Option<Value>,
    pub to_quota: Option<Value>,
    pub cache_age_secs: Option<i64>,
}

/// Result of resolving an `agy` account slot and preparing its credential overlay.
pub struct PreparedAgyAccount {
    pub account_id: Option<String>,
    pub overlay: Option<AgyCredOverlay>,
    pub switched: Option<AgyAccountSwitch>,
}

#[derive(Debug, Clone)]
pub struct AutoSwitchDecision {
    pub selected_id: Option<String>,
    pub current_before: Option<String>,
    pub pool: Option<QuotaPool>,
    pub pointer_changed: bool,
    pub switched: Option<AgyAccountSwitch>,
    pub reason: String,
}

fn red_tiebreak_key(eval: &PoolQuotaState) -> (bool, i64) {
    if eval.passive_exhausted {
        return (false, 0);
    }
    let five_pos = eval.raw_five_hour_pct.unwrap_or(0.0) > 0.0;
    let min_pct = match (eval.raw_five_hour_pct, eval.weekly_pct) {
        (Some(f), Some(w)) => f.min(w),
        (Some(f), None) => f,
        (None, Some(w)) => w,
        (None, None) => 0.0,
    };
    (five_pos, (min_pct * 100.0).round() as i64)
}

fn slot_has_any_zero_entry(acct_quota: &Value, now_ep: i64) -> bool {
    let acct_fetched_ep = acct_quota["fetchedEpoch"].as_i64().unwrap_or(0);
    for entry in acct_quota["entries"].as_array().into_iter().flatten() {
        let Some(pct) = entry["remainingPct"].as_f64().filter(|v| v.is_finite()) else {
            continue;
        };
        let is_passive = s(entry, "source") == "passive_error";
        let entry_ep = entry["fetchedEpoch"]
            .as_i64()
            .filter(|&e| e > 0)
            .unwrap_or(acct_fetched_ep);
        let reset_at = s(entry, "resetAt");
        let reset_ep = (!reset_at.is_empty()).then(|| iso_epoch(reset_at)).flatten();
        if is_passive {
            let expired = match reset_ep {
                Some(rep) => now_ep >= rep + RESET_AT_BUFFER_SECS,
                None => entry_ep > 0 && now_ep >= entry_ep + PASSIVE_SYNTH_RESET_SECS + RESET_AT_BUFFER_SECS,
            };
            if !expired {
                return true;
            }
            continue;
        }
        if let Some(rep) = reset_ep {
            if now_ep >= rep + RESET_AT_BUFFER_SECS {
                continue;
            }
        }
        if entry_ep <= 0 {
            continue;
        }
        let age = now_ep - entry_ep;
        let stale = match s(entry, "window") {
            "fiveHour" => age > FIVE_HOUR_STALE_SECS,
            "weekly" => age > WEEKLY_STALE_SECS,
            _ => age > WEEKLY_STALE_SECS,
        };
        if !stale && pct <= 0.0 {
            return true;
        }
    }
    false
}

/// Pure three-tier hysteresis selector for a new `agy` session.
///
/// Rules:
/// - If `!cfg.enabled` or `<= 1` configured account: returns `active_id` with zero switches.
/// - Filters out unhealthy slots (deleted, missing creds, `identityMismatch`, `authFailed`).
/// - Known model pool (`Gemini` / `ClaudeGpt`):
///   - Starts from `poolCurrent[pool]` (defaulting to `active_id`).
///   - Evaluates tiers: Green (`eff_5h >= fiveHourMinPct && 7d >= weeklyMinPct`, or unknown) >
///     Yellow (`5h >= 5% && 7d >= 2%`, no passive_error) > Red (rest).
///   - Only upgrades pointer when another healthy slot has a STRICTLY higher tier (`tier(other) > tier(cur)`).
///   - Same tier (Green/Green or Yellow/Yellow) NEVER switches (full hysteresis, no ping-pong).
///   - Double-Red: switches only to a slot with strictly better `(5h > 0, min(5h, 7d))`.
/// - Unknown model: reads `active_id` (avoids 0% slot if another healthy non-0% exists) and
///   NEVER mutates `poolCurrent`.
#[allow(clippy::too_many_arguments)]
pub fn select_account_for_new_session(
    root: &Path,
    state: &Value,
    quota_cache: &Value,
    pool_current: &BTreeMap<QuotaPool, String>,
    running_writers: &BTreeMap<(String, QuotaPool), usize>,
    model: &str,
    cfg: &AgyAutoSwitchConfig,
    now_ep: i64,
) -> AutoSwitchDecision {
    let active_id = state["active"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let pool = classify_model_pool(model);

    let mut all_ids: Vec<String> = state["accounts"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    all_ids.sort();

    if !is_auto_mode_state(state) {
        let reason = format!(
            "当前为固定号模式（{}），不自动切号",
            active_id.as_deref().unwrap_or("none")
        );
        return AutoSwitchDecision {
            selected_id: active_id.clone(),
            current_before: active_id,
            pool,
            pointer_changed: false,
            switched: None,
            reason,
        };
    }

    if all_ids.len() <= 1 {
        return AutoSwitchDecision {
            selected_id: active_id.clone(),
            current_before: active_id,
            pool,
            pointer_changed: false,
            switched: None,
            reason: "仅配置 1 个账号，保持不变".to_string(),
        };
    }

    let healthy: Vec<String> = all_ids
        .iter()
        .filter(|id| check_slot_health(root, state, quota_cache, id).is_ok())
        .cloned()
        .collect();

    if healthy.is_empty() {
        return AutoSwitchDecision {
            selected_id: active_id.clone(),
            current_before: active_id,
            pool,
            pointer_changed: false,
            switched: None,
            reason: "无健康可用账号，沿用活跃号".to_string(),
        };
    }

    if healthy.len() == 1 {
        let only = healthy[0].clone();
        if active_id.as_deref() == Some(only.as_str()) {
            return AutoSwitchDecision {
                selected_id: Some(only.clone()),
                current_before: Some(only),
                pool,
                pointer_changed: false,
                switched: None,
                reason: "仅 1 个可用健康账号，保持不变".to_string(),
            };
        }
        let from = active_id.clone().unwrap_or_else(|| only.clone());
        return AutoSwitchDecision {
            selected_id: Some(only.clone()),
            current_before: Some(from.clone()),
            pool,
            pointer_changed: pool.is_some(),
            switched: Some(AgyAccountSwitch {
                kind: "auto".to_string(),
                from,
                to: only,
                reason: "仅 1 个健康可用号（原号不可用）".to_string(),
                pool: pool.map(|p| p.key().to_string()),
                from_quota: None,
                to_quota: None,
                cache_age_secs: None,
            }),
            reason: "仅 1 个健康可用号（原号不可用）".to_string(),
        };
    }

    // Unknown model: stay on `active_id` unless it is at 0%, and NEVER overwrite `poolCurrent`.
    let Some(pool) = pool else {
        let base = active_id
            .as_ref()
            .filter(|a| healthy.iter().any(|h| h == *a))
            .cloned()
            .unwrap_or_else(|| healthy[0].clone());
        let base_zero = slot_has_any_zero_entry(&quota_cache["accounts"][&base], now_ep);
        if !base_zero {
            return AutoSwitchDecision {
                selected_id: Some(base.clone()),
                current_before: Some(base),
                pool: None,
                pointer_changed: false,
                switched: None,
                reason: "未知模型池，沿用活跃号（不改池指针）".to_string(),
            };
        }
        if let Some(alt) = healthy
            .iter()
            .find(|id| *id != &base && !slot_has_any_zero_entry(&quota_cache["accounts"][id.as_str()], now_ep))
        {
            let reason = format!("未知模型池：{base} 存在 0% 池，改用 {alt}（不改池指针）");
            return AutoSwitchDecision {
                selected_id: Some(alt.clone()),
                current_before: Some(base.clone()),
                pool: None,
                pointer_changed: false,
                switched: Some(AgyAccountSwitch {
                    kind: "auto".to_string(),
                    from: base,
                    to: alt.clone(),
                    reason: reason.clone(),
                    pool: None,
                    from_quota: None,
                    to_quota: None,
                    cache_age_secs: None,
                }),
                reason,
            };
        }
        return AutoSwitchDecision {
            selected_id: Some(base.clone()),
            current_before: Some(base),
            pool: None,
            pointer_changed: false,
            switched: None,
            reason: "未知模型池，无更优非 0% 槽位，沿用活跃号".to_string(),
        };
    };

    // Known pool: resolve current pointer (defaulting to active_id if unset or unhealthy).
    let stored_ptr = pool_current.get(&pool).cloned();
    let current_id = stored_ptr
        .as_ref()
        .filter(|id| healthy.iter().any(|h| h == *id))
        .cloned()
        .or_else(|| {
            active_id
                .as_ref()
                .filter(|id| healthy.iter().any(|h| h == *id))
                .cloned()
        })
        .unwrap_or_else(|| healthy[0].clone());

    let eval_of = |id: &str| -> PoolQuotaState {
        let k = running_writers.get(&(id.to_string(), pool)).copied().unwrap_or(0);
        evaluate_slot_pool(&quota_cache["accounts"][id], pool, cfg, k, now_ep)
    };
    let cur_eval = eval_of(&current_id);

    // 1. Look for candidates with STRICTLY higher tier (`tier(other) > tier(current)`).
    let mut higher: Vec<(String, PoolQuotaState)> = healthy
        .iter()
        .filter(|id| *id != &current_id)
        .map(|id| (id.clone(), eval_of(id)))
        .filter(|(_, ev)| ev.tier > cur_eval.tier)
        .collect();

    if !higher.is_empty() {
        higher.sort_by(|(id_a, a), (id_b, b)| {
            b.tier
                .cmp(&a.tier)
                .then_with(|| (!b.unknown).cmp(&(!a.unknown)))
                .then_with(|| {
                    b.weekly_pct
                        .unwrap_or(-1.0)
                        .total_cmp(&a.weekly_pct.unwrap_or(-1.0))
                })
                .then_with(|| {
                    b.eff_five_hour_pct
                        .unwrap_or(-1.0)
                        .total_cmp(&a.eff_five_hour_pct.unwrap_or(-1.0))
                })
                .then_with(|| id_a.cmp(id_b))
        });
        let (best_id, best_eval) = higher.remove(0);
        let reason = format!(
            "{} {}档（{}），改用 {}档 {}（{}）",
            current_id,
            cur_eval.tier.label_zh(),
            cur_eval.format_short(),
            best_eval.tier.label_zh(),
            best_id,
            best_eval.format_short(),
        );
        let switched = AgyAccountSwitch {
            kind: "auto".to_string(),
            from: current_id.clone(),
            to: best_id.clone(),
            reason: reason.clone(),
            pool: Some(pool.key().to_string()),
            from_quota: Some(cur_eval.to_quota_json()),
            to_quota: Some(best_eval.to_quota_json()),
            cache_age_secs: cur_eval.cache_age_secs.or(best_eval.cache_age_secs),
        };
        return AutoSwitchDecision {
            selected_id: Some(best_id),
            current_before: Some(current_id),
            pool: Some(pool),
            pointer_changed: true,
            switched: Some(switched),
            reason,
        };
    }

    // 2. Both/all are Red: pick strictly better `(5h > 0, min(5h, 7d))`.
    if cur_eval.tier == QuotaTier::Red {
        let cur_key = red_tiebreak_key(&cur_eval);
        let mut better_red: Vec<(String, PoolQuotaState, (bool, i64))> = healthy
            .iter()
            .filter(|id| *id != &current_id)
            .map(|id| {
                let ev = eval_of(id);
                let key = red_tiebreak_key(&ev);
                (id.clone(), ev, key)
            })
            .filter(|(_, _, key)| *key > cur_key)
            .collect();
        if !better_red.is_empty() {
            better_red.sort_by(|(id_a, _, ka), (id_b, _, kb)| kb.cmp(ka).then_with(|| id_a.cmp(id_b)));
            let (best_id, best_eval, _) = better_red.remove(0);
            let reason = format!(
                "双红档：{current_id}（{}），改用 {best_id}（{}）",
                cur_eval.format_short(),
                best_eval.format_short(),
            );
            let switched = AgyAccountSwitch {
                kind: "auto".to_string(),
                from: current_id.clone(),
                to: best_id.clone(),
                reason: reason.clone(),
                pool: Some(pool.key().to_string()),
                from_quota: Some(cur_eval.to_quota_json()),
                to_quota: Some(best_eval.to_quota_json()),
                cache_age_secs: cur_eval.cache_age_secs.or(best_eval.cache_age_secs),
            };
            return AutoSwitchDecision {
                selected_id: Some(best_id),
                current_before: Some(current_id),
                pool: Some(pool),
                pointer_changed: true,
                switched: Some(switched),
                reason,
            };
        }
    }

    // 3. Same tier (Green/Green or Yellow/Yellow) or tied Red: stay on current pointer!
    let reason = format!(
        "维持当前号 {current_id}：{}档（{}），无严格更高档备选",
        cur_eval.tier.label_zh(),
        cur_eval.format_short(),
    );
    AutoSwitchDecision {
        selected_id: Some(current_id.clone()),
        current_before: Some(current_id),
        pool: Some(pool),
        pointer_changed: false,
        switched: None,
        reason,
    }
}

/// Choose a distinct healthy target slot for mid-run exhaustion handoff or reviewer full-template retry.
/// - When `require_non_red == true` (reviewer): only accepts Green or Yellow candidates.
/// - When `require_non_red == false` (writer/read-only): prefers Green > Yellow, or Red with `5h > 0`
///   and not passively exhausted.
#[allow(clippy::too_many_arguments)]
pub fn select_handoff_target_in(
    root: &Path,
    state: &Value,
    quota_cache: &Value,
    running_writers: &BTreeMap<(String, QuotaPool), usize>,
    from_id: &str,
    model: &str,
    cfg: &AgyAutoSwitchConfig,
    now_ep: i64,
    require_non_red: bool,
) -> Option<String> {
    if !is_auto_mode_state(state) {
        return None;
    }
    let mut all_ids: Vec<String> = state["accounts"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    all_ids.sort();
    let candidates: Vec<String> = all_ids
        .into_iter()
        .filter(|id| id != from_id && check_slot_health(root, state, quota_cache, id).is_ok())
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let Some(pool) = classify_model_pool(model) else {
        return candidates
            .into_iter()
            .find(|id| !slot_has_any_zero_entry(&quota_cache["accounts"][id.as_str()], now_ep));
    };
    let mut scored: Vec<(String, PoolQuotaState)> = candidates
        .into_iter()
        .map(|id| {
            let k = running_writers.get(&(id.clone(), pool)).copied().unwrap_or(0);
            let ev = evaluate_slot_pool(&quota_cache["accounts"][&id], pool, cfg, k, now_ep);
            (id, ev)
        })
        .filter(|(_, ev)| {
            if require_non_red {
                ev.tier > QuotaTier::Red
            } else {
                ev.tier > QuotaTier::Red
                    || (!ev.passive_exhausted && ev.raw_five_hour_pct.unwrap_or(0.0) > 0.0)
            }
        })
        .collect();
    if scored.is_empty() {
        return None;
    }
    scored.sort_by(|(id_a, a), (id_b, b)| {
        b.tier
            .cmp(&a.tier)
            .then_with(|| (!b.unknown).cmp(&(!a.unknown)))
            .then_with(|| {
                b.weekly_pct
                    .unwrap_or(-1.0)
                    .total_cmp(&a.weekly_pct.unwrap_or(-1.0))
            })
            .then_with(|| {
                b.eff_five_hour_pct
                    .unwrap_or(-1.0)
                    .total_cmp(&a.eff_five_hour_pct.unwrap_or(-1.0))
            })
            .then_with(|| id_a.cmp(id_b))
    });
    Some(scored.remove(0).0)
}

pub fn select_handoff_target(
    from_id: &str,
    model: &str,
    exclude_run: Option<&Path>,
    require_non_red: bool,
) -> Option<String> {
    let root = accounts_root();
    if !accounts_meta_path(&root).is_file() {
        return None;
    }
    let state_root = state_dir();
    let cfg = load_agy_auto_switch_config();
    let state = load_state(&root);
    let quota_cache = load_quota_cache_in(&state_root);
    let running = count_running_writers(&state_root, exclude_run);
    select_handoff_target_in(
        &root,
        &state,
        &quota_cache,
        &running,
        from_id,
        model,
        &cfg,
        epoch() as i64,
        require_non_red,
    )
}

/// Format a human-readable one-line notice for `status`, `card`, and TUI when a run
/// switched `agy` accounts (`auto`, `handoff`, or legacy `fallback`). Never prints emails.
pub fn format_account_switch_line(sw: &Value, is_reviewer: bool) -> Option<String> {
    if !sw.is_object() {
        return None;
    }
    let from = s(sw, "from");
    let to = s(sw, "to");
    let reason = s(sw, "reason");
    let kind = s(sw, "kind");
    if from.is_empty() || to.is_empty() {
        return None;
    }
    match kind {
        "auto" => {
            let prefix = if is_reviewer {
                "审查自动选号"
            } else {
                "自动选号"
            };
            if reason.is_empty() {
                Some(format!("{prefix}：{from} → {to}"))
            } else {
                Some(format!("{prefix}：{from} → {to}（{reason}）"))
            }
        }
        "handoff" => {
            let prefix = if is_reviewer {
                "审查换号重审"
            } else {
                "中途换号接力"
            };
            if reason.is_empty() {
                Some(format!("{prefix}：{from} → {to}"))
            } else {
                Some(format!("{prefix}：{from} → {to}（{reason}）"))
            }
        }
        _ => {
            let detail = match (is_reviewer, reason.is_empty()) {
                (true, true) => "审查员".to_string(),
                (true, false) => format!("审查员：{reason}"),
                (false, true) => "原槽位不可用".to_string(),
                (false, false) => reason.to_string(),
            };
            Some(format!("本 run 已从 {from} 换到 {to}（{detail}）"))
        }
    }
}

/// Check whether a pinned `agy` slot is structurally unavailable (deleted, missing credentials,
/// or identity mismatch). Quota 0% NEVER triggers a silent fallback on an already-started session
/// because sessions are bound to their account; quota exhaustion is handled via explicit mid-run
/// new-session handoff instead.
pub(crate) fn check_pinned_slot_unavailability(
    root: &Path,
    state: &Value,
    quota_cache: &Value,
    pinned_id: &str,
    _model: &str,
    _now_ep: i64,
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
    if acct_quota.is_object() && b(acct_quota, "identityMismatch") {
        return Some("身份校验失败".to_string());
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
    /// NEVER triggers automatic account selection or quota refresh (used by `probe` and
    /// explicit active-slot tooling).
    pub fn prepare_active() -> Res<(Option<String>, Option<Self>)> {
        let prep = Self::prepare_for_session_with_opts(None, "", false, None, None)?;
        Ok((prep.account_id, prep.overlay))
    }

    /// Prepare a private credential overlay for an explicit slot (or active if `None`),
    /// NEVER triggering automatic account selection or quota refresh (used by `AuthRefresh` probe).
    pub fn prepare_for_account(account_id: Option<&str>) -> Res<(Option<String>, Option<Self>)> {
        let pinned = account_id.map(str::trim).filter(|s| !s.is_empty());
        let prep = Self::prepare_for_session_with_opts(pinned, "", false, None, None)?;
        Ok((prep.account_id, prep.overlay))
    }

    pub fn prepare_for_session_with_opts(
        pinned_id: Option<&str>,
        model: &str,
        allow_auto_switch: bool,
        preferred_account: Option<&str>,
        current_run_dir: Option<&Path>,
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
        let state_root = state_dir();
        let cfg = load_agy_auto_switch_config();
        let pinned = pinned_id.map(str::trim).filter(|s| !s.is_empty());
        let preferred = preferred_account.map(str::trim).filter(|s| !s.is_empty());

        // Lazy singleflight quota refresh BEFORE taking `accounts/.lock` (only for new sessions
        // in `auto` mode with >= 2 configured slots).
        if pinned.is_none() && preferred.is_none() && allow_auto_switch {
            let pre_state = load_state(&root);
            let slot_count = pre_state["accounts"].as_object().map_or(0, Map::len);
            if is_auto_mode_state(&pre_state) && slot_count >= 2 {
                let pool_cur = load_pool_current_in(&state_root);
                let running = count_running_writers(&state_root, current_run_dir);
                let now_ep = epoch() as i64;
                let min_secs = (cfg.refresh_minutes as i64 * 60).max(60);
                let pool = classify_model_pool(model);
                let active_id = pre_state["active"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let current_id = pool
                    .and_then(|p| pool_cur.get(&p).cloned())
                    .filter(|id| pre_state["accounts"].get(id.as_str()).is_some())
                    .or(active_id);

                let pre_cache = load_quota_cache_in(&state_root);
                let current_is_fresh_green = if let (Some(ref cid), Some(p)) = (&current_id, pool) {
                    let cur_opts = QuotaRefreshOptions::auto_stale(min_secs, Some(vec![cid.clone()]));
                    let fresh = !slot_needs_refresh(&root, &pre_cache, cid, now_ep, &cur_opts);
                    let healthy = check_slot_health(&root, &pre_state, &pre_cache, cid).is_ok();
                    let k = running.get(&(cid.clone(), p)).copied().unwrap_or(0);
                    let eval = evaluate_slot_pool(&pre_cache["accounts"][cid], p, &cfg, k, now_ep);
                    fresh && healthy && eval.tier == QuotaTier::Green
                } else {
                    false
                };

                if !current_is_fresh_green {
                    // Step 1: refresh current slot first if stale.
                    if let Some(ref cid) = current_id {
                        let cur_opts = QuotaRefreshOptions::auto_stale(min_secs, Some(vec![cid.clone()]));
                        let _ = refresh_quotas_singleflight(None, &cur_opts);
                    }
                    // Step 2: check if current slot is now Green; if not, refresh remaining slots.
                    let after_cur_cache = load_quota_cache_in(&state_root);
                    let after_now = epoch() as i64;
                    let now_green = if let (Some(ref cid), Some(p)) = (&current_id, pool) {
                        let healthy =
                            check_slot_health(&root, &pre_state, &after_cur_cache, cid).is_ok();
                        let k = running.get(&(cid.clone(), p)).copied().unwrap_or(0);
                        let eval = evaluate_slot_pool(
                            &after_cur_cache["accounts"][cid],
                            p,
                            &cfg,
                            k,
                            after_now,
                        );
                        healthy && eval.tier == QuotaTier::Green
                    } else {
                        false
                    };
                    if !now_green {
                        let all_opts = QuotaRefreshOptions::auto_stale(min_secs, None);
                        let _ = refresh_quotas_singleflight(None, &all_opts);
                    }
                }
            }
        }

        let quota_cache = load_quota_cache_in(&state_root);
        let pool_cur = load_pool_current_in(&state_root);
        let running = count_running_writers(&state_root, current_run_dir);
        let (prep, pointer_update) = Self::prepare_for_session_full(
            &root,
            &real_home,
            &real_xdg_config,
            pinned,
            preferred,
            model,
            allow_auto_switch,
            &cfg,
            &quota_cache,
            &pool_cur,
            &running,
            epoch() as i64,
        )?;
        if let Some((pool, new_id)) = pointer_update {
            let _ = set_pool_current_in(&state_root, pool, &new_id);
        }
        Ok(prep)
    }

    #[cfg(test)]
    fn prepare_active_for(
        root: &Path,
        real_home: &Path,
        real_xdg_config: &Path,
    ) -> Res<(Option<String>, Option<Self>)> {
        let empty_quota = json!({"accounts": {}});
        let (prep, _) = Self::prepare_for_session_full(
            root,
            real_home,
            real_xdg_config,
            None,
            None,
            "",
            false,
            &AgyAutoSwitchConfig::default(),
            &empty_quota,
            &BTreeMap::new(),
            &BTreeMap::new(),
            epoch() as i64,
        )?;
        Ok((prep.account_id, prep.overlay))
    }

    #[cfg(test)]
    fn prepare_for_session_in(
        root: &Path,
        real_home: &Path,
        real_xdg_config: &Path,
        pinned_id: Option<&str>,
        model: &str,
        quota_cache: &Value,
        now_ep: i64,
    ) -> Res<PreparedAgyAccount> {
        let (prep, _) = Self::prepare_for_session_full(
            root,
            real_home,
            real_xdg_config,
            pinned_id,
            None,
            model,
            false,
            &AgyAutoSwitchConfig::default(),
            quota_cache,
            &BTreeMap::new(),
            &BTreeMap::new(),
            now_ep,
        )?;
        Ok(prep)
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_for_session_full(
        root: &Path,
        real_home: &Path,
        real_xdg_config: &Path,
        pinned_id: Option<&str>,
        preferred_account: Option<&str>,
        model: &str,
        allow_auto_switch: bool,
        auto_cfg: &AgyAutoSwitchConfig,
        quota_cache: &Value,
        pool_current: &BTreeMap<QuotaPool, String>,
        running_writers: &BTreeMap<(String, QuotaPool), usize>,
        now_ep: i64,
    ) -> Res<(PreparedAgyAccount, Option<(QuotaPool, String)>)> {
        let _lock = acquire_accounts_lock(root)?;
        let mut state = load_state(root);
        let active_id = state["active"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let pinned = pinned_id.map(str::trim).filter(|s| !s.is_empty());
        let preferred = preferred_account.map(str::trim).filter(|s| !s.is_empty());
        let mut pointer_update: Option<(QuotaPool, String)> = None;

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
                            kind: "fallback".to_string(),
                            from: pin.to_string(),
                            to: act,
                            reason,
                            pool: classify_model_pool(model).map(|p| p.key().to_string()),
                            from_quota: None,
                            to_quota: None,
                            cache_age_secs: None,
                        }),
                    )
                } else if state["accounts"].get(pin).is_some()
                    && slot_has_core_token(&slot_dir(root, pin))
                {
                    // No distinct active account to fall back to; stay on pinned slot.
                    (Some(pin.to_string()), None)
                } else {
                    (None, None)
                }
            } else {
                (Some(pin.to_string()), None)
            }
        } else if let Some(pref) = preferred {
            if state["accounts"].get(pref).is_some() && slot_has_core_token(&slot_dir(root, pref)) {
                (Some(pref.to_string()), None)
            } else {
                (active_id.clone(), None)
            }
        } else if allow_auto_switch && is_auto_mode_state(&state) {
            let decision = select_account_for_new_session(
                root,
                &state,
                quota_cache,
                pool_current,
                running_writers,
                model,
                auto_cfg,
                now_ep,
            );
            if decision.pointer_changed {
                if let (Some(p), Some(ref sel)) = (decision.pool, &decision.selected_id) {
                    pointer_update = Some((p, sel.clone()));
                }
            }
            (decision.selected_id, decision.switched)
        } else {
            (active_id.clone(), None)
        };

        let Some(selected_id) = selected_id else {
            return Ok((
                PreparedAgyAccount {
                    account_id: None,
                    overlay: None,
                    switched: None,
                },
                pointer_update,
            ));
        };
        let sdir = slot_dir(root, &selected_id);
        if !slot_has_core_token(&sdir) {
            return Ok((
                PreparedAgyAccount {
                    account_id: Some(selected_id),
                    overlay: None,
                    switched,
                },
                pointer_update,
            ));
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
        Ok((
            PreparedAgyAccount {
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
            },
            pointer_update,
        ))
    }

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

pub(crate) fn resolve_agy_bin(explicit: Option<&str>) -> Res<PathBuf> {
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

fn format_running_agy_breakdown(snap: &Value) -> String {
    let running = snap["runningAgy"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let n = running.len();
    if n == 0 {
        return "在跑 0 个".to_string();
    }
    let active = snap["active"].as_str().unwrap_or("");
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for p in running {
        let acct = p["account"]
            .as_str()
            .filter(|a| !a.is_empty())
            .or_else(|| (!active.is_empty()).then_some(active))
            .unwrap_or("未知");
        *counts.entry(acct.to_string()).or_insert(0) += 1;
    }
    let mut pairs: Vec<(String, usize)> = counts.into_iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let items: Vec<String> = pairs
        .into_iter()
        .map(|(id, k)| format!("{id} ×{k}"))
        .collect();
    format!("在跑 {n} 个：{}", items.join("，"))
}

fn pool_selected_id(snap: &Value, pool: QuotaPool) -> String {
    let ps = &snap["autoSwitch"]["poolSummary"][pool.key()];
    let sel = s(ps, "selected");
    if !sel.is_empty() {
        return sel.to_string();
    }
    let cur = s(ps, "current");
    if !cur.is_empty() {
        return cur.to_string();
    }
    let pc = s(&snap["autoSwitch"]["poolCurrent"], pool.key());
    if !pc.is_empty() {
        return pc.to_string();
    }
    "-".to_string()
}

fn format_delegation_selection_line(snap: &Value) -> String {
    let is_auto = s(snap, "mode") == AUTO_ACCOUNT_ID;
    let running_desc = format_running_agy_breakdown(snap);
    if is_auto {
        let gem = pool_selected_id(snap, QuotaPool::Gemini);
        let cg = pool_selected_id(snap, QuotaPool::ClaudeGpt);
        format!("委派选号：auto · Gemini → {gem} · Claude/GPT → {cg}（{running_desc}）")
    } else {
        let active = snap["active"].as_str().unwrap_or("");
        let slot = if active.is_empty() { "无" } else { active };
        format!("委派选号：固定 {slot}（{running_desc}）")
    }
}

fn format_live_slot_line(snap: &Value) -> String {
    let is_auto = s(snap, "mode") == AUTO_ACCOUNT_ID;
    let active = snap["active"].as_str().unwrap_or("");
    let slot = if active.is_empty() { "无" } else { active };
    if is_auto {
        format!("本机实盘：{slot}（只给手动运行的 agy 用，auto 不改它）")
    } else {
        format!("本机实盘：{slot}")
    }
}

fn cmd_list(args: &[String]) -> Res<i32> {
    let usage = "delegate agy account list [--quota|--quota-if-stale] [--agy-bin PATH] [--json]";
    let (pos, flags, kv) =
        parse_simple(usage, args, &["--json", "--quota", "--quota-if-stale"], &["--agy-bin"])?;
    if !pos.is_empty() {
        return Err(unrecognized_arg(
            &pos.join(" "),
            &["--json", "--quota", "--quota-if-stale", "--agy-bin"],
            usage,
        ));
    }
    if has(&flags, "--quota") {
        refresh_all_quotas(value(&kv, "--agy-bin"))?;
    } else if has(&flags, "--quota-if-stale") {
        refresh_stale_quotas(value(&kv, "--agy-bin"), None)?;
    }
    let snap = snapshot_json();
    if has(&flags, "--json") {
        println!("{snap}");
        return Ok(0);
    }
    let accounts = snap["accounts"].as_array().cloned().unwrap_or_default();
    let running = snap["runningAgy"].as_array().map_or(0, Vec::len);
    let active = snap["active"].as_str().unwrap_or("");
    let is_auto = s(&snap, "mode") == AUTO_ACCOUNT_ID;
    if accounts.is_empty() {
        println!("No agy accounts configured ({}).", s(&snap, "root"));
        println!("Next: delegate agy account add [<id>] | delegate agy account import-agy-swap --dry-run");
        return Ok(0);
    }
    let age_hdr = snap["quotaAgeSeconds"]
        .as_i64()
        .map(|a| format!(", quota age: {}", format_age_compact(a)))
        .unwrap_or_default();
    println!("{}", format_delegation_selection_line(&snap));
    println!("{}", format_live_slot_line(&snap));
    println!(
        "agy accounts ({} + auto, running agy: {}{age_hdr}):",
        accounts.len(),
        running
    );
    for item in &accounts {
        let mark = if !is_auto && b(item, "active") { "*" } else { " " };
        let id = s(item, "id");
        let email = s(item, "email");
        let last_used = s(item, "lastUsedAt");
        let last_str = if last_used.is_empty() { "-" } else { last_used };
        let files = item["fileCount"].as_u64().unwrap_or(0);
        let label = s(item, "label");
        let mut label_part = if label.is_empty() {
            String::new()
        } else {
            format!(" [{label}]")
        };
        if is_auto && b(item, "active") {
            label_part.push_str(" [实盘]");
        }
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
    // Always list the virtual `auto` slot row and mark `*` when selected.
    let auto_mark = if is_auto { "*" } else { " " };
    let mut pool_bits = Vec::new();
    for pool in QuotaPool::ALL {
        let ps = &snap["autoSwitch"]["poolSummary"][pool.key()];
        let sel = s(ps, "selected");
        let tier = s(ps, "tierLabel");
        if !sel.is_empty() {
            pool_bits.push(format!("{}→{sel}({tier})", pool.short_label()));
        }
    }
    let pools_desc = if pool_bits.is_empty() {
        "按池自动选号".to_string()
    } else {
        pool_bits.join(" ")
    };
    let auto_tail = if is_auto {
        String::new()
    } else {
        "  (执行 `use auto` 开启)".to_string()
    };
    println!("  {auto_mark} {:<16} [自动选号] {pools_desc}{auto_tail}", AUTO_ACCOUNT_ID);
    if b(&snap, "liveDrifted") {
        println!(
            "note: live ~/.gemini account ({}) differs from active slot ({})",
            s(&snap, "liveEmail"),
            if active.is_empty() { "none" } else { active }
        );
    }
    print_duplicate_quota_warnings(&snap);
    println!("Next: delegate agy account use <id|auto> | delegate agy account pick | delegate agy account status --quota");
    Ok(0)
}

fn cmd_status(args: &[String]) -> Res<i32> {
    let usage = "delegate agy account status [--quota|--quota-if-stale] [--agy-bin PATH] [--json]";
    let (pos, flags, kv) =
        parse_simple(usage, args, &["--json", "--quota", "--quota-if-stale"], &["--agy-bin"])?;
    if !pos.is_empty() {
        return Err(unrecognized_arg(
            &pos.join(" "),
            &["--json", "--quota", "--quota-if-stale", "--agy-bin"],
            usage,
        ));
    }
    if has(&flags, "--quota") {
        refresh_all_quotas(value(&kv, "--agy-bin"))?;
    } else if has(&flags, "--quota-if-stale") {
        refresh_stale_quotas(value(&kv, "--agy-bin"), None)?;
    }
    let snap = snapshot_json();
    if has(&flags, "--json") {
        println!("{snap}");
        return Ok(0);
    }
    let live = s(&snap, "liveEmail");
    let accounts = snap["accounts"].as_array().cloned().unwrap_or_default();
    let running = snap["runningAgy"].as_array().cloned().unwrap_or_default();
    println!("{}", format_delegation_selection_line(&snap));
    println!("{}", format_live_slot_line(&snap));
    if !live.is_empty() {
        println!("live ~/.gemini email: {live}");
    }
    if b(&snap, "liveDrifted") {
        println!("warning: live credentials differ from active slot");
    }
    print_duplicate_quota_warnings(&snap);
    if let Some(cur) = accounts.iter().find(|a| b(a, "active")) {
        let cur_id = s(cur, "id");
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
                println!("{cur_id} 的实时额度（agy -p /usage，{age}）");
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
                println!("{cur_id} 的实时额度（agy -p /usage，{age}）");
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
                if let Some(line) = format_cli_quota_estimate_line(label, &cur["quotaEstimate"][key]) {
                    println!("{line}");
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
        let run_opt = p["run"].as_str().filter(|r| !r.is_empty());
        let acct_opt = p["account"].as_str().filter(|a| !a.is_empty());
        match (run_opt, acct_opt) {
            (Some(run), Some(acct)) => {
                println!("  pid={pid} (delegate run: {run}, account: {acct}) {cmd}");
            }
            (Some(run), None) => println!("  pid={pid} (delegate run: {run}) {cmd}"),
            (None, Some(acct)) => println!("  pid={pid} (account: {acct}) {cmd}"),
            (None, None) => println!("  pid={pid} {cmd}"),
        }
    }
    println!("store: {}", s(&snap, "root"));
    println!("Next: delegate agy account list --quota | delegate agy account use <id>");
    Ok(0)
}

fn temp_home_has_core_token(temp_home: &Path) -> bool {
    CRED_FILES.iter().any(|cf| {
        cf.is_core_token
            && fs::metadata(temp_home.join(cf.home_rel))
                .is_ok_and(|m| m.is_file() && m.len() > 0)
    })
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
        if id.eq_ignore_ascii_case(AUTO_ACCOUNT_ID) {
            return Err("account slot id \"auto\" is reserved for automatic selection mode".into());
        }
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
    crate::common::strip_antigravity_env(&mut cmd);
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
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run {}: {e}", agy_bin.display()))?;
    let mut prompted_logged_in = false;
    let status = loop {
        if let Some(st) = child
            .try_wait()
            .map_err(|e| format!("failed while waiting for {}: {e}", agy_bin.display()))?
        {
            break st;
        }
        if !json_out && !prompted_logged_in && temp_home_has_core_token(&temp_home.path) {
            prompted_logged_in = true;
            let mut err = std::io::stderr().lock();
            if err.is_terminal() {
                let _ = write!(
                    err,
                    "\r\n\x1b[1;32m>>> 已登录，按两次 Ctrl+D 退出 agy 即保存 <<<\x1b[0m\r\n"
                );
            } else {
                let _ = writeln!(err, ">>> 已登录，按两次 Ctrl+D 退出 agy 即保存 <<<");
            }
            let _ = err.flush();
        }
        thread::sleep(Duration::from_millis(200));
    };

    let has_core = temp_home_has_core_token(&temp_home.path);
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
        println!("其他机器跑 lan-agent-sync 同步新槽位。");
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
        return Err("usage: delegate agy account use <id|auto> [--force] [--json]".into());
    }
    let target_id = &pos[0];
    let json_out = has(&flags, "--json");

    if target_id == AUTO_ACCOUNT_ID {
        let root = accounts_root();
        let home_dir = home();
        let _lock = acquire_accounts_lock(&root)?;
        let mut state = load_state(&root);
        let acct_count = state["accounts"].as_object().map_or(0, Map::len);
        if acct_count == 0 {
            return Err(
                "no agy account slots configured; run `delegate agy account add` or `import-agy-swap`"
                    .into(),
            );
        }
        let prev_active = state["active"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let prev_mode = if is_auto_mode_state(&state) { "auto" } else { "fixed" };
        let running = detect_running_agy_for_home(&home_dir);

        // Keep live ~/.gemini untouched on the prior active slot; only switch mode to `auto`.
        state["schemaVersion"] = json!(SCHEMA_VERSION);
        state["mode"] = json!(AUTO_ACCOUNT_ID);
        save_state(&root, &state)?;

        let state_root = state_dir();
        let pool_cur = load_pool_current_in(&state_root);
        let mut pc_json = Map::new();
        for pool in QuotaPool::ALL {
            let cur = pool_cur
                .get(&pool)
                .filter(|id| state["accounts"].get(id.as_str()).is_some())
                .cloned()
                .or_else(|| prev_active.clone());
            if let Some(id) = cur {
                pc_json.insert(pool.key().to_string(), json!(id));
            }
        }
        let running_json: Vec<Value> = running.iter().map(running_agy_to_json).collect();
        if json_out {
            println!(
                "{}",
                json!({
                    "ok": true,
                    "mode": "auto",
                    "selected": "auto",
                    "previousMode": prev_mode,
                    "active": prev_active,
                    "liveSlot": prev_active,
                    "poolCurrent": Value::Object(pc_json),
                    "runningAgyCount": running.len(),
                    "runningAgy": running_json,
                })
            );
        } else {
            let live_lbl = prev_active.as_deref().unwrap_or("none");
            println!(
                "switched agy account selection to auto (live ~/.gemini stays on {live_lbl}; new runs will auto-select per quota pool)."
            );
            if !running.is_empty() {
                println!(
                    "note: {} running agy process(es) and existing sessions will continue on their original account.",
                    running.len()
                );
                for p in &running {
                    println!("{}", format_running_proc_line(p));
                }
            } else {
                println!(
                    "note: existing sessions remain pinned; new runs will auto-select per pool. Use `delegate agy account use <id>` to pin a fixed account."
                );
            }
        }
        return Ok(0);
    }

    validate_id(target_id)?;
    let force = has(&flags, "--force");

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
    state["mode"] = json!("fixed");
    if let Some(obj) = state["accounts"][target_id].as_object_mut() {
        obj.insert("lastUsedAt".into(), json!(now));
        obj.insert("lastSyncedAt".into(), json!(now));
    }
    save_state(&root, &state)?;
    // Sync all per-pool pointers to `target_id` so re-entering `use auto` starts from here.
    let _ = set_all_pool_currents_in(&state_dir(), target_id);

    let target_email = s(&state["accounts"][target_id], "email").to_string();
    let running_json: Vec<Value> = running.iter().map(running_agy_to_json).collect();
    if json_out {
        println!(
            "{}",
            json!({
                "ok": true,
                "mode": "fixed",
                "selected": target_id,
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
    if state["accounts"].as_object().is_none_or(Map::is_empty) {
        state["mode"] = json!("fixed");
    }
    save_state(&root, &state)?;
    let _ = clear_pool_current_slot_in(&state_dir(), id);

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
        if let Some(line) = format_cli_quota_estimate_line(label, &acct_view["quotaEstimate"][key]) {
            println!("{line}");
        }
    }
    println!("Next: delegate agy account status | delegate usage --account {id}");
    Ok(0)
}

fn resolve_pick_model_alias(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // Try repo `.delegate.json` first, then user config `models` alias map (strictly read-only).
    if let Ok(cwd) = env::current_dir() {
        if let Some(top) = git_top(&cwd) {
            if let Ok(Some(repo_doc)) = crate::config::read_file(&top.join(".delegate.json")) {
                if let Some(entry) = repo_doc.get("models").and_then(|m| m.get(trimmed)) {
                    if let Some(mapped) = crate::models::alias_model(entry) {
                        return mapped.to_string();
                    }
                }
            }
        }
    }
    if let Ok(Some(user_doc)) = crate::config::read_file(&crate::config::user_path()) {
        if let Some(entry) = user_doc.get("models").and_then(|m| m.get(trimmed)) {
            if let Some(mapped) = crate::models::alias_model(entry) {
                return mapped.to_string();
            }
        }
    }
    trimmed.to_string()
}

/// Strictly read-only dry-run report for `delegate agy account pick [--model <alias>] [--json]`.
/// Never creates a run, never queries `agy -p /usage`, never mutates state files.
#[allow(clippy::too_many_arguments)]
pub fn build_pick_report_in(
    root: &Path,
    state: &Value,
    quota_cache: &Value,
    pool_current: &BTreeMap<QuotaPool, String>,
    running_writers: &BTreeMap<(String, QuotaPool), usize>,
    model_input: Option<&str>,
    resolved_model: Option<&str>,
    cfg: &AgyAutoSwitchConfig,
    now_ep: i64,
) -> Value {
    let active_id = state["active"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut all_ids: Vec<String> = state["accounts"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    all_ids.sort();

    let mut pool_cur_json = Map::new();
    for pool in QuotaPool::ALL {
        let cur = pool_current
            .get(&pool)
            .filter(|id| state["accounts"].get(id.as_str()).is_some())
            .cloned()
            .or_else(|| active_id.clone());
        if let Some(id) = cur {
            pool_cur_json.insert(pool.key().to_string(), json!(id));
        }
    }

    let mut accounts_json: Vec<Value> = Vec::new();
    for id in &all_ids {
        let health = check_slot_health(root, state, quota_cache, id);
        let acct_quota = &quota_cache["accounts"][id];
        let fetched_at = s(acct_quota, "fetchedAt").to_string();
        let fetched_ep = acct_quota["fetchedEpoch"].as_i64().unwrap_or(0);
        let age_secs = (fetched_ep > 0).then(|| (now_ep - fetched_ep).max(0));

        let mut pools_map = Map::new();
        for pool in QuotaPool::ALL {
            let k = running_writers.get(&(id.clone(), pool)).copied().unwrap_or(0);
            let ev = evaluate_slot_pool(acct_quota, pool, cfg, k, now_ep);
            pools_map.insert(
                pool.key().to_string(),
                json!({
                    "tier": ev.tier.as_str(),
                    "tierLabel": ev.tier.label_zh(),
                    "rawFiveHourPct": ev.raw_five_hour_pct,
                    "effectiveFiveHourPct": ev.eff_five_hour_pct,
                    "weeklyPct": ev.weekly_pct,
                    "runningWriters": ev.running_writers,
                    "passiveExhausted": ev.passive_exhausted,
                    "unknown": ev.unknown,
                }),
            );
        }
        accounts_json.push(json!({
            "id": id,
            "active": active_id.as_deref() == Some(id.as_str()),
            "healthy": health.is_ok(),
            "unhealthyReason": health.err(),
            "fetchedAt": fetched_at,
            "fetchedEpoch": fetched_ep,
            "ageSeconds": age_secs,
            "pools": Value::Object(pools_map),
        }));
    }

    let mut picks_map = Map::new();
    for pool in QuotaPool::ALL {
        let probe_model = match pool {
            QuotaPool::Gemini => "agy/gemini",
            QuotaPool::ClaudeGpt => "agy/claude",
        };
        let dec = select_account_for_new_session(
            root,
            state,
            quota_cache,
            pool_current,
            running_writers,
            probe_model,
            cfg,
            now_ep,
        );
        picks_map.insert(
            pool.key().to_string(),
            json!({
                "pool": pool.key(),
                "poolLabel": pool.short_label(),
                "current": dec.current_before,
                "selected": dec.selected_id,
                "wouldSwitch": dec.switched.is_some(),
                "pointerChanged": dec.pointer_changed,
                "reason": dec.reason,
            }),
        );
    }

    let quota_updated_at = s(quota_cache, "updatedAt").to_string();
    let quota_updated_ep = quota_cache["updatedEpoch"].as_i64().unwrap_or(0);
    let quota_age_secs = (quota_updated_ep > 0).then(|| (now_ep - quota_updated_ep).max(0));
    let auto_mode = is_auto_mode_state(state);
    let mut auto_cfg_json = cfg.to_json();
    auto_cfg_json["enabled"] = json!(auto_mode);
    auto_cfg_json["mode"] = json!(if auto_mode { "auto" } else { "fixed" });

    let mut out = json!({
        "ok": true,
        "mode": if auto_mode { "auto" } else { "fixed" },
        "autoSwitch": auto_cfg_json,
        "active": active_id,
        "poolCurrent": Value::Object(pool_cur_json),
        "quotaUpdatedAt": quota_updated_at,
        "quotaAgeSeconds": quota_age_secs,
        "picks": Value::Object(picks_map),
        "accounts": accounts_json,
    });

    if let Some(raw_m) = model_input.filter(|m| !m.trim().is_empty()) {
        let res_m = resolved_model.unwrap_or(raw_m);
        let dec = select_account_for_new_session(
            root,
            state,
            quota_cache,
            pool_current,
            running_writers,
            res_m,
            cfg,
            now_ep,
        );
        out["model"] = json!(raw_m);
        out["resolvedModel"] = json!(res_m);
        out["pool"] = json!(dec.pool.map(QuotaPool::key));
        out["current"] = json!(dec.current_before);
        out["selected"] = json!(dec.selected_id);
        out["wouldSwitch"] = json!(dec.switched.is_some());
        out["pointerChanged"] = json!(dec.pointer_changed);
        out["reason"] = json!(dec.reason);
    }
    out
}

fn format_pick_pool_eval_cell(pool_obj: &Value) -> String {
    let tier = s(pool_obj, "tierLabel");
    if b(pool_obj, "passiveExhausted") {
        return format!("{tier}(被动0%)");
    }
    if b(pool_obj, "unknown") {
        return format!("{tier}(未知)");
    }
    let eff5 = pool_obj["effectiveFiveHourPct"]
        .as_f64()
        .map(|v| format!("{:.0}%", v))
        .unwrap_or_else(|| "-".to_string());
    let wk = pool_obj["weeklyPct"]
        .as_f64()
        .map(|v| format!("{:.0}%", v))
        .unwrap_or_else(|| "-".to_string());
    let rw = pool_obj["runningWriters"].as_u64().unwrap_or(0);
    if rw > 0 {
        let raw5 = pool_obj["rawFiveHourPct"]
            .as_f64()
            .map(|v| format!("{:.0}%", v))
            .unwrap_or_else(|| "-".to_string());
        format!("{tier}(5h:{eff5}[原{raw5},在跑{rw}] 7d:{wk})")
    } else {
        format!("{tier}(5h:{eff5} 7d:{wk})")
    }
}

fn cmd_pick(args: &[String]) -> Res<i32> {
    let usage = "delegate agy account pick [--model <alias>] [--json]";
    let (pos, flags, kv) = parse_simple(usage, args, &["--json"], &["--model"])?;
    if !pos.is_empty() {
        return Err(unrecognized_arg(&pos.join(" "), &["--json", "--model"], usage));
    }
    let json_out = has(&flags, "--json");
    let model_input = value(&kv, "--model");
    let resolved = model_input.map(resolve_pick_model_alias);

    let root = accounts_root();
    let state_root = state_dir();
    let state = load_state(&root);
    let quota_cache = load_quota_cache_in(&state_root);
    let pool_cur = load_pool_current_in(&state_root);
    let running = count_running_writers(&state_root, None);
    let cfg = load_agy_auto_switch_config();
    let now_ep = epoch() as i64;

    let report = build_pick_report_in(
        &root,
        &state,
        &quota_cache,
        &pool_cur,
        &running,
        model_input,
        resolved.as_deref(),
        &cfg,
        now_ep,
    );

    if json_out {
        println!("{report}");
        return Ok(0);
    }

    let is_auto = s(&report, "mode") == AUTO_ACCOUNT_ID;
    let active = report["active"].as_str().unwrap_or("none");
    let status_lbl = if is_auto {
        format!(
            "auto（5h≥{:.0}% 7d≥{:.0}% · {}m · 预扣{:.0}%/写手）",
            cfg.five_hour_min_pct,
            cfg.weekly_min_pct,
            cfg.refresh_minutes,
            cfg.per_run_reserve_pct,
        )
    } else {
        format!("固定号（{active}；执行 `delegate agy account use auto` 开启自动选号）")
    };
    let cache_at = s(&report, "quotaUpdatedAt");
    let cache_age = report["quotaAgeSeconds"]
        .as_i64()
        .map(format_age_compact)
        .unwrap_or_else(|| "无缓存".to_string());
    let cache_info = if cache_at.is_empty() {
        "无缓存".to_string()
    } else {
        format!("{cache_at}（{cache_age}）")
    };
    println!("模式：{status_lbl} · 实盘号：{active} · 缓存：{cache_info}");

    if let Some(raw_m) = model_input {
        let sel = report["selected"].as_str().unwrap_or("none");
        let pool_str = report["pool"].as_str().unwrap_or("unknown");
        let reason = s(&report, "reason");
        let res_m = s(&report, "resolvedModel");
        let model_lbl = if res_m != raw_m && !res_m.is_empty() {
            format!("{raw_m} -> {res_m}")
        } else {
            raw_m.to_string()
        };
        println!("新会话选号 [{model_lbl} · 池:{pool_str}]：{sel}（{reason}）");
    } else {
        for pool in QuotaPool::ALL {
            let p = &report["picks"][pool.key()];
            let sel = p["selected"].as_str().unwrap_or("none");
            let reason = s(p, "reason");
            println!("新会话选号 [{}]：{sel}（{reason}）", pool.short_label());
        }
    }

    if let Some(accounts) = report["accounts"].as_array() {
        if accounts.is_empty() {
            println!("  （未配置 agy 账号槽位）");
        } else {
            for acct in accounts {
                let id = s(acct, "id");
                let mark = if b(acct, "active") { "*" } else { " " };
                let health_tag = if b(acct, "healthy") {
                    String::new()
                } else {
                    format!(" [不可用:{}]", s(acct, "unhealthyReason"))
                };
                let fetched = s(acct, "fetchedAt");
                let age = acct["ageSeconds"]
                    .as_i64()
                    .map(format_age_compact)
                    .unwrap_or_else(|| "无缓存".to_string());
                let g_cell = format_pick_pool_eval_cell(&acct["pools"]["gemini"]);
                let c_cell = format_pick_pool_eval_cell(&acct["pools"]["claude_gpt"]);
                let ts_part = if fetched.is_empty() {
                    "缓存:无".to_string()
                } else {
                    format!("缓存:{fetched}({age})")
                };
                println!(
                    "  {mark} {id:<14} Gemini={g_cell}  Claude/GPT={c_cell}  {ts_part}{health_tag}"
                );
            }
        }
    }
    Ok(0)
}

fn print_account_help() {
    println!(
        "Usage: delegate agy account <subcommand> [options]\n\
         \x20      delegate agy login [<id>] [--label TEXT] [--overwrite] [--agy-bin PATH] [-- <agy-args>...]\n\n\
         Manage local Antigravity CLI (agy) accounts (~/.config/delegate/agy-accounts/).\n\
         Real quota uses `agy -p /usage </dev/null` only (no internal HTTP APIs).\n\n\
         Subcommands:\n\
         \x20 list [--quota|--quota-if-stale] [--agy-bin PATH] [--json]\n\
         \x20                                      List saved account slots, active marker, 5h/7d usage & quota\n\
         \x20 status [--quota|--quota-if-stale] [--agy-bin PATH] [--json]\n\
         \x20                                      Show active account, live email, quota, usage, running agy\n\
         \x20 pick [--model ALIAS] [--json]        Read-only dry-run: show which account a new session would pick from cache\n\
         \x20 add [<id>] [--label TEXT] [--overwrite] [--agy-bin PATH] [-- <agy-args>...]\n\
         \x20                                      Run interactive agy login in a temp HOME and save slot (alias: `delegate agy login`)\n\
         \x20 use <id|auto> [--force] [--json]     Switch to a fixed slot (updates ~/.gemini) or `auto` (auto-select per pool without touching ~/.gemini)\n\
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
    if first == "login" {
        let sub_args = &args[1..];
        if sub_args
            .first()
            .is_some_and(|a| matches!(a.as_str(), "-h" | "--help" | "help"))
        {
            print_account_help();
            return Ok(0);
        }
        return cmd_add(sub_args);
    }
    if !matches!(first, "account" | "accounts") {
        return Err(format!(
            "unknown agy subcommand {first:?} (expected: account, login)"
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
        "pick" => cmd_pick(sub_args),
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
            r#"{"active":"orbit@example.test","old":[]}"#,
        )
        .unwrap();
        // Before cli token exists, falls back to google_accounts.json.
        assert_eq!(live_email(h).as_deref(), Some("orbit@example.test"));

        // When a long-lived agy process overwrites antigravity-cli/antigravity-oauth-token
        // with another account without touching google_accounts.json, live_email must report it.
        fs::write(
            h.join(".gemini/antigravity-cli/antigravity-oauth-token"),
            make_test_token_json("nova@example.test", "AT-G", "2026-11-25T19:50:07Z"),
        )
        .unwrap();
        assert_eq!(live_email(h).as_deref(), Some("nova@example.test"));
        let all = collect_home_cred_emails(h);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].1, "nova@example.test");
        assert_eq!(all[1].1, "orbit@example.test");
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
            json!({"id":"nova","quota":{"ok":true,"entries":same_entries}}),
            json!({"id":"orbit","quota":{"ok":true,"entries":same_entries}}),
        ];
        assert_eq!(
            find_duplicate_quota_slot_groups(&dup_accounts),
            vec![vec!["nova".to_string(), "orbit".to_string()]]
        );

        // Same percentages but different resetAt must NOT trigger duplicate warning.
        let distinct_accounts = vec![
            json!({"id":"nova","quota":{"ok":true,"entries":same_entries}}),
            json!({"id":"orbit","quota":{"ok":true,"entries":diff_reset}}),
        ];
        assert!(find_duplicate_quota_slot_groups(&distinct_accounts).is_empty());

        // Empty entries must NOT trigger duplicate warning.
        let empty_accounts = vec![
            json!({"id":"nova","quota":{"ok":false,"entries":[]}}),
            json!({"id":"orbit","quota":{"ok":false,"entries":[]}}),
        ];
        assert!(find_duplicate_quota_slot_groups(&empty_accounts).is_empty());
    }

    #[test]
    fn detect_quota_depletion_warnings_flags_only_when_earlier_than_reset() {
        let snap = json!({
            "mode": "fixed",
            "active": "hot-acct",
            "accounts": [
                {
                    "id": "standby-hot",
                    "active": false,
                    "quota": {
                        "ok": true,
                        "entries": [
                            {"pool": "Gemini Models", "window": "weekly", "remainingPct": 25.0, "resetInSeconds": 432000}
                        ]
                    },
                    "quotaEstimate": {
                        "pools": [{
                            "pool": "Gemini Models",
                            "tier": "yellow",
                            "weekly": {
                                "pool": "Gemini Models",
                                "status": "depletes",
                                "alert": true,
                                "fastMinutes": 300,
                                "midMinutes": 360,
                                "slowMinutes": 420,
                                "runningCount": 1,
                                "basis": "24h"
                            }
                        }]
                    }
                },
                {
                    "id": "hot-acct",
                    "active": true,
                    "quota": {
                        "ok": true,
                        "entries": [
                            {"pool": "Gemini Models", "window": "weekly", "remainingPct": 35.0, "resetInSeconds": 432000}
                        ]
                    },
                    "quotaEstimate": {
                        "pools": [{
                            "pool": "Gemini Models",
                            "tier": "yellow",
                            "weekly": {
                                "pool": "Gemini Models",
                                "status": "depletes",
                                "alert": true,
                                "fastMinutes": 600,
                                "midMinutes": 660,
                                "slowMinutes": 720,
                                "runningCount": 1,
                                "basis": "24h"
                            }
                        }]
                    }
                }
            ]
        });
        // Fixed mode alerts ONLY on active `hot-acct`, never on inactive `standby-hot`.
        let warns = detect_quota_depletion_warnings(&snap);
        assert_eq!(warns.len(), 1);
        assert_eq!(warns[0].account_id, "hot-acct");
        assert!(warns[0].active);
        assert_eq!(warns[0].pool, "Gemini");
        let line = format_status_quota_warning_line(&warns).expect("warning line");
        assert!(line.contains("⚠ 额度预警"), "{line}");
        assert!(line.contains("hot-acct"), "{line}");
        assert!(line.contains("11 小时"), "{line}");
        assert!(!line.contains("不自动换号"), "{line}");

        // Auto mode with a Green backup slot suppresses warning.
        let auto_green_backup = json!({
            "mode": "auto",
            "autoSwitch": {
                "enabled": true,
                "poolSummary": { "gemini": { "selected": "slot-a" } }
            },
            "accounts": [
                {
                    "id": "slot-a",
                    "active": true,
                    "quota": {
                        "ok": true,
                        "entries": [
                            {"pool": "Gemini Models", "window": "fiveHour", "remainingPct": 25.0, "resetInSeconds": 10800}
                        ]
                    },
                    "quotaEstimate": {
                        "pools": [{
                            "pool": "Gemini Models",
                            "tier": "yellow",
                            "fiveHour": {
                                "pool": "Gemini Models",
                                "status": "depletes",
                                "alert": true,
                                "fastMinutes": 35,
                                "midMinutes": 45,
                                "slowMinutes": 60,
                                "runningCount": 1,
                                "basis": "30m"
                            }
                        }]
                    }
                },
                {
                    "id": "slot-b",
                    "active": false,
                    "quota": {
                        "ok": true,
                        "entries": [
                            {"pool": "Gemini Models", "window": "fiveHour", "remainingPct": 95.0, "resetInSeconds": 14400}
                        ]
                    },
                    "quotaEstimate": {
                        "pools": [{
                            "pool": "Gemini Models",
                            "tier": "green",
                            "fiveHour": { "pool": "Gemini Models", "status": "idle" }
                        }]
                    }
                }
            ]
        });
        assert!(
            detect_quota_depletion_warnings(&auto_green_backup).is_empty(),
            "auto mode with green backup must suppress depletion alert"
        );

        // Auto mode when ALL slots are non-Green and combined capacity won't reach earliest reset -> warns.
        let mut auto_all_low = auto_green_backup.clone();
        auto_all_low["accounts"][1]["quota"]["entries"][0]["remainingPct"] = json!(5.0);
        auto_all_low["accounts"][1]["quotaEstimate"]["pools"][0]["tier"] = json!("red");
        let auto_warns = detect_quota_depletion_warnings(&auto_all_low);
        assert_eq!(auto_warns.len(), 1);
        assert!(auto_warns[0].auto_all_low);
        let auto_line = format_status_quota_warning_line(&auto_warns).expect("auto warning line");
        assert!(auto_line.contains("auto 模式 Gemini 池"), "{auto_line}");
        assert!(auto_line.contains("slot-a 5h 25%"), "{auto_line}");
        assert!(auto_line.contains("slot-b 5h 5%"), "{auto_line}");

        // Backup with passive_error synthetic 15m reset must NOT suppress auto all-low warning.
        let mut auto_passive_backup = auto_all_low.clone();
        auto_passive_backup["accounts"][1]["quota"]["entries"][0]["remainingPct"] = json!(0.0);
        auto_passive_backup["accounts"][1]["quota"]["entries"][0]["resetInSeconds"] = json!(900);
        auto_passive_backup["accounts"][1]["quota"]["entries"][0]["source"] = json!("passive_error");
        let passive_warns = detect_quota_depletion_warnings(&auto_passive_backup);
        assert_eq!(
            passive_warns.len(),
            1,
            "passive_error synthetic 15m reset on backup must not suppress auto all-low alert"
        );

        // Backup whose weekly is 0% (Red) must NOT contribute its 5h % to combined capacity
        // nor suppress auto 5h warning even if its 5h is high (60%) or resets soon.
        let mut auto_weekly_blocked = auto_all_low.clone();
        auto_weekly_blocked["accounts"][1]["quota"]["entries"] = json!([
            {"pool": "Gemini Models", "window": "fiveHour", "remainingPct": 60.0, "resetInSeconds": 1200},
            {"pool": "Gemini Models", "window": "weekly", "remainingPct": 0.0, "resetInSeconds": 200000}
        ]);
        let wb_warns = detect_quota_depletion_warnings(&auto_weekly_blocked);
        assert_eq!(
            wb_warns.len(),
            1,
            "backup with 0% weekly must not suppress auto 5h alert even if 5h is 60% and resets soon"
        );
        assert_eq!(
            wb_warns[0].remaining_minutes, 45,
            "weekly-blocked backup's 5h % must not inflate combined_pct"
        );

        // Tiny reset window (< 5m) must never round ETA above resetAt.
        assert_eq!(format_eta_zh_phrase(2, 2, Some(2), 240), "约 3 分钟后用完");
        assert_eq!(format_eta_zh_phrase(2, 2, Some(3), 240), "约 3 分钟后到重置前之间用完");
        assert_eq!(format_eta_zh_phrase(1, 1, None, 90), "即将用完");
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

        // 3. Cached quota 0% on Gemini pool MUST stay strictly sticky on pinned alpha
        //    (never silently switch accounts while resuming an existing conversation).
        let exhausted_quota = json!({
            "accounts": {
                "alpha": {
                    "ok": true,
                    "fetchedEpoch": now_ep,
                    "entries": [
                        {"pool": "Gemini Models", "window": "fiveHour", "remainingPct": 0.0, "resetAt": "2026-11-26T18:00:00Z"},
                        {"pool": "Claude and GPT models", "window": "fiveHour", "remainingPct": 85.0, "resetAt": "2026-11-26T18:00:00Z"}
                    ]
                }
            }
        });
        let keep_sticky_on_zero = AgyCredOverlay::prepare_for_session_in(
            &root,
            &real_home,
            &real_cfg,
            Some("alpha"),
            "agy/gemini-4-neon",
            &exhausted_quota,
            now_ep,
        )
        .unwrap();
        assert_eq!(keep_sticky_on_zero.account_id.as_deref(), Some("alpha"));
        assert!(keep_sticky_on_zero.switched.is_none());

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

    fn setup_two_test_slots() -> (TempHome, PathBuf, PathBuf, PathBuf, Value) {
        let base = TempHome::create().unwrap();
        let real_home = base.path.join("home");
        let real_cfg = real_home.join(".config");
        let root = real_cfg.join("delegate/agy-accounts");
        fs::create_dir_all(real_home.join(".gemini/antigravity-cli")).unwrap();
        for (id, email, tok) in [
            ("acct-a", "a@example.test", "TOK-A"),
            ("acct-b", "b@example.test", "TOK-B"),
        ] {
            let sdir = slot_dir(&root, id);
            ensure_dir_mode(&sdir, 0o700).unwrap();
            let bytes = make_test_token_json(email, tok, "2026-11-27T20:00:00Z");
            write_secret_0600(&sdir.join("antigravity-cli-oauth-token"), &bytes, true).unwrap();
        }
        install_slot_to_home(&slot_dir(&root, "acct-a"), &real_home).unwrap();
        let state = json!({
            "schemaVersion": SCHEMA_VERSION,
            "active": "acct-a",
            "mode": "auto",
            "accounts": {
                "acct-a": {"id": "acct-a", "email": "a@example.test"},
                "acct-b": {"id": "acct-b", "email": "b@example.test"}
            }
        });
        save_state(&root, &state).unwrap();
        (base, root, real_home, real_cfg, state)
    }

    fn mk_pool_quota(
        now_ep: i64,
        a_gem: (f64, f64),
        b_gem: (f64, f64),
        reset_at: &str,
    ) -> Value {
        json!({
            "schemaVersion": 1,
            "updatedEpoch": now_ep,
            "updatedAt": format_epoch(now_ep, false, "%Y-%m-%dT%H:%M:%SZ"),
            "accounts": {
                "acct-a": {
                    "ok": true,
                    "fetchedEpoch": now_ep,
                    "fetchedAt": format_epoch(now_ep, false, "%Y-%m-%dT%H:%M:%SZ"),
                    "entries": [
                        {"pool": "Gemini Models", "window": "fiveHour", "remainingPct": a_gem.0, "resetAt": reset_at},
                        {"pool": "Gemini Models", "window": "weekly", "remainingPct": a_gem.1, "resetAt": "2026-12-04T00:00:00Z"}
                    ]
                },
                "acct-b": {
                    "ok": true,
                    "fetchedEpoch": now_ep,
                    "fetchedAt": format_epoch(now_ep, false, "%Y-%m-%dT%H:%M:%SZ"),
                    "entries": [
                        {"pool": "Gemini Models", "window": "fiveHour", "remainingPct": b_gem.0, "resetAt": reset_at},
                        {"pool": "Gemini Models", "window": "weekly", "remainingPct": b_gem.1, "resetAt": "2026-12-04T00:00:00Z"}
                    ]
                }
            }
        })
    }

    fn enabled_cfg() -> AgyAutoSwitchConfig {
        AgyAutoSwitchConfig::default()
    }

    #[test]
    fn carbon_counterexample_double_low_no_ping_pong() {
        let (_base, root, _, _, state) = setup_two_test_slots();
        let now_ep = iso_epoch("2026-11-27T10:00:00Z").unwrap();
        let cfg = enabled_cfg();
        let mut pc = BTreeMap::new();
        pc.insert(QuotaPool::Gemini, "acct-a".to_string());
        let rw = BTreeMap::new();

        // Weekend double-low: both Yellow (5h=80%, 7d=25% vs 26%) -> stays on current acct-a.
        let q1 = mk_pool_quota(now_ep, (80.0, 25.0), (80.0, 26.0), "2026-11-27T15:00:00Z");
        let d1 = select_account_for_new_session(&root, &state, &q1, &pc, &rw, "agy/gemini-4-neon", &cfg, now_ep);
        assert_eq!(d1.selected_id.as_deref(), Some("acct-a"));
        assert!(!d1.pointer_changed);
        assert!(d1.switched.is_none());

        // 15m later: acct-a drops 2% to 23% (still Yellow) -> still stays on acct-a, no ping-pong.
        let q2 = mk_pool_quota(now_ep + 900, (78.0, 23.0), (80.0, 26.0), "2026-11-27T15:00:00Z");
        let d2 = select_account_for_new_session(&root, &state, &q2, &pc, &rw, "agy/gemini-4-neon", &cfg, now_ep + 900);
        assert_eq!(d2.selected_id.as_deref(), Some("acct-a"));
        assert!(!d2.pointer_changed);
    }

    #[test]
    fn carbon_counterexample_never_switches_to_zero_pct_dead_slot_and_double_red_rule() {
        let (_base, root, _, _, state) = setup_two_test_slots();
        let now_ep = iso_epoch("2026-11-27T10:00:00Z").unwrap();
        let cfg = enabled_cfg();
        let mut pc = BTreeMap::new();
        pc.insert(QuotaPool::Gemini, "acct-a".to_string());
        let rw = BTreeMap::new();

        // A(5h=25%, 7d=60%) is Yellow; B(5h=0%, 7d=75%) is Red -> MUST stay on A, never switch to 0% B!
        let q = mk_pool_quota(now_ep, (25.0, 60.0), (0.0, 75.0), "2026-11-27T15:00:00Z");
        let d = select_account_for_new_session(&root, &state, &q, &pc, &rw, "agy/gemini-4-neon", &cfg, now_ep);
        assert_eq!(d.selected_id.as_deref(), Some("acct-a"));
        assert!(!d.pointer_changed);

        // Double-Red: A(5h=0%, 7d=80%) vs B(5h=3%, 7d=10%) -> B has 5h > 0, so switches to B.
        let q_red = mk_pool_quota(now_ep, (0.0, 80.0), (3.0, 10.0), "2026-11-27T15:00:00Z");
        let d_red = select_account_for_new_session(&root, &state, &q_red, &pc, &rw, "agy/gemini-4-neon", &cfg, now_ep);
        assert_eq!(d_red.selected_id.as_deref(), Some("acct-b"));
        assert!(d_red.pointer_changed);
        assert_eq!(d_red.switched.as_ref().unwrap().kind, "auto");

        // Double-Red both 5h=0%: A(0%, 50%) vs B(0%, 80%) -> both (false, 0) -> stays on current A.
        let q_both0 = mk_pool_quota(now_ep, (0.0, 50.0), (0.0, 80.0), "2026-11-27T15:00:00Z");
        let d_both0 = select_account_for_new_session(&root, &state, &q_both0, &pc, &rw, "agy/gemini-4-neon", &cfg, now_ep);
        assert_eq!(d_both0.selected_id.as_deref(), Some("acct-a"));
        assert!(!d_both0.pointer_changed);
    }

    #[test]
    fn carbon_counterexample_7d_5h_mismatch_stays_on_current() {
        let (_base, root, _, _, state) = setup_two_test_slots();
        let now_ep = iso_epoch("2026-11-27T10:00:00Z").unwrap();
        let cfg = enabled_cfg();
        let mut pc = BTreeMap::new();
        pc.insert(QuotaPool::Gemini, "acct-a".to_string());
        let rw = BTreeMap::new();

        // A(5h=90%, 7d=29%) Yellow vs B(5h=12%, 7d=31%) Yellow -> stays on A!
        let q = mk_pool_quota(now_ep, (90.0, 29.0), (12.0, 31.0), "2026-11-27T15:00:00Z");
        let d = select_account_for_new_session(&root, &state, &q, &pc, &rw, "agy/gemini-4-neon", &cfg, now_ep);
        assert_eq!(d.selected_id.as_deref(), Some("acct-a"));
        assert!(!d.pointer_changed);
    }

    #[test]
    fn carbon_counterexample_manual_use_syncs_all_pool_pointers() {
        let (base, root, _, _, state) = setup_two_test_slots();
        let state_root = base.path.join("state");
        fs::create_dir_all(&state_root).unwrap();
        let now_ep = iso_epoch("2026-11-27T10:00:00Z").unwrap();
        let cfg = enabled_cfg();

        // Initially Gemini pointer is acct-a, Claude pointer is acct-a.
        set_all_pool_currents_in(&state_root, "acct-a").unwrap();
        assert_eq!(load_pool_current_in(&state_root).get(&QuotaPool::Gemini).map(String::as_str), Some("acct-a"));
        assert_eq!(load_pool_current_in(&state_root).get(&QuotaPool::ClaudeGpt).map(String::as_str), Some("acct-a"));

        // Manual `use acct-b` syncs BOTH pools to acct-b.
        set_all_pool_currents_in(&state_root, "acct-b").unwrap();
        let pc = load_pool_current_in(&state_root);
        assert_eq!(pc.get(&QuotaPool::Gemini).map(String::as_str), Some("acct-b"));
        assert_eq!(pc.get(&QuotaPool::ClaudeGpt).map(String::as_str), Some("acct-b"));

        // Both Green -> next new session stays on acct-b (respecting manual `use acct-b`).
        let q = mk_pool_quota(now_ep, (80.0, 80.0), (80.0, 80.0), "2026-11-27T15:00:00Z");
        let d = select_account_for_new_session(&root, &state, &q, &pc, &BTreeMap::new(), "agy/gemini-4-neon", &cfg, now_ep);
        assert_eq!(d.selected_id.as_deref(), Some("acct-b"));
        assert!(!d.pointer_changed);
    }

    #[test]
    fn carbon_counterexample_concurrent_blind_window_virtual_reserve() {
        let (_base, root, _, _, state) = setup_two_test_slots();
        let now_ep = iso_epoch("2026-11-27T10:00:00Z").unwrap();
        let cfg = enabled_cfg(); // perRunReservePct = 5.0
        let mut pc = BTreeMap::new();
        pc.insert(QuotaPool::Gemini, "acct-a".to_string());
        let q = mk_pool_quota(now_ep, (34.0, 80.0), (85.0, 80.0), "2026-11-27T15:00:00Z");

        // k = 0 running writers -> eff_5h = 34 >= 30 (Green) -> stays on acct-a.
        let d0 = select_account_for_new_session(&root, &state, &q, &pc, &BTreeMap::new(), "agy/gemini-4-neon", &cfg, now_ep);
        assert_eq!(d0.selected_id.as_deref(), Some("acct-a"));

        // k = 2 running Claude writers on acct-a -> does NOT deduct from Gemini pool!
        let mut rw_other_pool = BTreeMap::new();
        rw_other_pool.insert(("acct-a".to_string(), QuotaPool::ClaudeGpt), 2);
        let d_other = select_account_for_new_session(&root, &state, &q, &pc, &rw_other_pool, "agy/gemini-4-neon", &cfg, now_ep);
        assert_eq!(d_other.selected_id.as_deref(), Some("acct-a"));

        // k = 2 running Gemini writers on acct-a -> eff_5h = 34 - 10 = 24 < 30 (Yellow) < B(Green) -> switches to acct-b!
        let mut rw = BTreeMap::new();
        rw.insert(("acct-a".to_string(), QuotaPool::Gemini), 2);
        let d2 = select_account_for_new_session(&root, &state, &q, &pc, &rw, "agy/gemini-4-neon", &cfg, now_ep);
        assert_eq!(d2.selected_id.as_deref(), Some("acct-b"));
        assert!(d2.pointer_changed);
        assert_eq!(d2.switched.as_ref().unwrap().kind, "auto");
    }

    #[test]
    fn carbon_counterexample_reset_at_60s_buffer_and_post_reset_unknown() {
        let (_base, root, _, _, state) = setup_two_test_slots();
        let reset_str = "2026-11-27T12:00:00Z";
        let reset_ep = iso_epoch(reset_str).unwrap();
        let cfg = enabled_cfg();

        // A was 0% at reset_ep - 600; B is measured 50%/60% (Green).
        let q = mk_pool_quota(reset_ep - 600, (0.0, 80.0), (50.0, 60.0), reset_str);

        // At reset_ep + 30 (within 60s buffer): A is STILL Red!
        let ev_buf = evaluate_slot_pool(&q["accounts"]["acct-a"], QuotaPool::Gemini, &cfg, 0, reset_ep + 30);
        assert_eq!(ev_buf.tier, QuotaTier::Red);
        assert_eq!(ev_buf.raw_five_hour_pct, Some(0.0));

        // At reset_ep + 60: A's 5h entry is reset -> treated as None (unknown, NOT 100.0%).
        let ev_after = evaluate_slot_pool(&q["accounts"]["acct-a"], QuotaPool::Gemini, &cfg, 0, reset_ep + 60);
        assert_eq!(ev_after.tier, QuotaTier::Green);
        assert_eq!(ev_after.raw_five_hour_pct, None);

        // When current is B(50%/60%, Green), post-reset A does NOT override measured B!
        let mut pc = BTreeMap::new();
        pc.insert(QuotaPool::Gemini, "acct-b".to_string());
        let d = select_account_for_new_session(&root, &state, &q, &pc, &BTreeMap::new(), "agy/gemini-4-neon", &cfg, reset_ep + 60);
        assert_eq!(d.selected_id.as_deref(), Some("acct-b"));
        assert!(!d.pointer_changed);
    }

    #[test]
    fn carbon_counterexample_passive_exhausted_excludes_429_and_synthesizes_15m_reset() {
        // Transient 429 / RPM rate limits MUST NOT trigger passive exhaustion.
        for non_quota in [
            "HTTP 429 Too Many Requests",
            "429 rate limit exceeded",
            "rate_limit_exceeded: try again in 5s",
            "429 RESOURCE_EXHAUSTED: requests per minute limit",
            "capacity temporarily unavailable",
        ] {
            assert!(
                !is_quota_exhausted_error(non_quota),
                "429/rate-limit must NOT be treated as quota exhaustion: {non_quota}"
            );
        }
        for hard_quota in [
            "insufficient_quota: plan limit reached",
            "Error: quota exceeded for this account",
            "insufficient credit balance",
            "Five hour limit reached",
            "usage limit exceeded",
            "RESOURCE_EXHAUSTED: no tokens left",
            "Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 1h6m5s.",
            r#"AGY_ERROR: {"short_error":"RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 1h6m5s.","status":"RESOURCE_EXHAUSTED","error_code":429,"code_kind":"http","retryable":true}"#,
            "429 RESOURCE_EXHAUSTED: quota reached",
        ] {
            assert!(
                is_quota_exhausted_error(hard_quota),
                "hard quota error should match: {hard_quota}"
            );
        }

        // Passive exhaustion with NO prior cache synthesizes resetAt = now + 15m and preserves weekly/history.
        let base = TempHome::create().unwrap();
        let state_root = base.path.join("state");
        fs::create_dir_all(&state_root).unwrap();
        let now_ep = iso_epoch("2026-11-27T10:00:00Z").unwrap();
        save_quota_cache_in(
            &state_root,
            &json!({
                "schemaVersion": 1,
                "updatedEpoch": now_ep - 300,
                "accounts": {
                    "acct-a": {
                        "ok": true,
                        "fetchedEpoch": now_ep - 300,
                        "entries": [
                            {"pool": "Gemini Models", "window": "weekly", "remainingPct": 77.0, "resetAt": "2026-12-04T00:00:00Z"}
                        ],
                        "history": [{"epoch": now_ep - 300, "weekly": 77.0}]
                    }
                }
            }),
        )
        .unwrap();

        mark_slot_quota_exhausted_passive_in(&state_root, "acct-a", "agy/gemini-4-neon", now_ep).unwrap();
        let cache = load_quota_cache_in(&state_root);
        let acct = &cache["accounts"]["acct-a"];
        // Preserves original account fetchedEpoch, weekly, and history.
        assert_eq!(acct["fetchedEpoch"].as_i64(), Some(now_ep - 300));
        assert_eq!(acct["history"].as_array().unwrap().len(), 1);
        let fh_entry = acct["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| s(e, "window") == "fiveHour")
            .expect("synthesized fiveHour passive entry");
        assert_eq!(s(fh_entry, "source"), "passive_error");
        assert_eq!(fh_entry["remainingPct"].as_f64(), Some(0.0));
        let expected_reset = format_epoch(now_ep + 900, false, "%Y-%m-%dT%H:%M:%SZ");
        assert_eq!(s(fh_entry, "resetAt"), expected_reset);

        // Immediately Red; expires after 15m + 60s buffer so it never permanently locks out.
        let cfg = enabled_cfg();
        let ev_now = evaluate_slot_pool(acct, QuotaPool::Gemini, &cfg, 0, now_ep + 60);
        assert_eq!(ev_now.tier, QuotaTier::Red);
        assert!(ev_now.passive_exhausted);
        let ev_expired = evaluate_slot_pool(acct, QuotaPool::Gemini, &cfg, 0, now_ep + 900 + 60);
        assert!(!ev_expired.passive_exhausted);
        assert_eq!(ev_expired.tier, QuotaTier::Green);
    }

    #[test]
    fn carbon_counterexample_stale_cache_is_unknown_and_401_excludes_slot() {
        let (_base, root, _, _, state) = setup_two_test_slots();
        let now_ep = iso_epoch("2026-11-27T10:00:00Z").unwrap();
        let cfg = enabled_cfg();
        let mut pc = BTreeMap::new();
        pc.insert(QuotaPool::Gemini, "acct-a".to_string());

        // acct-a has very stale data (>2h for 5h, >24h for 7d) showing low 5% -> degraded to Unknown (Green),
        // so it MUST maintain current acct-a instead of switching on stale numbers!
        let stale_q = json!({
            "accounts": {
                "acct-a": {
                    "ok": true,
                    "fetchedEpoch": now_ep - 25 * 3600,
                    "entries": [
                        {"pool": "Gemini Models", "window": "fiveHour", "remainingPct": 5.0, "resetAt": "2026-11-28T00:00:00Z"},
                        {"pool": "Gemini Models", "window": "weekly", "remainingPct": 5.0, "resetAt": "2026-12-04T00:00:00Z"}
                    ]
                },
                "acct-b": {
                    "ok": true,
                    "fetchedEpoch": now_ep,
                    "entries": [
                        {"pool": "Gemini Models", "window": "fiveHour", "remainingPct": 90.0, "resetAt": "2026-11-27T15:00:00Z"},
                        {"pool": "Gemini Models", "window": "weekly", "remainingPct": 90.0, "resetAt": "2026-12-04T00:00:00Z"}
                    ]
                }
            }
        });
        let ev_stale = evaluate_slot_pool(&stale_q["accounts"]["acct-a"], QuotaPool::Gemini, &cfg, 0, now_ep);
        assert!(ev_stale.unknown);
        assert_eq!(ev_stale.tier, QuotaTier::Green);
        let d = select_account_for_new_session(&root, &state, &stale_q, &pc, &BTreeMap::new(), "agy/gemini-4-neon", &cfg, now_ep);
        assert_eq!(d.selected_id.as_deref(), Some("acct-a"));
        assert!(!d.pointer_changed);

        // 401 authFailedEpoch excludes acct-a even if old cached quota was 90%.
        let future_ep = epoch() as i64 + 3600;
        let auth_failed_q = json!({
            "accounts": {
                "acct-a": {
                    "ok": false,
                    "fetchedEpoch": now_ep - 600,
                    "authFailedEpoch": future_ep,
                    "entries": [
                        {"pool": "Gemini Models", "window": "fiveHour", "remainingPct": 90.0, "resetAt": "2026-11-27T15:00:00Z"}
                    ]
                },
                "acct-b": {
                    "ok": true,
                    "fetchedEpoch": now_ep,
                    "entries": [
                        {"pool": "Gemini Models", "window": "fiveHour", "remainingPct": 80.0, "resetAt": "2026-11-27T15:00:00Z"}
                    ]
                }
            }
        });
        assert!(check_slot_health(&root, &state, &auth_failed_q, "acct-a").is_err());
        let d_auth = select_account_for_new_session(&root, &state, &auth_failed_q, &pc, &BTreeMap::new(), "agy/gemini-4-neon", &cfg, now_ep);
        assert_eq!(d_auth.selected_id.as_deref(), Some("acct-b"));
    }

    #[test]
    fn carbon_counterexample_prepare_active_never_triggers_auto_switch() {
        let (_base, root, real_home, real_cfg, _state) = setup_two_test_slots();
        let now_ep = iso_epoch("2026-11-27T10:00:00Z").unwrap();
        let cfg = enabled_cfg();
        // acct-a (active) is 0% Red, acct-b is 95% Green.
        let q = mk_pool_quota(now_ep, (0.0, 50.0), (95.0, 95.0), "2026-11-27T15:00:00Z");

        // allow_auto_switch = false (used by `prepare_active` / `probe`) MUST keep active acct-a!
        let (prep_active, ptr_upd) = AgyCredOverlay::prepare_for_session_full(
            &root,
            &real_home,
            &real_cfg,
            None,
            None,
            "",
            false,
            &cfg,
            &q,
            &BTreeMap::new(),
            &BTreeMap::new(),
            now_ep,
        )
        .unwrap();
        assert_eq!(prep_active.account_id.as_deref(), Some("acct-a"));
        assert!(prep_active.switched.is_none());
        assert!(ptr_upd.is_none());

        // Contrast: allow_auto_switch = true for new session switches to acct-b and updates pointer!
        let (prep_new, ptr_upd2) = AgyCredOverlay::prepare_for_session_full(
            &root,
            &real_home,
            &real_cfg,
            None,
            None,
            "agy/gemini-4-neon",
            true,
            &cfg,
            &q,
            &BTreeMap::new(),
            &BTreeMap::new(),
            now_ep,
        )
        .unwrap();
        assert_eq!(prep_new.account_id.as_deref(), Some("acct-b"));
        assert_eq!(prep_new.switched.as_ref().unwrap().kind, "auto");
        assert_eq!(ptr_upd2, Some((QuotaPool::Gemini, "acct-b".to_string())));
    }

    #[test]
    fn carbon_counterexample_unified_pool_matching_and_unknown_model_safety() {
        for m in [
            "agy/gemini-4-neon",
            "neon",
            "carbon",
            "agy/gemini-4-carbon",
            "hydrogen",
            "agy/gemini-4-hydrogen",
            "gemini-2.5-flash",
            "gemini-2.5-pro",
        ] {
            assert_eq!(classify_model_pool(m), Some(QuotaPool::Gemini), "model={m}");
            assert!(model_matches_pool(m, "Gemini Models"));
            assert!(!model_matches_pool(m, "Claude and GPT models"));
        }
        for m in [
            "agy/claude-opus-4-6-thinking",
            "claude-sonnet-4",
            "haiku",
            "openai/gpt-5.4-pro",
            "codex",
        ] {
            assert_eq!(classify_model_pool(m), Some(QuotaPool::ClaudeGpt), "model={m}");
            assert!(model_matches_pool(m, "Claude and GPT models"));
            assert!(!model_matches_pool(m, "Gemini Models"));
        }
        assert_eq!(classify_model_pool("provider/mystery-v1"), None);
        assert_eq!(classify_model_pool(""), None);

        // Unknown model reads active_id (avoids 0%) and NEVER mutates poolCurrent!
        let (_base, root, _, _, state) = setup_two_test_slots();
        let now_ep = iso_epoch("2026-11-27T10:00:00Z").unwrap();
        let cfg = enabled_cfg();
        let mut pc = BTreeMap::new();
        pc.insert(QuotaPool::Gemini, "acct-b".to_string());
        let q_ok = mk_pool_quota(now_ep, (60.0, 60.0), (90.0, 90.0), "2026-11-27T15:00:00Z");
        let d1 = select_account_for_new_session(&root, &state, &q_ok, &pc, &BTreeMap::new(), "agy/mystery-v1", &cfg, now_ep);
        assert_eq!(d1.selected_id.as_deref(), Some("acct-a")); // active_id, not poolCurrent
        assert!(!d1.pointer_changed);

        let q_zero = mk_pool_quota(now_ep, (0.0, 60.0), (90.0, 90.0), "2026-11-27T15:00:00Z");
        let d2 = select_account_for_new_session(&root, &state, &q_zero, &pc, &BTreeMap::new(), "agy/mystery-v1", &cfg, now_ep);
        assert_eq!(d2.selected_id.as_deref(), Some("acct-b")); // avoids 0% slot
        assert!(!d2.pointer_changed); // NEVER overwrites poolCurrent
    }

    #[test]
    fn singleflight_refresh_dedups_concurrent_callers_respects_15m_and_failure_backoff() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        let (base, root, _, _, _state) = setup_two_test_slots();
        let state_root = base.path.join("state");
        fs::create_dir_all(&state_root).unwrap();
        let now_ep = iso_epoch("2026-11-27T10:00:00Z").unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let opts = QuotaRefreshOptions::auto_stale(900, None);

        // Two threads call refresh simultaneously when cache is empty -> singleflight lock + double-check
        // guarantees each slot is fetched ONLY ONCE (2 total slot fetches, not 4).
        thread::scope(|sc| {
            let mut hs = Vec::new();
            for _ in 0..2 {
                let root_ref = &root;
                let state_ref = &state_root;
                let opts_ref = &opts;
                let calls_c = calls.clone();
                hs.push(sc.spawn(move || {
                    refresh_quotas_singleflight_in(
                        root_ref,
                        state_ref,
                        opts_ref,
                        || now_ep,
                        |id, email| {
                            calls_c.fetch_add(1, Ordering::SeqCst);
                            thread::sleep(Duration::from_millis(80));
                            SlotQuotaFetch {
                                id: id.to_string(),
                                ok: true,
                                identity_mismatch: false,
                                expected_email: email.to_string(),
                                observed_email: String::new(),
                                error: None,
                                parsed: ParsedAgyUsage {
                                    entries: vec![QuotaPoolEntry {
                                        pool: "Gemini Models".into(),
                                        window: "fiveHour".into(),
                                        window_label: "Five Hour Limit Remaining".into(),
                                        remaining_pct: 88.0,
                                        reset_at: "2026-11-27T15:00:00Z".into(),
                                    }],
                                    unknown_lines: vec![],
                                },
                                refreshed_creds: 0,
                            }
                        },
                    )
                    .unwrap()
                }));
            }
            for h in hs {
                let _ = h.join().unwrap();
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 2, "2 slots fetched once total across 2 concurrent threads");

        // Within 15m (now + 600s): 0 fetches.
        let calls_before = calls.load(Ordering::SeqCst);
        let _ = refresh_quotas_singleflight_in(
            &root,
            &state_root,
            &opts,
            || now_ep + 600,
            |id, email| {
                calls.fetch_add(1, Ordering::SeqCst);
                SlotQuotaFetch::error_result(id, email, "should not be called".into())
            },
        )
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), calls_before);

        // Forced (`--quota` / `Q`): penetrates 15m throttle!
        let _ = refresh_quotas_singleflight_in(
            &root,
            &state_root,
            &QuotaRefreshOptions::forced(),
            || now_ep + 600,
            |id, email| {
                calls.fetch_add(1, Ordering::SeqCst);
                SlotQuotaFetch::error_result(id, email, "network timeout".into())
            },
        )
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), calls_before + 2);
        // Failed fetch preserves prior entries & fetchedEpoch, and triggers 5m backoff!
        let after_fail = load_quota_cache_in(&state_root);
        assert_eq!(after_fail["accounts"]["acct-a"]["fetchedEpoch"].as_i64(), Some(now_ep));
        assert_eq!(after_fail["accounts"]["acct-a"]["entries"].as_array().unwrap().len(), 1);
        assert_eq!(after_fail["accounts"]["acct-a"]["lastAttemptEpoch"].as_i64(), Some(now_ep + 600));

        // Even past 15m from original fetch (now + 950s, but only 350s? wait: 600 + 200 = 800 -> within 5m backoff):
        // check now_ep + 700 (only 100s after failed attempt at +600) with min_interval=60 -> skipped due to 5m backoff!
        let backoff_opts = QuotaRefreshOptions::auto_stale(60, None);
        let calls_before_backoff = calls.load(Ordering::SeqCst);
        let _ = refresh_quotas_singleflight_in(
            &root,
            &state_root,
            &backoff_opts,
            || now_ep + 700,
            |id, email| {
                calls.fetch_add(1, Ordering::SeqCst);
                SlotQuotaFetch::error_result(id, email, "should back off".into())
            },
        )
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), calls_before_backoff);
    }

    #[test]
    fn pick_report_and_format_account_switch_line_cover_all_kinds_without_emails() {
        let (_base, root, _, _, state) = setup_two_test_slots();
        let now_ep = iso_epoch("2026-11-27T10:00:00Z").unwrap();
        let cfg = enabled_cfg();
        let q = mk_pool_quota(now_ep, (20.0, 80.0), (90.0, 85.0), "2026-11-27T15:00:00Z");
        let rep = build_pick_report_in(
            &root,
            &state,
            &q,
            &BTreeMap::new(),
            &BTreeMap::new(),
            Some("neon"),
            Some("agy/gemini-4-neon"),
            &cfg,
            now_ep,
        );
        assert_eq!(rep["selected"], "acct-b");
        assert_eq!(rep["wouldSwitch"], true);
        assert_eq!(rep["pool"], "gemini");
        assert!(!rep.to_string().contains("example.test"), "pick report must not leak emails");

        let auto_line = format_account_switch_line(
            &json!({"kind": "auto", "from": "acct-a", "to": "acct-b", "reason": "acct-a 黄档"}),
            false,
        )
        .unwrap();
        assert!(auto_line.starts_with("自动选号：acct-a → acct-b"));
        let handoff_line = format_account_switch_line(
            &json!({"kind": "handoff", "from": "acct-a", "to": "acct-b", "reason": "运行中额度耗尽"}),
            false,
        )
        .unwrap();
        assert!(handoff_line.starts_with("中途换号接力：acct-a → acct-b"));
        let rev_handoff = format_account_switch_line(
            &json!({"kind": "handoff", "from": "acct-a", "to": "acct-b", "reason": "完整模板换号重审"}),
            true,
        )
        .unwrap();
        assert!(rev_handoff.starts_with("审查换号重审：acct-a → acct-b"));
    }

    #[test]
    fn legacy_accounts_json_without_mode_defaults_to_fixed_and_pick_reports_fixed_slot() {
        let (_base, root, _, _, _) = setup_two_test_slots();
        let now_ep = iso_epoch("2026-11-27T10:00:00Z").unwrap();
        let cfg = enabled_cfg();
        // Legacy accounts.json has NO "mode" field -> must be treated as fixed mode!
        let legacy_state = json!({
            "schemaVersion": SCHEMA_VERSION,
            "active": "acct-a",
            "accounts": {
                "acct-a": {"id": "acct-a", "email": "a@example.test"},
                "acct-b": {"id": "acct-b", "email": "b@example.test"}
            }
        });
        assert!(!is_auto_mode_state(&legacy_state));

        // Even though acct-a is 10% Yellow and acct-b is 95% Green, fixed mode stays on acct-a!
        let q = mk_pool_quota(now_ep, (10.0, 80.0), (95.0, 95.0), "2026-11-27T15:00:00Z");
        let d = select_account_for_new_session(
            &root,
            &legacy_state,
            &q,
            &BTreeMap::new(),
            &BTreeMap::new(),
            "agy/gemini-4-neon",
            &cfg,
            now_ep,
        );
        assert_eq!(d.selected_id.as_deref(), Some("acct-a"));
        assert!(!d.pointer_changed);
        assert!(d.switched.is_none());

        // `pick` in fixed mode works cleanly and reports the fixed active slot.
        let rep = build_pick_report_in(
            &root,
            &legacy_state,
            &q,
            &BTreeMap::new(),
            &BTreeMap::new(),
            Some("neon"),
            Some("agy/gemini-4-neon"),
            &cfg,
            now_ep,
        );
        assert_eq!(rep["mode"], "fixed");
        assert_eq!(rep["selected"], "acct-a");
        assert_eq!(rep["wouldSwitch"], false);
        assert!(s(&rep, "reason").contains("固定号"));

        // `derive_id` never collides with reserved `"auto"`.
        assert_ne!(derive_id("auto@example.test", &Map::new()), "auto");
    }

    #[test]
    fn build_quota_history_point_records_reset_epochs_and_isolates_same_cycle() {
        let now_ep = iso_epoch("2026-11-27T10:00:00Z").unwrap();
        let reset_5h = "2026-11-27T14:30:00Z";
        let reset_7d = "2026-12-04T07:00:00Z";
        let entries = vec![
            QuotaPoolEntry {
                pool: "Gemini Models".into(),
                window: "fiveHour".into(),
                window_label: "Five Hour Limit Remaining".into(),
                remaining_pct: 80.0,
                reset_at: reset_5h.into(),
            },
            QuotaPoolEntry {
                pool: "Gemini Models".into(),
                window: "weekly".into(),
                window_label: "Weekly Limit Remaining".into(),
                remaining_pct: 65.0,
                reset_at: reset_7d.into(),
            },
            QuotaPoolEntry {
                pool: "Claude and GPT models".into(),
                window: "fiveHour".into(),
                window_label: "Five Hour Limit Remaining".into(),
                remaining_pct: 100.0,
                reset_at: "1970-01-01T00:00:00Z".into(),
            },
        ];
        let pt = build_quota_history_point(now_ep, &entries).expect("history point");
        assert_eq!(
            pt["pools"]["Gemini Models"]["fiveHourResetEp"].as_i64(),
            iso_epoch(reset_5h)
        );
        assert_eq!(
            pt["pools"]["Gemini Models"]["weeklyResetEp"].as_i64(),
            iso_epoch(reset_7d)
        );
        assert_eq!(
            pt["pools"]["Claude and GPT models"]["fiveHourResetEp"].as_i64(),
            Some(0)
        );

        // Cross-reset contamination test: previous cycle ended 20m ago at 98%, new cycle started 15m ago at 96%.
        // With `fiveHourResetEp` recorded, previous cycle MUST NOT be mixed into current cycle!
        let cur_reset_ep = now_ep + 4 * 3600 + 45 * 60; // started 15m ago
        let prev_reset_ep = now_ep - 20 * 60;
        let cur_reset_iso = format_epoch(cur_reset_ep, false, "%Y-%m-%dT%H:%M:%SZ");
        let acct_quota = json!({
            "ok": true,
            "fetchedEpoch": now_ep,
            "entries": [
                {
                    "pool": "Gemini Models",
                    "window": "fiveHour",
                    "remainingPct": 96.0,
                    "resetAt": cur_reset_iso
                }
            ],
            "history": [
                {
                    "epoch": now_ep - 40 * 60,
                    "pools": {
                        "Gemini Models": {
                            "fiveHour": 98.0,
                            "fiveHourResetEp": prev_reset_ep
                        }
                    }
                },
                {
                    "epoch": now_ep,
                    "pools": {
                        "Gemini Models": {
                            "fiveHour": 96.0,
                            "fiveHourResetEp": cur_reset_ep
                        }
                    }
                }
            ]
        });
        let cfg = AgyAutoSwitchConfig::default();
        let st = evaluate_pool_quota(&acct_quota, QuotaPool::Gemini, &cfg, 1, 1, true, now_ep);
        assert!(
            matches!(
                st.eta_five_hour,
                QuotaEta::Insufficient {
                    reason: "too_early",
                    ..
                }
            ),
            "15m into fresh cycle must be Insufficient(too_early), got {:?}",
            st.eta_five_hour
        );
    }

    #[test]
    fn pool_quota_eta_gates_insufficient_idle_multi_machine_and_cap_at_reset() {
        let cfg = AgyAutoSwitchConfig::default();
        let base_ep = iso_epoch("2026-11-27T10:00:00Z").unwrap();

        // 1. Reproduce 03:04 complaint: 20m observed (< 25m), 90% remaining, 27m wall clock -> Insufficient, no alert!
        let t_start = base_ep - 27 * 60;
        let reset_ep = t_start + FIVE_HOUR_WINDOW_SECS;
        let fetched_ep = t_start + 20 * 60;
        let reset_iso = format_epoch(reset_ep, false, "%Y-%m-%dT%H:%M:%SZ");
        let fresh_quota = json!({
            "ok": true,
            "fetchedEpoch": fetched_ep,
            "entries": [{
                "pool": "Gemini Models",
                "window": "fiveHour",
                "remainingPct": 90.0,
                "resetAt": reset_iso
            }],
            "history": [{
                "epoch": fetched_ep,
                "pools": { "Gemini Models": { "fiveHour": 90.0, "fiveHourResetEp": reset_ep } }
            }]
        });
        let st_fresh = evaluate_pool_quota(&fresh_quota, QuotaPool::Gemini, &cfg, 1, 1, true, base_ep);
        assert!(matches!(
            st_fresh.eta_five_hour,
            QuotaEta::Insufficient {
                reason: "too_early",
                elapsed_mins: Some(27)
            }
        ));

        // 2. Idle gate vs multi-machine remote burn:
        //    - k_run == 0 and no recent drop -> Idle
        //    - k_run == 0, selected, fresh (<20m), recent drop > 0 -> NOT Idle (remote machine is burning)
        let active_reset_ep = base_ep + 3 * 3600; // started 2h ago
        let active_reset_iso = format_epoch(active_reset_ep, false, "%Y-%m-%dT%H:%M:%SZ");
        let steady_idle = json!({
            "ok": true,
            "fetchedEpoch": base_ep - 300,
            "entries": [{
                "pool": "Gemini Models",
                "window": "fiveHour",
                "remainingPct": 40.0,
                "resetAt": active_reset_iso
            }],
            "history": [
                { "epoch": base_ep - 2100, "pools": { "Gemini Models": { "fiveHour": 55.0, "fiveHourResetEp": active_reset_ep } } },
                { "epoch": base_ep - 1200, "pools": { "Gemini Models": { "fiveHour": 40.0, "fiveHourResetEp": active_reset_ep } } },
                { "epoch": base_ep - 300,  "pools": { "Gemini Models": { "fiveHour": 40.0, "fiveHourResetEp": active_reset_ep } } }
            ]
        });
        let st_idle = evaluate_pool_quota(&steady_idle, QuotaPool::Gemini, &cfg, 0, 0, true, base_ep);
        assert_eq!(st_idle.eta_five_hour, QuotaEta::Idle);

        let remote_burn = json!({
            "ok": true,
            "fetchedEpoch": base_ep - 300,
            "entries": [{
                "pool": "Gemini Models",
                "window": "fiveHour",
                "remainingPct": 35.0,
                "resetAt": active_reset_iso
            }],
            "history": [
                { "epoch": base_ep - 2100, "pools": { "Gemini Models": { "fiveHour": 55.0, "fiveHourResetEp": active_reset_ep } } },
                { "epoch": base_ep - 1200, "pools": { "Gemini Models": { "fiveHour": 45.0, "fiveHourResetEp": active_reset_ep } } },
                { "epoch": base_ep - 300,  "pools": { "Gemini Models": { "fiveHour": 35.0, "fiveHourResetEp": active_reset_ep } } }
            ]
        });
        let st_remote = evaluate_pool_quota(&remote_burn, QuotaPool::Gemini, &cfg, 0, 0, true, base_ep);
        assert!(
            matches!(st_remote.eta_five_hour, QuotaEta::DepletesIn { alert: false, .. }),
            "remote burn with k_run=0 must not be falsely silenced to Idle: {:?}",
            st_remote.eta_five_hour
        );
        // Same state with k_run=2, k_write=2 -> unified eff_5h = 35 - 10 = 25 (Yellow tier) AND alert = true!
        let st_local = evaluate_pool_quota(&remote_burn, QuotaPool::Gemini, &cfg, 2, 2, true, base_ep);
        assert_eq!(st_local.tier, QuotaTier::Yellow);
        assert_eq!(st_local.eff_five_hour_pct, Some(25.0));
        match st_local.eta_five_hour {
            QuotaEta::DepletesIn { fast_m, mid_m, alert, .. } => {
                assert!(alert);
                assert!(fast_m <= mid_m);
                assert!(mid_m * 60 < 3 * 3600, "ETA must stay strictly below resetAt");
            }
            ref other => panic!("expected DepletesIn(alert=true), got {other:?}"),
        }

        // 3. Weekly staleness split: 14h old cache -> auto-switch tier STILL sees weekly_pct (24h limit),
        //    while ETA returns Insufficient("stale") (12h limit)!
        let wk_reset_ep = base_ep + 5 * 86400;
        let wk_reset_iso = format_epoch(wk_reset_ep, false, "%Y-%m-%dT%H:%M:%SZ");
        let wk_14h_old = json!({
            "ok": true,
            "fetchedEpoch": base_ep - 14 * 3600,
            "entries": [{
                "pool": "Gemini Models",
                "window": "weekly",
                "remainingPct": 25.0,
                "resetAt": wk_reset_iso
            }],
            "history": []
        });
        let st_wk = evaluate_pool_quota(&wk_14h_old, QuotaPool::Gemini, &cfg, 1, 1, true, base_ep);
        assert_eq!(st_wk.tier, QuotaTier::Yellow, "24h auto-switch tier must remain unchanged");
        assert_eq!(st_wk.weekly_pct, Some(25.0));
        assert!(matches!(
            st_wk.eta_weekly,
            QuotaEta::Insufficient { reason: "stale", .. }
        ));
    }

    #[test]
    fn replay_backtest_locks_5h_and_weekly_false_positive_rates() {
        let raw = include_str!("../tests/fixtures/quota_replay_history.json");
        assert!(!raw.contains('@'), "public replay fixture must not contain emails");
        let doc: Value = serde_json::from_str(raw).expect("valid replay fixture JSON");
        let slots = doc["slots"].as_array().expect("slots array");
        assert_eq!(slots.len(), 2);

        let cfg = AgyAutoSwitchConfig::default();

        for min_sample_gap_secs in [0i64, 15 * 60i64] {
            let mut fp_5h = 0usize;
            let mut total_5h_non_exh = 0usize;
            let mut fp_7d = 0usize;
            let mut total_7d = 0usize;
            let mut saw_true_exh_alert = false;
            let mut post_reset_90_alerts = 0usize;

            for slot in slots {
                let hist_all = slot["history"].as_array().expect("history");

                // Replay 5h cycles
                for cyc in slot["cycles5h"].as_array().expect("cycles5h") {
                    let s_idx = cyc["startIdx"].as_u64().unwrap() as usize;
                    let e_idx = cyc["endIdx"].as_u64().unwrap() as usize;
                    let reset_ep = cyc["resetEp"].as_i64().unwrap();
                    let completed = b(cyc, "completed");
                    let exhausted = b(cyc, "exhausted");
                    let near_zero = b(cyc, "nearZeroManualSwitch");
                    let true_zero_ep = cyc["trueZeroEp"].as_i64();
                    let reset_iso = format_epoch(reset_ep, false, "%Y-%m-%dT%H:%M:%SZ");

                    // Apply optional >=15m sampling throttle
                    let mut sampled: Vec<Value> = Vec::new();
                    let mut last_kept_ep = 0i64;
                    for item in &hist_all[..=e_idx] {
                        let ep = item["epoch"].as_i64().unwrap();
                        if min_sample_gap_secs > 0 && last_kept_ep > 0 && ep - last_kept_ep < min_sample_gap_secs {
                            continue;
                        }
                        sampled.push(item.clone());
                        last_kept_ep = ep;
                    }

                    let seg_start_ep = hist_all[s_idx]["epoch"].as_i64().unwrap();
                    for (si, pt) in sampled.iter().enumerate() {
                        let ep = pt["epoch"].as_i64().unwrap();
                        if ep < seg_start_ep {
                            continue;
                        }
                        let pct = pt["pools"]["Gemini Models"]["fiveHour"].as_f64().unwrap();
                        if pct <= 0.0 || pct >= 100.0 {
                            continue;
                        }
                        let prev_pct = if si > 0 && sampled[si - 1]["epoch"].as_i64().unwrap() >= seg_start_ep {
                            sampled[si - 1]["pools"]["Gemini Models"]["fiveHour"].as_f64().unwrap()
                        } else {
                            100.0
                        };
                        let k_run = usize::from(prev_pct > pct);
                        let acct_quota = json!({
                            "ok": true,
                            "fetchedEpoch": ep,
                            "entries": [{
                                "pool": "Gemini Models",
                                "window": "fiveHour",
                                "remainingPct": pct,
                                "resetAt": reset_iso
                            }],
                            "history": &sampled[..=si]
                        });
                        let st = evaluate_pool_quota(
                            &acct_quota,
                            QuotaPool::Gemini,
                            &cfg,
                            k_run,
                            k_run,
                            true,
                            ep,
                        );
                        let alerted = matches!(st.eta_five_hour, QuotaEta::DepletesIn { alert: true, .. });

                        // Post-reset high-margin check (first 30m with >= 88% remaining -> 0 alerts!)
                        let elapsed_from_start = ep - (reset_ep - FIVE_HOUR_WINDOW_SECS);
                        if elapsed_from_start <= 30 * 60 && pct >= 88.0 && alerted {
                            post_reset_90_alerts += 1;
                        }

                        if exhausted {
                            if let Some(tz) = true_zero_ep {
                                if ep < tz && alerted {
                                    saw_true_exh_alert = true;
                                    // At 73% remaining (~93m before T_zero), check ETA accuracy within ±15m!
                                    if (pct - 73.0).abs() < 0.5 {
                                        if let QuotaEta::DepletesIn { mid_m, .. } = st.eta_five_hour {
                                            let actual_m = (tz - ep) / 60;
                                            let err = (mid_m as i64 - actual_m).abs();
                                            assert!(
                                                err <= 15,
                                                "true depletion ETA error too large at 73%: pred={mid_m}m actual={actual_m}m"
                                            );
                                        }
                                    }
                                }
                            }
                        } else if completed && !near_zero {
                            total_5h_non_exh += 1;
                            if alerted {
                                fp_5h += 1;
                            }
                        }
                    }
                }

                // Replay weekly cycles
                for cyc in slot["cycles7d"].as_array().expect("cycles7d") {
                    let s_idx = cyc["startIdx"].as_u64().unwrap() as usize;
                    let e_idx = cyc["endIdx"].as_u64().unwrap() as usize;
                    let reset_ep = cyc["resetEp"].as_i64().unwrap();
                    let reset_iso = format_epoch(reset_ep, false, "%Y-%m-%dT%H:%M:%SZ");

                    let mut sampled: Vec<Value> = Vec::new();
                    let mut last_kept_ep = 0i64;
                    for item in &hist_all[..=e_idx] {
                        let ep = item["epoch"].as_i64().unwrap();
                        if min_sample_gap_secs > 0 && last_kept_ep > 0 && ep - last_kept_ep < min_sample_gap_secs {
                            continue;
                        }
                        sampled.push(item.clone());
                        last_kept_ep = ep;
                    }
                    let seg_start_ep = hist_all[s_idx]["epoch"].as_i64().unwrap();
                    for (si, pt) in sampled.iter().enumerate() {
                        let ep = pt["epoch"].as_i64().unwrap();
                        if ep < seg_start_ep {
                            continue;
                        }
                        let pct = pt["pools"]["Gemini Models"]["weekly"].as_f64().unwrap();
                        if pct <= 0.0 || pct >= 100.0 {
                            continue;
                        }
                        let prev_pct = if si > 0 && sampled[si - 1]["epoch"].as_i64().unwrap() >= seg_start_ep {
                            sampled[si - 1]["pools"]["Gemini Models"]["weekly"].as_f64().unwrap()
                        } else {
                            100.0
                        };
                        let k_run = usize::from(prev_pct > pct);
                        let acct_quota = json!({
                            "ok": true,
                            "fetchedEpoch": ep,
                            "entries": [{
                                "pool": "Gemini Models",
                                "window": "weekly",
                                "remainingPct": pct,
                                "resetAt": reset_iso
                            }],
                            "history": &sampled[..=si]
                        });
                        let st = evaluate_pool_quota(
                            &acct_quota,
                            QuotaPool::Gemini,
                            &cfg,
                            k_run,
                            k_run,
                            true,
                            ep,
                        );
                        total_7d += 1;
                        if matches!(st.eta_weekly, QuotaEta::DepletesIn { alert: true, .. }) {
                            fp_7d += 1;
                        }
                    }
                }
            }

            assert!(total_5h_non_exh >= 50, "expected >=50 non-exhausted 5h points, got {total_5h_non_exh}");
            assert!(total_7d >= 100, "expected >=100 weekly points, got {total_7d}");
            let fpr_5h = (fp_5h as f64) / (total_5h_non_exh as f64);
            assert!(
                fpr_5h <= 0.03,
                "5h FPR regression (throttle={min_sample_gap_secs}s): {fp_5h}/{total_5h_non_exh} = {:.1}%",
                fpr_5h * 100.0
            );
            assert_eq!(
                fp_7d, 0,
                "weekly FPR regression (throttle={min_sample_gap_secs}s): {fp_7d}/{total_7d}"
            );
            assert_eq!(
                post_reset_90_alerts, 0,
                "post-reset high-margin (>=88% within 30m) must have 0 alerts"
            );
            if min_sample_gap_secs == 0 {
                assert!(
                    saw_true_exh_alert,
                    "true exhaustion cycle must trigger a pre-depletion alert"
                );
            }
        }
    }

    fn mk_shadow_snap(
        mode: &str,
        active: &str,
        accounts: Value,
        auto_switch: Option<Value>,
    ) -> Value {
        let mut v = json!({
            "mode": mode,
            "active": active,
            "accounts": accounts,
        });
        if let Some(asw) = auto_switch {
            v["autoSwitch"] = asw;
        }
        v
    }

    #[test]
    fn shadow_headroom_fits_when_remaining_covers_writers_plus_one() {
        let now_ep: i64 = 1_780_000_000;
        let snap = mk_shadow_snap(
            "fixed",
            "work",
            json!([{
                "id": "work",
                "active": true,
                "quota": {
                    "ok": true,
                    "fetchedEpoch": now_ep - 300,
                    "entries": [{
                        "pool": "Gemini Models",
                        "window": "fiveHour",
                        "remainingPct": 40.0,
                        "resetInSeconds": 10800
                    }]
                },
                "quotaEstimate": {
                    "pools": [{
                        "pool": "Gemini Models",
                        "tier": "yellow",
                        "fiveHour": {
                            "status": "depletes",
                            "alert": true,
                            "midMinutes": 100
                        },
                        "weekly": {
                            "status": "safe"
                        }
                    }]
                }
            }]),
            None,
        );
        let mut writers = BTreeMap::new();
        writers.insert(("work".to_string(), QuotaPool::Gemini), 1);
        // active_writes = 1 -> needs (1 + 1) * 45 = 90m <= 100m -> Fits
        let res = check_write_quota_headroom_from_snapshot(
            &snap,
            QuotaPool::Gemini,
            &writers,
            Some(45),
            now_ep,
        );
        assert_eq!(res, HeadroomCheck::Fits);

        // Safe-only window also fits.
        let safe_snap = mk_shadow_snap(
            "fixed",
            "work",
            json!([{
                "id": "work",
                "active": true,
                "quota": { "ok": true, "fetchedEpoch": now_ep - 60, "entries": [] },
                "quotaEstimate": {
                    "pools": [{
                        "pool": "Gemini Models",
                        "tier": "green",
                        "fiveHour": { "status": "safe" },
                        "weekly": { "status": "idle" }
                    }]
                }
            }]),
            None,
        );
        assert_eq!(
            check_write_quota_headroom_from_snapshot(
                &safe_snap,
                QuotaPool::Gemini,
                &writers,
                Some(45),
                now_ep,
            ),
            HeadroomCheck::Fits
        );
    }

    #[test]
    fn shadow_headroom_would_hold_when_remaining_below_required() {
        let now_ep: i64 = 1_780_000_000;
        let snap = mk_shadow_snap(
            "fixed",
            "work",
            json!([{
                "id": "work",
                "active": true,
                "quota": {
                    "ok": true,
                    "fetchedEpoch": now_ep - 420,
                    "entries": [{
                        "pool": "Gemini Models",
                        "window": "fiveHour",
                        "remainingPct": 25.0,
                        "resetInSeconds": 9000
                    }]
                },
                "quotaEstimate": {
                    "pools": [{
                        "pool": "Gemini Models",
                        "tier": "yellow",
                        "fiveHour": {
                            "status": "depletes",
                            "alert": true,
                            "midMinutes": 75
                        },
                        "weekly": {
                            "status": "depletes",
                            "alert": false,
                            "midMinutes": 200
                        }
                    }]
                }
            }]),
            None,
        );
        let mut writers = BTreeMap::new();
        writers.insert(("work".to_string(), QuotaPool::Gemini), 1);
        // active_writes = 1 -> required = (1 + 1) * 45 = 90m > 75m -> WouldHold
        let res = check_write_quota_headroom_from_snapshot(
            &snap,
            QuotaPool::Gemini,
            &writers,
            Some(45),
            now_ep,
        );
        assert_eq!(
            res,
            HeadroomCheck::WouldHold(HeadroomDecision {
                pool: QuotaPool::Gemini,
                account_id: "work".to_string(),
                window: "fiveHour".to_string(),
                remaining_minutes: 75,
                required_minutes: 90,
                median_write_minutes: 45,
                active_writes: 1,
                cache_age_seconds: 420,
            })
        );

        // Auto mode: another healthy Green account can take over -> Fits.
        let auto_green_snap = mk_shadow_snap(
            "auto",
            "a",
            json!([
                {
                    "id": "a",
                    "active": true,
                    "quota": {
                        "ok": true,
                        "fetchedEpoch": now_ep - 180,
                        "entries": [{
                            "pool": "Gemini Models",
                            "window": "fiveHour",
                            "remainingPct": 15.0,
                            "resetInSeconds": 9000
                        }]
                    },
                    "quotaEstimate": {
                        "pools": [{
                            "pool": "Gemini Models",
                            "tier": "yellow",
                            "fiveHour": { "status": "depletes", "alert": true, "midMinutes": 30 }
                        }]
                    }
                },
                {
                    "id": "b",
                    "active": false,
                    "quota": {
                        "ok": true,
                        "fetchedEpoch": now_ep - 180,
                        "entries": [{
                            "pool": "Gemini Models",
                            "window": "fiveHour",
                            "remainingPct": 90.0,
                            "resetInSeconds": 14400
                        }]
                    },
                    "quotaEstimate": {
                        "pools": [{
                            "pool": "Gemini Models",
                            "tier": "green",
                            "fiveHour": { "status": "safe" }
                        }]
                    }
                }
            ]),
            Some(json!({
                "enabled": true,
                "poolSummary": { "gemini": { "selected": "a" } }
            })),
        );
        assert_eq!(
            check_write_quota_headroom_from_snapshot(
                &auto_green_snap,
                QuotaPool::Gemini,
                &writers,
                Some(45),
                now_ep,
            ),
            HeadroomCheck::Fits
        );

        // Auto mode: both accounts yellow, combined 15% + 15% = 30% -> scale 2.0 -> 30m * 2 = 60m < 90m -> WouldHold.
        let auto_both_low_snap = mk_shadow_snap(
            "auto",
            "a",
            json!([
                {
                    "id": "a",
                    "active": true,
                    "quota": {
                        "ok": true,
                        "fetchedEpoch": now_ep - 180,
                        "entries": [{
                            "pool": "Gemini Models",
                            "window": "fiveHour",
                            "remainingPct": 15.0,
                            "resetInSeconds": 9000
                        }]
                    },
                    "quotaEstimate": {
                        "pools": [{
                            "pool": "Gemini Models",
                            "tier": "yellow",
                            "fiveHour": { "status": "depletes", "alert": true, "midMinutes": 30 }
                        }]
                    }
                },
                {
                    "id": "b",
                    "active": false,
                    "quota": {
                        "ok": true,
                        "fetchedEpoch": now_ep - 180,
                        "entries": [{
                            "pool": "Gemini Models",
                            "window": "fiveHour",
                            "remainingPct": 15.0,
                            "resetInSeconds": 9000
                        }]
                    },
                    "quotaEstimate": {
                        "pools": [{
                            "pool": "Gemini Models",
                            "tier": "yellow",
                            "fiveHour": { "status": "depletes", "alert": true, "midMinutes": 30 }
                        }]
                    }
                }
            ]),
            Some(json!({
                "enabled": true,
                "poolSummary": { "gemini": { "selected": "a" } }
            })),
        );
        let mut auto_writers = BTreeMap::new();
        auto_writers.insert(("a".to_string(), QuotaPool::Gemini), 1);
        match check_write_quota_headroom_from_snapshot(
            &auto_both_low_snap,
            QuotaPool::Gemini,
            &auto_writers,
            Some(45),
            now_ep,
        ) {
            HeadroomCheck::WouldHold(d) => {
                assert_eq!(d.remaining_minutes, 60);
                assert_eq!(d.required_minutes, 90);
                assert_eq!(d.active_writes, 1);
            }
            other => panic!("expected WouldHold in auto all-low, got {other:?}"),
        }
    }

    #[test]
    fn shadow_headroom_unknown_when_cache_older_than_20m() {
        let now_ep: i64 = 1_780_000_000;
        let mk = |age: i64| {
            mk_shadow_snap(
                "fixed",
                "work",
                json!([{
                    "id": "work",
                    "active": true,
                    "quota": {
                        "ok": true,
                        "fetchedEpoch": now_ep - age,
                        "entries": [{
                            "pool": "Gemini Models",
                            "window": "fiveHour",
                            "remainingPct": 10.0,
                            "resetInSeconds": 9000
                        }]
                    },
                    "quotaEstimate": {
                        "pools": [{
                            "pool": "Gemini Models",
                            "tier": "yellow",
                            "fiveHour": { "status": "depletes", "alert": true, "midMinutes": 15 }
                        }]
                    }
                }]),
                None,
            )
        };
        let writers = BTreeMap::new();
        // 20m + 1s = 1201s -> StaleCache (must NOT emit WouldHold)
        assert_eq!(
            check_write_quota_headroom_from_snapshot(
                &mk(SHADOW_HEADROOM_MAX_CACHE_AGE_SECS + 1),
                QuotaPool::Gemini,
                &writers,
                Some(45),
                now_ep,
            ),
            HeadroomCheck::Unknown(HeadroomUnknownReason::StaleCache)
        );
        // Exactly 20m = 1200s -> still fresh, triggers WouldHold
        assert!(matches!(
            check_write_quota_headroom_from_snapshot(
                &mk(SHADOW_HEADROOM_MAX_CACHE_AGE_SECS),
                QuotaPool::Gemini,
                &writers,
                Some(45),
                now_ep,
            ),
            HeadroomCheck::WouldHold(_)
        ));
    }

    #[test]
    fn shadow_headroom_median_write_duration_from_history() {
        let base = env::temp_dir().join(format!(
            "delegate-shadow-median-{}-{}",
            std::process::id(),
            TEMP_HOME_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let state = base.join("state");
        let hdir = state.join("history").join("demo-0123456789ab");
        fs::create_dir_all(&hdir).unwrap();

        let now_ep = iso_epoch("2026-11-27T10:00:00Z").unwrap();
        // Recent writes: 20m (1200s), 50m (3000s), 80m (4800s) -> median 50m.
        // Ignored: read-only 5m (300s), >14d old write 200m (12000s), 0s write.
        let lines = [
            json!({"event":"finish","run":"r1","mode":"write","state":"delivered","finishedAt":"2026-11-26T10:00:00Z","elapsedSeconds":1200}),
            json!({"event":"finish","run":"r2","mode":"write","state":"delivered","finishedAt":"2026-11-25T10:00:00Z","elapsedSeconds":4800}),
            json!({"event":"finish","run":"r3","mode":"write","state":"failed","finishedAt":"2026-11-24T10:00:00Z","elapsedSeconds":3000}),
            json!({"event":"finish","run":"r4","mode":"read","state":"answered","finishedAt":"2026-11-26T12:00:00Z","elapsedSeconds":300}),
            json!({"event":"finish","run":"r5","mode":"write","state":"delivered","finishedAt":"2026-11-01T10:00:00Z","elapsedSeconds":12000}),
            json!({"event":"finish","run":"r6","mode":"write","state":"delivered","finishedAt":"2026-11-26T11:00:00Z","elapsedSeconds":0}),
        ];
        let body = lines
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(hdir.join("history.jsonl"), body).unwrap();

        assert_eq!(median_write_duration_mins_in(&state, now_ep), Some(50));

        // Empty state returns None.
        let empty = base.join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert_eq!(median_write_duration_mins_in(&empty, now_ep), None);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn quota_shadow_reconcile_recall_and_false_positive_rate() {
        let since_ep = iso_epoch("2026-11-24T00:00:00Z").unwrap();
        let truncated_429 = "API Error: 429 RESOURCE_EXHAUSTED: You have exhausted your capacity on this model. Your quota will reset after 2h15m. Please upgrade your subscription or wait for reset (truncated at 200 chars...";
        assert!(
            is_quota_exhausted_error(truncated_429),
            "truncated 200-char 429 RESOURCE_EXHAUSTED error must match is_quota_exhausted_error"
        );

        let events = vec![
            // Run 1: Gemini started + shadow hold at 10:00 -> real 429 at 10:40 (TP hold + predicted failure)
            json!({"seq":1,"at":"2026-11-26T10:00:00Z","run":"run-1","repo":"/r","kind":"started","payload":{"model":"agy/gemini-4-neon","mode":"write"}}),
            json!({"seq":2,"at":"2026-11-26T10:00:01Z","run":"run-1","repo":"/r","kind":"quota_would_hold","payload":{"pool":"gemini","poolLabel":"Gemini","remainingMinutes":30,"requiredMinutes":90,"activeWrites":1,"cacheAgeSeconds":120,"source":"fill"}}),
            json!({"seq":3,"at":"2026-11-26T10:40:00Z","run":"run-1","repo":"/r","kind":"failed","payload":{"error":truncated_429}}),
            // Run 2: Gemini started + shadow hold at 14:00 -> no subsequent Gemini 429 (FP hold)
            json!({"seq":4,"at":"2026-11-26T14:00:00Z","run":"run-2","repo":"/r","kind":"started","payload":{"model":"agy/gemini-4-neon","mode":"write"}}),
            json!({"seq":5,"at":"2026-11-26T14:00:01Z","run":"run-2","repo":"/r","kind":"quota_would_hold","payload":{"pool":"gemini","poolLabel":"Gemini","remainingMinutes":40,"requiredMinutes":90,"activeWrites":1,"cacheAgeSeconds":200,"source":"dag"}}),
            // Run 3: Claude/GPT 429 at 14:20 -> different pool, must NOT match run-2's Gemini hold; counts as missed failure
            json!({"seq":6,"at":"2026-11-26T14:10:00Z","run":"run-3","repo":"/r","kind":"started","payload":{"model":"agy/claude-4-sonnet","mode":"write"}}),
            json!({"seq":7,"at":"2026-11-26T14:20:00Z","run":"run-3","repo":"/r","kind":"failed","payload":{"error":"RESOURCE_EXHAUSTED (code 429): quota reached, resets in 1h"}}),
            // Run 4: Transient rate limit 429 -> must NOT be counted as real quota failure
            json!({"seq":8,"at":"2026-11-26T14:30:00Z","run":"run-4","repo":"/r","kind":"started","payload":{"model":"agy/gemini-4-neon","mode":"write"}}),
            json!({"seq":9,"at":"2026-11-26T14:35:00Z","run":"run-4","repo":"/r","kind":"failed","payload":{"error":"HTTP 429 Too Many Requests: rate limit exceeded, try again in 30s"}}),
            // Run 5: Another Gemini 429 at 20:00 with no prior hold -> missed failure
            json!({"seq":10,"at":"2026-11-26T19:30:00Z","run":"run-5","repo":"/r","kind":"started","payload":{"model":"agy/gemini-4-neon","mode":"write"}}),
            json!({"seq":11,"at":"2026-11-26T20:00:00Z","run":"run-5","repo":"/r","kind":"failed","payload":{"error":"quota_exhausted"}}),
        ];

        let rep = compute_quota_shadow_report(&events, "72h", since_ep, None);
        // Holds: 2 (run-1 TP, run-2 FP) -> FPR = 1/2 = 0.5
        assert_eq!(rep.total_holds, 2);
        assert_eq!(rep.true_positive_holds, 1);
        assert_eq!(rep.false_positive_holds, 1);
        assert!((rep.false_positive_rate.unwrap() - 0.5).abs() < 1e-9);
        // Real quota failures: 3 (run-1 predicted, run-3 missed, run-5 missed; run-4 transient ignored) -> recall = 1/3
        assert_eq!(rep.total_failures, 3);
        assert_eq!(rep.predicted_failures, 1);
        assert_eq!(rep.missed_failures, 2);
        assert!((rep.recall.unwrap() - (1.0 / 3.0)).abs() < 1e-9);

        let j = rep.to_json();
        assert_eq!(j["holds"], 2);
        assert_eq!(j["quotaFailures"], 3);
        assert_eq!(j["predictedFailures"], 1);
        assert_eq!(j["falsePositiveHolds"], 1);

        let human = rep.format_human();
        assert!(human.contains("召回率：33.3%"));
        assert!(human.contains("误报率：50.0%"));

        // Empty events -> None / null / N/A
        let empty = compute_quota_shadow_report(&[], "72h", since_ep, None);
        assert_eq!(empty.recall, None);
        assert_eq!(empty.false_positive_rate, None);
        assert!(empty.to_json()["recall"].is_null());
        assert!(empty.to_json()["falsePositiveRate"].is_null());
        assert!(empty.format_human().contains("N/A"));
    }

    #[test]
    fn parse_delegate_agy_home_pid_strictly_validates_format() {
        assert_eq!(
            parse_delegate_agy_home_pid("delegate-agy-home-12345-0-0123456789abcdef"),
            Some(12345)
        );
        assert_eq!(parse_delegate_agy_home_pid("delegate-agy-home-0-0-0123456789abcdef"), None);
        assert_eq!(parse_delegate_agy_home_pid("delegate-agy-home-12345-0-0123456789ABCDEF"), None);
        assert_eq!(parse_delegate_agy_home_pid("delegate-agy-home-12345-0-0123"), None);
        assert_eq!(parse_delegate_agy_home_pid("delegate-agy-home-abc-0-0123456789abcdef"), None);
        assert_eq!(parse_delegate_agy_home_pid("other-dir-12345-0-0123456789abcdef"), None);
        assert_eq!(
            parse_delegate_agy_home_pid("delegate-agy-home-12345-0-0123456789abcdef-extra"),
            None
        );
    }

    #[test]
    fn cleanup_orphan_agy_homes_only_removes_dead_delegate_dirs() {
        let base = TempHome::create().unwrap();
        let root = &base.path;

        // 1. Dead-pid delegate dir with `.gemini` -> removed
        let dead_gem = root.join("delegate-agy-home-99999999-0-0000000000000001");
        fs::create_dir_all(dead_gem.join(".gemini")).unwrap();
        fs::write(dead_gem.join(".gemini/google_accounts.json"), "{}").unwrap();

        // 2. Dead-pid empty 0700 delegate dir -> removed
        let dead_empty = root.join("delegate-agy-home-99999999-1-0000000000000002");
        fs::create_dir(&dead_empty).unwrap();
        fs::set_permissions(&dead_empty, fs::Permissions::from_mode(0o700)).unwrap();

        // 3. Live-pid delegate dir (current process pid) -> kept
        let live_dir = root.join(format!(
            "delegate-agy-home-{}-2-0000000000000003",
            std::process::id()
        ));
        fs::create_dir_all(live_dir.join(".gemini")).unwrap();

        // 4. Non-delegate dir with `.gemini` -> kept
        let other_dir = root.join("not-delegate-home");
        fs::create_dir_all(other_dir.join(".gemini")).unwrap();

        // 5. Symlink with delegate-like name pointing to real dir -> symlink & target kept
        let target_dir = root.join("victim-dir");
        fs::create_dir_all(target_dir.join(".gemini")).unwrap();
        let sym = root.join("delegate-agy-home-99999999-3-0000000000000004");
        std::os::unix::fs::symlink(&target_dir, &sym).unwrap();

        // 6. Dead-pid delegate-named dir with arbitrary non-delegate file and no `.gemini` -> kept
        let alien_dir = root.join("delegate-agy-home-99999999-4-0000000000000005");
        fs::create_dir(&alien_dir).unwrap();
        fs::set_permissions(&alien_dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(alien_dir.join("user-notes.txt"), "keep me").unwrap();

        let report = cleanup_orphan_agy_homes_in(root);
        assert_eq!(report.removed, 2);
        assert_eq!(report.failed, 0);
        assert!(!dead_gem.exists());
        assert!(!dead_empty.exists());
        assert!(live_dir.is_dir());
        assert!(other_dir.is_dir());
        assert!(fs::symlink_metadata(&sym).is_ok());
        assert!(target_dir.is_dir());
        assert!(alien_dir.is_dir());
    }

    #[test]
    fn format_delegation_and_live_lines_match_verbatim_spec() {
        let snap = json!({
            "mode": "auto",
            "active": "nova",
            "autoSwitch": {
                "poolSummary": {
                    "gemini": {"selected": "orbit"},
                    "claude_gpt": {"selected": "orbit"}
                }
            },
            "runningAgy": [
                {"pid": 1, "account": "orbit"},
                {"pid": 2, "account": "orbit"},
                {"pid": 3, "account": "alpha"}
            ]
        });
        assert_eq!(
            format_delegation_selection_line(&snap),
            "委派选号：auto · Gemini → orbit · Claude/GPT → orbit（在跑 3 个：orbit ×2，alpha ×1）"
        );
        assert_eq!(
            format_live_slot_line(&snap),
            "本机实盘：nova（只给手动运行的 agy 用，auto 不改它）"
        );
    }

    #[test]
    fn temp_home_core_token_detection_accepts_two_file_minimal_login() {
        let th = TempHome::create().unwrap();
        assert!(!temp_home_has_core_token(&th.path));

        // Non-core file alone is not enough.
        let scope = th.path.join(".config/antigravity/credential-user-scope");
        fs::create_dir_all(scope.parent().unwrap()).unwrap();
        fs::write(&scope, "scope-only").unwrap();
        assert!(!temp_home_has_core_token(&th.path));

        // Empty core token file is not enough.
        let tok = th.path.join(".gemini/antigravity-cli/antigravity-oauth-token");
        fs::create_dir_all(tok.parent().unwrap()).unwrap();
        fs::write(&tok, "").unwrap();
        assert!(!temp_home_has_core_token(&th.path));

        // Non-empty antigravity-cli token satisfies core check (new agy 2-file login).
        fs::write(&tok, r#"{"id_token_email":"danny@example.test","token":{"access_token":"x"}}"#)
            .unwrap();
        assert!(temp_home_has_core_token(&th.path));
    }

    #[test]
    fn agy_login_alias_help_and_unknown_subcommand_error() {
        assert_eq!(command(&["login".into(), "--help".into()]).unwrap(), 0);
        let err = command(&["bogus".into()]).unwrap_err();
        assert!(err.contains("expected: account, login"), "unexpected err: {err}");
    }
}
