//! Local Antigravity CLI (`agy`) multi-account management.
//!
//! Replaces third-party `agy-swap` with a transparent, manual-only, file-locked
//! credential slot manager under `~/.config/delegate/agy-accounts/<id>/`.
//!
//! Design invariants:
//! - No automatic rotation (quota exhaustion never switches accounts automatically).
//! - No external quota API calls (local metadata only).
//! - Secret tokens (`access_token`, `refresh_token`, `id_token`) are never printed to
//!   stdout, stderr, JSON output, or logs.
//! - Slot directories are `0700`, credential files and `accounts.json` are `0600`,
//!   written atomically via `O_CREAT|O_EXCL` mode `0600` temp file + `sync_data` + `rename`.
//! - `use` refuses to switch while any `agy` process sharing the same `HOME` is running,
//!   rather than deferring switches to a hidden wrapper hook (`--next-start`), preventing
//!   split-brain token refreshes and eliminating wrapper complexity.

use crate::common::*;
use crate::{has, parse_simple, value};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const SCHEMA_VERSION: u64 = 1;

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

fn live_email(home_dir: &Path) -> Option<String> {
    read_email_from_json_file(&home_dir.join(".gemini/google_accounts.json"), &["active"])
        .or_else(|| {
            read_email_from_json_file(
                &home_dir.join(".gemini/jetski-standalone-oauth-token"),
                &["id_token_email"],
            )
        })
        .or_else(|| {
            read_email_from_json_file(
                &home_dir.join(".gemini/antigravity-cli/antigravity-oauth-token"),
                &["id_token_email"],
            )
        })
        .or_else(|| {
            read_email_from_json_file(
                &home_dir.join(".gemini/antigravity/antigravity-oauth-token"),
                &["id_token_email"],
            )
        })
}

fn slot_email(dir: &Path) -> Option<String> {
    read_email_from_json_file(&dir.join("google_accounts.json"), &["active"])
        .or_else(|| {
            read_email_from_json_file(&dir.join("jetski-standalone-oauth-token"), &["id_token_email"])
        })
        .or_else(|| {
            read_email_from_json_file(&dir.join("antigravity-cli-oauth-token"), &["id_token_email"])
        })
        .or_else(|| {
            read_email_from_json_file(&dir.join("antigravity-oauth-token"), &["id_token_email"])
        })
}

/// Read only the non-secret `token.expiry` ISO timestamp from local slot files.
fn slot_token_expiry(dir: &Path) -> Option<String> {
    for name in [
        "jetski-standalone-oauth-token",
        "antigravity-cli-oauth-token",
        "antigravity-oauth-token",
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

        // Scope by HOME so isolated temp HOMEs (e.g. `agy account add` or tests) do not collide.
        let mut delegate_run: Option<String> = None;
        if let Ok(raw_env) = fs::read(proc_dir.join("environ")) {
            let mut proc_home: Option<PathBuf> = None;
            for item in raw_env.split(|&b| b == 0) {
                if let Some(val) = item.strip_prefix(b"HOME=") {
                    proc_home = Some(PathBuf::from(String::from_utf8_lossy(val).into_owned()));
                } else if let Some(val) = item.strip_prefix(b"DELEGATE_RUN_DIR=") {
                    let p = PathBuf::from(String::from_utf8_lossy(val).into_owned());
                    if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                        if !name.is_empty() {
                            delegate_run = Some(name.to_string());
                        }
                    }
                }
            }
            if let Some(ph) = proc_home {
                let ph_canon = fs::canonicalize(&ph).unwrap_or(ph);
                if ph_canon != target_canon {
                    continue;
                }
            }
        }

        found.push(RunningAgy {
            pid,
            delegate_run,
            cmdline: clip(&argv.join(" "), 120),
        });
    }

    found.sort_by_key(|p| p.pid);
    found
}

/// Strictly lock-free, zero-subprocess snapshot of local `agy` accounts for
/// `delegate snapshot --json`, `delegate agy account list/status`, and `delegate-tui`.
/// Never reads or exposes secret tokens.
pub fn snapshot_json() -> Value {
    snapshot_json_opts(true)
}

/// Fast snapshot for `delegate snapshot --json`: skips the `/proc` scan when no local
/// `agy-accounts/accounts.json` exists yet so snapshot stays well under 20 ms.
pub fn snapshot_json_fast() -> Value {
    let has_accounts = accounts_meta_path(&accounts_root()).is_file();
    snapshot_json_opts(has_accounts)
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
            items.push(json!({
                "id": id,
                "email": email,
                "label": s(entry, "label"),
                "active": is_active,
                "addedAt": s(entry, "addedAt"),
                "lastUsedAt": s(entry, "lastUsedAt"),
                "lastSyncedAt": s(entry, "lastSyncedAt"),
                "tokenExpiresAt": token_expiry,
                "files": files,
                "fileCount": files.len(),
                "source": s(entry, "source"),
            }));
        }
    }

    let live_drifted = !live.is_empty()
        && (active_id.is_none() || (!active_email.is_empty() && active_email != live));

    let running_json: Vec<Value> = running
        .iter()
        .map(|p| {
            json!({
                "pid": p.pid,
                "run": p.delegate_run,
                "cmdline": p.cmdline,
            })
        })
        .collect();

    json!({
        "root": root.to_string_lossy(),
        "active": active_id,
        "liveEmail": live,
        "liveDrifted": live_drifted,
        "runningAgy": running_json,
        "accounts": items,
    })
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

/// Save back any present live credential files from `$HOME` into `slot_dir`.
/// Captures tokens refreshed by `agy` since the last switch.
fn save_live_to_slot(home_dir: &Path, dst_slot: &Path) -> Res<usize> {
    ensure_dir_mode(dst_slot, 0o700)?;
    let mut copied = 0usize;
    for cf in CRED_FILES {
        let src = home_dir.join(cf.home_rel);
        if src.is_file() {
            let bytes = fs::read(&src).map_err(|e| format!("read {}: {e}", src.display()))?;
            if !bytes.is_empty() {
                write_secret_0600(&dst_slot.join(cf.slot_name), &bytes, true)?;
                copied += 1;
            }
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
    match &p.delegate_run {
        Some(run) => format!("  pid={} (delegate run: {}) {}", p.pid, run, p.cmdline),
        None => format!("  pid={} {}", p.pid, p.cmdline),
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
        let dir = base.join(format!(
            "delegate-agy-add-{}-{:016x}",
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

fn cmd_list(args: &[String]) -> Res<i32> {
    let (pos, flags, _) = parse_simple(args, &["--json"], &[])?;
    if !pos.is_empty() {
        return Err(format!("unrecognized arguments: {}", pos.join(" ")));
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
    println!(
        "agy accounts ({}, active: {}, running agy: {}):",
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
        println!("  {mark} {id:<16} {email:<30} last_used={last_str} files={files}{label_part}");
    }
    if b(&snap, "liveDrifted") {
        println!(
            "note: live ~/.gemini account ({}) differs from active slot ({})",
            s(&snap, "liveEmail"),
            if active.is_empty() { "none" } else { active }
        );
    }
    println!("Next: delegate agy account use <id> | delegate agy account status");
    Ok(0)
}

fn cmd_status(args: &[String]) -> Res<i32> {
    let (pos, flags, _) = parse_simple(args, &["--json"], &[])?;
    if !pos.is_empty() {
        return Err(format!("unrecognized arguments: {}", pos.join(" ")));
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
            println!("local token expiry: {exp} (quota not polled externally)");
        }
        println!("stored files: {}", cur["fileCount"].as_u64().unwrap_or(0));
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
    println!("Next: delegate agy account list | delegate agy account use <id>");
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
    let (pos, flags, _) = parse_simple(args, &["--force", "--json"], &[])?;
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

    // Refuse switching while any agy process sharing this HOME is running.
    let running = detect_running_agy_for_home(&home_dir);
    if !running.is_empty() {
        if json_out {
            let procs: Vec<Value> = running
                .iter()
                .map(|p| {
                    json!({
                        "pid": p.pid,
                        "run": p.delegate_run,
                        "cmdline": p.cmdline,
                    })
                })
                .collect();
            println!(
                "{}",
                json!({
                    "ok": false,
                    "error": "running_agy_processes",
                    "message": format!(
                        "cannot switch agy account while {} agy process(es) are running",
                        running.len()
                    ),
                    "runningAgy": procs,
                })
            );
        } else {
            eprintln!(
                "delegate agy account use: refused — {} agy process(es) currently running in this HOME:",
                running.len()
            );
            for p in &running {
                eprintln!("{}", format_running_proc_line(p));
            }
            eprintln!("Wait for running agy tasks to finish (or stop them) before switching.");
        }
        return Ok(1);
    }

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
        saved_back_files = save_live_to_slot(&home_dir, &owner_slot)?;
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
    }
    Ok(0)
}

fn cmd_remove(args: &[String]) -> Res<i32> {
    let (pos, flags, _) = parse_simple(args, &["--force", "--json"], &[])?;
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
    let (pos, flags, kv) = parse_simple(
        args,
        &["--dry-run", "--overwrite", "--json"],
        &["--source"],
    )?;
    if !pos.is_empty() {
        return Err(format!("unrecognized arguments: {}", pos.join(" ")));
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
                    let _ = save_live_to_slot(&home_dir, &sdir)?;
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
    let (pos, flags, kv) = parse_simple(
        args,
        &["--dry-run", "--force", "--json"],
        &["--mode", "--entry", "--real-bin"],
    )?;
    if !pos.is_empty() {
        return Err(format!("unrecognized arguments: {}", pos.join(" ")));
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

fn print_account_help() {
    println!(
        "Usage: delegate agy account <subcommand> [options]\n\n\
         Manage local Antigravity CLI (agy) accounts manually (~/.config/delegate/agy-accounts/).\n\
         No automatic rotation and no external quota API calls.\n\n\
         Subcommands:\n\
         \x20 list [--json]                        List saved account slots and active marker\n\
         \x20 status [--json]                      Show active account, live email, last used, running agy\n\
         \x20 add [<id>] [--label TEXT] [--overwrite] [--agy-bin PATH] [-- <agy-args>...]\n\
         \x20                                      Run interactive agy login in a temp HOME and save slot\n\
         \x20 use <id> [--force] [--json]          Save back refreshed tokens and switch (refuses if agy running)\n\
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
}
