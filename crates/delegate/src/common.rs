use serde_json::Value;
use std::env;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static ATOMIC_NONCE: AtomicU64 = AtomicU64::new(1);

pub type Res<T> = Result<T, String>;
pub fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}
pub const GUARDS: [&str; 7] = [
    "DELEGATE_AGENT",
    "DELEGATE_PARENT_RUN",
    "PI_DELEGATE_ACTIVE",
    "DELEGATE_RUN_DIR",
    "DELEGATE_LANE_HELD",
    "PI_DELEGATE_AGENT",
    "PI_DELEGATE_PARENT_RUN",
];
/// Parent-controller session and Antigravity runtime variables stripped when launching
/// colleagues so nested `agy` / Claude / Codex / Pi invocations never cross-talk with
/// the parent controller's language server or session identity.
pub const CONTROLLER_ENV: [&str; 10] = [
    "ANTIGRAVITY_LS_ADDRESS",
    "ANTIGRAVITY_CSRF_TOKEN",
    "ANTIGRAVITY_CONVERSATION_ID",
    "ANTIGRAVITY_TRAJECTORY_ID",
    "ANTIGRAVITY_SOURCE_METADATA",
    "ANTIGRAVITY_AGENT",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_THREAD_ID",
    "PI_SESSION_ID",
    "DELEGATE_CALLER",
];
/// Where the caller's session id comes from: an explicit id from any harness,
/// then Antigravity (prioritized when `ANTIGRAVITY_AGENT` is active), Claude Code,
/// Codex and Pi session variables.
pub const CALLER_ENV: [&str; 5] = [
    "DELEGATE_CALLER",
    "ANTIGRAVITY_CONVERSATION_ID",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_THREAD_ID",
    "PI_SESSION_ID",
];
fn env_nonempty(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}
pub(crate) fn is_antigravity_agent_with(get: impl Fn(&str) -> Option<String>) -> bool {
    get("ANTIGRAVITY_AGENT")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .is_some_and(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
}
pub fn caller_source() -> Option<(String, &'static str)> {
    caller_source_with(env_nonempty)
}
fn caller_source_with(get: impl Fn(&str) -> Option<String>) -> Option<(String, &'static str)> {
    let order: [&str; 5] = if is_antigravity_agent_with(&get) {
        CALLER_ENV
    } else {
        [
            "DELEGATE_CALLER",
            "CLAUDE_CODE_SESSION_ID",
            "CODEX_THREAD_ID",
            "PI_SESSION_ID",
            "ANTIGRAVITY_CONVERSATION_ID",
        ]
    };
    order.into_iter().find_map(|name| {
        get(name)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(|value| (value, name))
    })
}
pub fn caller() -> Option<String> {
    caller_source().map(|(id, _)| id)
}

pub(crate) fn proc_stat_state_ppid_ticks_tty(pid: u32) -> Option<(char, u32, u64, i32)> {
    let txt = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = txt.rfind(')')?;
    let fields: Vec<&str> = txt.get(rparen + 2..)?.split_whitespace().collect();
    // fields[0]=state, fields[1]=ppid, fields[4]=tty_nr (field 7), fields[19]=starttime (field 22).
    let state = fields.first()?.chars().next()?;
    let ppid = fields.get(1)?.parse::<u32>().ok()?;
    let tty_nr = fields.get(4)?.parse::<i32>().unwrap_or(0);
    let ticks = fields.get(19)?.parse::<u64>().ok()?;
    Some((state, ppid, ticks, tty_nr))
}

/// Write `<run>/stall-warning.json` unless an active warning of a different `kind` is present.
pub fn write_stall_warning_for_kind(run: &Path, kind: &str, warning: &Value) -> bool {
    let path = run.join("stall-warning.json");
    if path.is_file() {
        let existing = json(&path);
        let existing_kind = s(&existing, "kind");
        if !existing_kind.is_empty() && existing_kind != kind {
            return false;
        }
    }
    write_json(path, warning).is_ok()
}

/// Remove `<run>/stall-warning.json` only if its `kind` matches `kind`.
pub fn clear_stall_warning_for_kind(run: &Path, kind: &str) {
    let path = run.join("stall-warning.json");
    if path.is_file() {
        let existing = json(&path);
        if s(&existing, "kind") == kind {
            let _ = fs::remove_file(&path);
        }
    }
}

pub fn pid_alive_with_ticks(pid: u32, expected_ticks: Option<u64>) -> bool {
    if !pid_alive(pid as i32) {
        return false;
    }
    if let Some((state, _ppid, actual_ticks, _tty_nr)) = proc_stat_state_ppid_ticks_tty(pid) {
        if state == 'Z' {
            return false;
        }
        if let Some(exp) = expected_ticks.filter(|&t| t > 0) {
            if actual_ticks != exp {
                return false;
            }
        }
    }
    true
}

fn pid_is_delegate_colleague(pid: u32) -> bool {
    let Ok(raw) = fs::read(format!("/proc/{pid}/environ")) else {
        return false;
    };
    raw.split(|&b| b == 0).any(|entry| {
        entry
            .strip_prefix(b"PI_DELEGATE_ACTIVE=")
            .is_some_and(|v| !v.is_empty() && v != b"0")
    })
}

fn is_harness_cmdline(pid: u32, source: Option<&str>) -> bool {
    let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let tokens: Vec<String> = raw
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    let Some(first) = tokens.first() else {
        return false;
    };
    let base = Path::new(first)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(first);
    let matches_direct = match source {
        Some("CLAUDE_CODE_SESSION_ID") => matches!(base, "claude"),
        Some("ANTIGRAVITY_CONVERSATION_ID") => matches!(base, "agy" | "antigravity"),
        Some("CODEX_THREAD_ID") => matches!(base, "codex"),
        Some("PI_SESSION_ID") => matches!(base, "pi" | "rpi"),
        _ => matches!(
            base,
            "claude" | "codex" | "pi" | "rpi" | "agy" | "antigravity"
        ),
    };
    if matches_direct {
        return true;
    }
    if matches!(base, "node" | "bun" | "deno") {
        return tokens.iter().skip(1).take(3).any(|arg| {
            let lower = arg.to_ascii_lowercase();
            match source {
                Some("CLAUDE_CODE_SESSION_ID") => lower.contains("claude"),
                Some("ANTIGRAVITY_CONVERSATION_ID") => lower.contains("antigravity"),
                Some("CODEX_THREAD_ID") => lower.contains("codex"),
                Some("PI_SESSION_ID") => {
                    lower.ends_with("/pi") || lower.contains("pi-coding-agent")
                }
                _ => {
                    lower.contains("claude")
                        || lower.contains("codex")
                        || lower.ends_with("/pi")
                        || lower.contains("pi-coding-agent")
                        || lower.contains("antigravity")
                }
            }
        });
    }
    false
}

pub(crate) fn find_session_harness_pid(source: Option<&str>) -> Option<(u32, u64, i32)> {
    if env::var("DELEGATE_SESSION_HARNESS_PID").ok().as_deref() == Some("0") {
        return None;
    }
    let mut pid = std::os::unix::process::parent_id();
    for _ in 0..8 {
        if pid <= 1 {
            break;
        }
        let (_state, ppid, ticks, tty_nr) = proc_stat_state_ppid_ticks_tty(pid)?;
        if is_harness_cmdline(pid, source) && !pid_is_delegate_colleague(pid) {
            return Some((pid, ticks, tty_nr));
        }
        if ppid == pid {
            break;
        }
        pid = ppid;
    }
    None
}

pub(crate) fn current_terminal_id(harness_tty_nr: i32) -> Option<String> {
    for (env_key, prefix) in [
        ("TMUX_PANE", "tmux"),
        ("TERM_SESSION_ID", "term"),
        ("WEZTERM_PANE", "wezterm"),
        ("KITTY_WINDOW_ID", "kitty"),
        ("WINDOWID", "win"),
    ] {
        if let Ok(val) = env::var(env_key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(format!("{prefix}:{trimmed}"));
            }
        }
    }
    if harness_tty_nr != 0 {
        return Some(format!("tty:{harness_tty_nr}"));
    }
    if let Some((_, _, _, self_tty)) = proc_stat_state_ppid_ticks_tty(std::process::id()) {
        if self_tty != 0 {
            return Some(format!("tty:{self_tty}"));
        }
    }
    None
}

fn current_cwd_canonical() -> Option<String> {
    let cwd = env::current_dir().ok()?;
    let canon = fs::canonicalize(&cwd).unwrap_or(cwd);
    Some(canon.to_string_lossy().into_owned())
}

/// Lock-free best-effort recording of the caller's activity timestamp, terminal, cwd,
/// and enclosing harness PID (when detected) into `<state>/sessions.json` so the Stop
/// hook can tell whether a session is still live using delegate's own timestamps and
/// suggest `delegate adopt <run>` after `/clear` in the same terminal or working directory.
pub fn touch_caller_session() {
    let Some((id, source)) = caller_source() else {
        return;
    };
    let state = state_dir();
    if fs::create_dir_all(&state).is_err() {
        return;
    }
    let path = state.join("sessions.json");
    let mut doc = json(&path);
    if !doc.is_object() {
        doc = serde_json::json!({});
    }
    let now_epoch = epoch() as i64;
    let cutoff = now_epoch - 14 * 86_400;
    let harness = find_session_harness_pid(Some(source));
    let tty = current_terminal_id(harness.map_or(0, |(_, _, tty_nr)| tty_nr));
    let cwd = current_cwd_canonical();
    if let Some(map) = doc.as_object_mut() {
        let prev = map.get(&id).cloned().unwrap_or(Value::Null);
        map.retain(|k, v| {
            k == &id
                || iso_epoch(s(v, "lastSeenAt")).is_none_or(|ts| ts >= cutoff)
        });
        let mut rec = serde_json::json!({
            "lastSeenAt": iso(),
        });
        if let Some((pid, start_ticks, _tty_nr)) = harness {
            rec["pid"] = serde_json::json!(pid);
            rec["startTicks"] = serde_json::json!(start_ticks);
        } else if let Some(prev_pid) = prev["pid"]
            .as_u64()
            .filter(|&p| p > 0 && p <= u32::MAX as u64)
        {
            let prev_ticks = prev["startTicks"].as_u64();
            if pid_alive_with_ticks(prev_pid as u32, prev_ticks) {
                rec["pid"] = serde_json::json!(prev_pid);
                if let Some(ticks) = prev_ticks {
                    rec["startTicks"] = serde_json::json!(ticks);
                }
            }
        }
        if let Some(t) = tty.or_else(|| {
            let old = s(&prev, "tty");
            (!old.is_empty()).then(|| old.to_string())
        }) {
            rec["tty"] = serde_json::json!(t);
        }
        if let Some(c) = cwd {
            rec["cwd"] = serde_json::json!(c);
        }
        map.insert(id, rec);
    }
    let _ = write_json(&path, &doc);
}
pub fn setting(name: &str, default: &str) -> String {
    env::var(format!("DELEGATE_{name}"))
        .ok()
        .filter(|x| !x.is_empty())
        .or_else(|| {
            env::var(format!("PI_DELEGATE_{name}"))
                .ok()
                .filter(|x| !x.is_empty())
        })
        .unwrap_or_else(|| default.to_string())
}
pub fn number(name: &str, default: u64) -> Res<u64> {
    let s = setting(name, &default.to_string());
    if !s.bytes().all(|b| b.is_ascii_digit()) || s.is_empty() {
        return Err(format!(
            "DELEGATE_{name} must be a non-negative integer (0 = unlimited), got {s:?}"
        ));
    }
    s.parse::<u64>().map_err(|e| e.to_string())
}
pub fn seconds(s: &str) -> Res<f64> {
    let (digits, factor) = match s.as_bytes().last() {
        Some(b's') => (&s[..s.len() - 1], 1.0),
        Some(b'm') => (&s[..s.len() - 1], 60.0),
        Some(b'h') => (&s[..s.len() - 1], 3600.0),
        Some(b'd') => (&s[..s.len() - 1], 86400.0),
        _ => (s, 1.0),
    };
    if digits.is_empty()
        || digits.matches('.').count() > 1
        || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'.')
    {
        return Err(format!(
            "invalid positive duration: {s} (e.g. 90, 90s, 15m, 1h)"
        ));
    }
    let n = digits.parse::<f64>().unwrap_or(0.0) * factor;
    if n <= 0.0 || !n.is_finite() {
        return Err(format!(
            "invalid positive duration: {s} (e.g. 90, 90s, 15m, 1h)"
        ));
    }
    Ok(n)
}
pub fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
pub fn epoch() -> f64 {
    now_ns() as f64 / 1e9
}
pub fn format_epoch(ts: i64, local: bool, pattern: &str) -> String {
    let t = ts as libc::time_t;
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe {
        if local {
            libc::localtime_r(&t, &mut tm);
        } else {
            libc::gmtime_r(&t, &mut tm);
        }
    }
    let p = CString::new(pattern).unwrap_or_default();
    let mut buf = [0u8; 64];
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), p.as_ptr(), &tm) };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}
pub fn format_time(local: bool, pattern: &str) -> String {
    let t = unsafe { libc::time(std::ptr::null_mut()) };
    format_epoch(t as i64, local, pattern)
}
pub fn iso() -> String {
    format_time(false, "%Y-%m-%dT%H:%M:%SZ")
}
pub fn stamp() -> String {
    format_time(true, "%Y%m%d-%H%M%S")
}
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in text.trim().chars() {
        if ch.is_alphanumeric() || ch == '_' {
            out.push(ch);
            dash = false;
        } else if !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
        if out.chars().count() >= 32 {
            break;
        }
    }
    out.trim_matches('-').to_string()
}
pub fn iso_epoch(value: &str) -> Option<i64> {
    let field = |start: usize, end: usize| value.get(start..end)?.parse::<i32>().ok();
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    tm.tm_year = field(0, 4)? - 1900;
    tm.tm_mon = field(5, 7)? - 1;
    tm.tm_mday = field(8, 10)?;
    tm.tm_hour = field(11, 13)?;
    tm.tm_min = field(14, 16)?;
    tm.tm_sec = field(17, 19)?;
    if value.get(19..20)? != "Z" {
        return None;
    }
    Some(unsafe { libc::timegm(&mut tm) })
}
pub fn script() -> PathBuf {
    if let Some(arg) = env::args_os().next() {
        let path = PathBuf::from(arg);
        if path.is_absolute() {
            return path;
        }
        if path.components().count() > 1 {
            if let Ok(path) = std::path::absolute(path) {
                return path;
            }
        } else if let Some(path) = env::var_os("PATH").and_then(|paths| {
            env::split_paths(&paths)
                .map(|dir| dir.join(&path))
                .find(|candidate| candidate.is_file())
        }) {
            if let Ok(path) = std::path::absolute(path) {
                return path;
            }
        }
    }
    env::current_exe().unwrap_or_else(|_| PathBuf::from("delegate"))
}
pub fn home() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}
pub fn state_dir() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
        .join("delegate")
}
pub fn cache_dir() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".cache"))
        .join("delegate")
}
pub fn runs_root() -> PathBuf {
    if let Some(s) = env::var_os("DELEGATE_RUNS").or_else(|| env::var_os("PI_DELEGATE_RUNS")) {
        if !s.is_empty() {
            return PathBuf::from(s);
        }
    }
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    git_top_fast(&cwd).unwrap_or(cwd).join(".local/run/delegate")
}
pub fn legacy_runs_root() -> Option<PathBuf> {
    if !setting("RUNS", "").is_empty() {
        return None;
    }
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    Some(git_top_fast(&cwd).unwrap_or(cwd).join(".local/run/pi"))
}
pub fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).unwrap_or_default()
}
pub fn json(path: impl AsRef<Path>) -> Value {
    serde_json::from_str(&read(path)).unwrap_or(Value::Null)
}
/// Generate a collision-free sibling temp path: `.<name>.<pid>.<nonce>.tmp`.
pub fn atomic_tmp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let pid = std::process::id();
    let seq = ATOMIC_NONCE.fetch_add(1, AtomicOrdering::Relaxed);
    let nonce = seq.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (now_ns() as u64);
    path.with_file_name(format!(".{name}.{pid}.{nonce:016x}.tmp"))
}

/// Atomic file write: unique PID+nonce sibling temp file -> `write_all` -> `sync_data` -> `rename`.
pub fn atomic_write(path: impl AsRef<Path>, data: impl AsRef<[u8]>) -> Res<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    let tmp = atomic_tmp_path(path);
    let res = (|| -> io::Result<()> {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        f.write_all(data.as_ref())?;
        f.sync_data()?;
        Ok(())
    })();
    if let Err(err) = res {
        let _ = fs::remove_file(&tmp);
        return Err(err.to_string());
    }
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err.to_string());
    }
    Ok(())
}

pub fn write(path: impl AsRef<Path>, data: impl AsRef<[u8]>) -> Res<()> {
    atomic_write(path, data)
}
pub fn write_json(path: impl AsRef<Path>, val: &Value) -> Res<()> {
    atomic_write(path, format!("{val}\n"))
}
pub fn append(path: impl AsRef<Path>, data: &str) -> Res<()> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(data.as_bytes()))
        .map_err(|e| e.to_string())
}
pub fn append_synced(path: impl AsRef<Path>, data: &str) -> Res<()> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| {
            f.write_all(data.as_bytes())?;
            f.sync_data()
        })
        .map_err(|e| e.to_string())
}
pub fn touch(path: impl AsRef<Path>) {
    let _ = OpenOptions::new().create(true).append(true).open(path);
}
pub fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
pub fn n(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}
pub fn b(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}
pub fn clip(s: &str, max: usize) -> String {
    let mut c = s.chars();
    let x: String = c.by_ref().take(max).collect();
    if c.next().is_some() {
        format!("{x}...")
    } else {
        x
    }
}
pub fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}
pub fn repository_relative(path: &str, top: &str) -> String {
    let path = Path::new(path);
    let top = Path::new(top);
    if !path.is_absolute() || !top.is_absolute() {
        return path.to_string_lossy().into_owned();
    }
    let path_parts = path.components().collect::<Vec<_>>();
    let top_parts = top.components().collect::<Vec<_>>();
    let shared = path_parts
        .iter()
        .zip(&top_parts)
        .take_while(|(a, b)| a == b)
        .count();
    let mut relative = PathBuf::new();
    for _ in shared..top_parts.len() {
        relative.push("..");
    }
    for component in &path_parts[shared..] {
        relative.push(component.as_os_str());
    }
    relative.to_string_lossy().into_owned()
}
pub fn lock(path: &Path, exclusive: bool, nonblock: bool) -> io::Result<File> {
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    let op = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    } | if nonblock { libc::LOCK_NB } else { 0 };
    if unsafe { libc::flock(f.as_raw_fd(), op) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(f)
}
pub fn locked_file(path: &Path, content: &str) -> Res<File> {
    let tmp = atomic_tmp_path(path);
    let mut f = lock(&tmp, true, false).map_err(|e| e.to_string())?;
    let res = (|| -> io::Result<()> {
        f.set_len(0)?;
        f.write_all(content.as_bytes())?;
        f.sync_data()?;
        Ok(())
    })();
    if let Err(err) = res {
        let _ = fs::remove_file(&tmp);
        return Err(err.to_string());
    }
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err.to_string());
    }
    Ok(f)
}
pub fn cmd_output(mut c: Command) -> Res<Output> {
    crate::locks::assert_not_holding_state_lock("subprocess");
    c.output().map_err(|e| e.to_string())
}
/// All Git invocations, including source probes, must avoid optional index writes.
pub fn git_command() -> Command {
    crate::locks::assert_not_holding_state_lock("git");
    let mut command = Command::new("git");
    command.env("GIT_OPTIONAL_LOCKS", "0");
    command
}
pub fn git(top: &Path, args: &[&str]) -> Res<Vec<u8>> {
    let o = cmd_output({
        let mut c = git_command();
        c.arg("-C").arg(top).args(args);
        c
    })?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).trim().to_string());
    }
    Ok(o.stdout)
}
pub fn git_text(top: &Path, args: &[&str]) -> Res<String> {
    Ok(String::from_utf8_lossy(&git(top, args)?)
        .trim_end()
        .to_string())
}
pub fn git_top(path: &Path) -> Option<PathBuf> {
    if env::var_os("GIT_DIR").is_none() && env::var_os("GIT_WORK_TREE").is_none() {
        return git_top_fast(path);
    }
    if crate::locks::holding_state_lock() {
        return git_top_fast(path);
    }
    git_text(path, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(PathBuf::from)
}
/// Pure-filesystem git toplevel lookup (<0.05ms, zero subprocesses, safe anywhere).
pub fn git_top_fast(path: &Path) -> Option<PathBuf> {
    let start = if path.is_file() {
        path.parent()?
    } else {
        path
    };
    let canonical = fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    if canonical
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new(".git"))
    {
        return None;
    }
    let mut cur = canonical.as_path();
    loop {
        if cur.join(".git").exists() {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}
/// Pure-filesystem git common directory lookup (<0.05ms, zero git subprocesses).
pub fn git_common_dir_fast(path: &Path) -> Option<PathBuf> {
    let top = git_top_fast(path)?;
    let dot_git = top.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    if dot_git.is_file() {
        let text = fs::read_to_string(&dot_git).ok()?;
        let raw = text.lines().next()?.trim().strip_prefix("gitdir:")?.trim();
        let git_dir = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            top.join(raw)
        };
        if let Ok(common_raw) = fs::read_to_string(git_dir.join("commondir")) {
            let trimmed = common_raw.trim();
            if !trimmed.is_empty() {
                let p = PathBuf::from(trimmed);
                let resolved = if p.is_absolute() { p } else { git_dir.join(p) };
                return Some(fs::canonicalize(&resolved).unwrap_or(resolved));
            }
        }
        return Some(fs::canonicalize(&git_dir).unwrap_or(git_dir));
    }
    None
}
/// Pure-filesystem resolution of `HEAD` commit SHA (<0.1ms, zero git subprocesses).
pub fn read_git_head(repo: &Path) -> Option<String> {
    if repo.as_os_str().is_empty() {
        return None;
    }
    let top = git_top_fast(repo)?;
    let dot_git = top.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else if dot_git.is_file() {
        let text = fs::read_to_string(&dot_git).ok()?;
        let raw = text
            .lines()
            .next()?
            .trim()
            .strip_prefix("gitdir:")?
            .trim();
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else {
            top.join(p)
        }
    } else {
        return None;
    };
    let common_dir = if let Ok(raw) = fs::read_to_string(git_dir.join("commondir")) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            git_dir.clone()
        } else {
            let p = PathBuf::from(trimmed);
            if p.is_absolute() {
                p
            } else {
                git_dir.join(p)
            }
        }
    } else {
        git_dir.clone()
    };
    let head_raw = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head_raw.trim();
    if let Some(ref_name) = head.strip_prefix("ref:") {
        let ref_name = ref_name.trim();
        for base in [&git_dir, &common_dir] {
            if let Ok(sha) = fs::read_to_string(base.join(ref_name)) {
                let sha = sha.trim();
                if sha.len() >= 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Some(sha.to_string());
                }
            }
        }
        if let Ok(packed) = fs::read_to_string(common_dir.join("packed-refs")) {
            for line in packed.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                    continue;
                }
                if let Some((sha, name)) = line.split_once(' ') {
                    if name.trim() == ref_name
                        && sha.len() >= 40
                        && sha.bytes().all(|b| b.is_ascii_hexdigit())
                    {
                        return Some(sha.to_string());
                    }
                }
            }
        }
        None
    } else if head.len() >= 40 && head.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(head.to_string())
    } else {
        None
    }
}
pub fn clean_env(c: &mut Command, extra: &Value) {
    for k in GUARDS {
        c.env_remove(k);
    }
    for k in CONTROLLER_ENV {
        c.env_remove(k);
    }
    // Existing host env and config env.CARGO_BUILD_JOBS no longer take effect;
    // delegate injects the derived (or explicit heavyJobs) CARGO_BUILD_JOBS instead.
    c.env_remove("CARGO_BUILD_JOBS");
    if let Some(obj) = extra.as_object() {
        for (k, v) in obj {
            if k == "CARGO_BUILD_JOBS" {
                continue;
            }
            if let Some(v) = v.as_str() {
                c.env(k, v);
            }
        }
    }
}
pub fn group(c: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        c.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
pub fn killpg(pid: i32, sig: i32) {
    unsafe {
        libc::killpg(pid, sig);
    }
}
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(pid, 0) == 0 }
}
pub fn group_members(pgid: i32) -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_string_lossy().parse::<i32>().ok() else {
            continue;
        };
        let stat = read(format!("/proc/{pid}/stat"));
        let Some((_, tail)) = stat.rsplit_once(") ") else {
            continue;
        };
        let f: Vec<_> = tail.split_whitespace().collect();
        if f.len() > 2 && f[0] != "Z" && f[2] == pgid.to_string() {
            return true;
        }
    }
    false
}
pub fn end_group(pid: i32, grace: f64) {
    for sig in [libc::SIGTERM, libc::SIGKILL] {
        crate::cleanup::signal_group(pid, sig);
        let start = std::time::Instant::now();
        while crate::cleanup::group_has_cleanup_members(pid)
            && start.elapsed().as_secs_f64() < grace
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        if !crate::cleanup::group_has_cleanup_members(pid) {
            break;
        }
    }
}
pub fn kill_group(pid: i32, grace: f64) {
    crate::cleanup::signal_group(pid, libc::SIGTERM);
    let start = std::time::Instant::now();
    while crate::cleanup::group_has_cleanup_members(pid) && start.elapsed().as_secs_f64() < grace {
        std::thread::sleep(Duration::from_millis(100));
    }
    if crate::cleanup::group_has_cleanup_members(pid) {
        crate::cleanup::signal_group(pid, libc::SIGKILL);
    }
}
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        s.into()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}
fn last_non_empty_line(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return String::new();
    };
    let start = len.saturating_sub(8192);
    if start > 0 && f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| clip(line, 160))
        .unwrap_or_default()
}

pub fn run_shell(
    command: (&str, &str),
    cwd: &Path,
    run: &Path,
    extra: &Value,
    timeout: Option<f64>,
    log: &mut File,
    holder: Option<&std::sync::atomic::AtomicI32>,
) -> Res<(i32, bool)> {
    run_shell_with(command, cwd, run, extra, timeout, log, holder, false)
}
#[allow(clippy::too_many_arguments)]
pub fn run_shell_with(
    command: (&str, &str),
    cwd: &Path,
    run: &Path,
    extra: &Value,
    timeout: Option<f64>,
    log: &mut File,
    holder: Option<&std::sync::atomic::AtomicI32>,
    force_local: bool,
) -> Res<(i32, bool)> {
    run_shell_cancellable(command, cwd, run, extra, timeout, log, holder, force_local, None)
}
#[allow(clippy::too_many_arguments)]
pub fn run_shell_isolated(
    command: (&str, &str),
    cwd: &Path,
    run: &Path,
    cleanup_dir: &Path,
    extra: &Value,
    timeout: Option<f64>,
    log: &mut File,
    holder: Option<&std::sync::atomic::AtomicI32>,
    force_local: bool,
) -> Res<(i32, bool)> {
    run_shell_inner(
        command,
        cwd,
        run,
        cleanup_dir,
        extra,
        timeout,
        log,
        holder,
        force_local,
        None,
    )
}
#[allow(clippy::too_many_arguments)]
pub fn run_shell_cancellable(
    command: (&str, &str),
    cwd: &Path,
    run: &Path,
    extra: &Value,
    timeout: Option<f64>,
    log: &mut File,
    holder: Option<&std::sync::atomic::AtomicI32>,
    force_local: bool,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Res<(i32, bool)> {
    run_shell_inner(
        command,
        cwd,
        run,
        run,
        extra,
        timeout,
        log,
        holder,
        force_local,
        cancel,
    )
}
#[allow(clippy::too_many_arguments)]
fn run_shell_inner(
    command: (&str, &str),
    cwd: &Path,
    run: &Path,
    cleanup_dir: &Path,
    extra: &Value,
    timeout: Option<f64>,
    log: &mut File,
    holder: Option<&std::sync::atomic::AtomicI32>,
    force_local: bool,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Res<(i32, bool)> {
    if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst)) {
        return Ok((130, false));
    }
    let (command, role) = command;
    let mut c = crate::cleanup::scoped_command(cleanup_dir, role, "sh", &[]);
    c.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(Stdio::from(log.try_clone().map_err(|e| e.to_string())?));
    clean_env(&mut c, extra);
    if let Some(vars) = crate::lane::current_slot_env() {
        c.env_remove("MAKEFLAGS").env_remove("CARGO_MAKEFLAGS");
        for (k, v) in vars {
            c.env(k, v);
        }
    } else {
        c.env(
            "CARGO_BUILD_JOBS",
            crate::lane::local_heavy_jobs(Some(run)).to_string(),
        );
    }
    let meta = json(run.join("meta.json"));
    if let Some(path) = crate::deny::shell_path_with(&meta, run, force_local)? {
        c.env("PATH", path);
    }
    let remote_status = cleanup_dir.join(format!("{role}.remote.json"));
    let _ = fs::remove_file(&remote_status);
    c.env("DELEGATE_SHELL_STATUS", &remote_status);
    if !force_local && matches!(role, "accept" | "verify") {
        c.env("DELEGATE_REMOTE_FALLBACK_ON_MISSING", "1");
    } else {
        c.env_remove("DELEGATE_REMOTE_FALLBACK_ON_MISSING");
    }
    let held_val = if force_local {
        "1"
    } else {
        crate::lane::current_lane_held_env()
    };
    c.env("DELEGATE_LANE_HELD", held_val)
        .env("DELEGATE_RUN_DIR", cleanup_dir);
    group(&mut c);
    let started = std::time::Instant::now();
    let queued_before = crate::lane::queued_seconds(run);
    let stall_kind = if matches!(role, "accept" | "verify" | "premerge") {
        Some("accept")
    } else if role == "generate" || role.starts_with("generate") {
        Some("generate")
    } else {
        None
    };
    let stall_limit = stall_kind.and_then(|k| crate::agents::stall_seconds(&meta, k));
    let log_path = run.join(format!("{role}.log"));
    let remote_log_path = run.join("remote-output.log");
    let mut last_log_fp = file_handle_fingerprint(log);
    let mut last_remote_log_fp = file_fingerprint(&remote_log_path);
    let mut last_output_at = std::time::Instant::now();
    let mut queued_at_last_output = 0.0_f64;
    let mut last_warn_at: Option<std::time::Instant> = None;
    let mut warned_stall = false;
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let pid = child.id() as i32;
    if let Some(h) = holder {
        h.store(pid, std::sync::atomic::Ordering::SeqCst);
    }
    if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst)) {
        kill_group(pid, 1.0);
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let waiter = std::thread::spawn(move || {
        loop {
            let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if rc == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break;
            }
        }
        let _ = tx.send(());
    });
    let timed = loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break false,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if holder.is_none() && crate::lane::stopped() {
            break false;
        }
        if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst)) {
            break false;
        }
        let queued = (crate::lane::queued_seconds(run) - queued_before).max(0.0);
        if let Some(limit) = timeout.filter(|t| *t > 0.0) {
            if started.elapsed().as_secs_f64() - queued >= limit {
                break true;
            }
        }
        if let Some(kind) = stall_kind {
            let log_grew = file_handle_output_advanced(log, &mut last_log_fp);
            let remote_grew = file_output_advanced(&remote_log_path, &mut last_remote_log_fp);
            if log_grew || remote_grew {
                last_output_at = std::time::Instant::now();
                queued_at_last_output = queued;
                if warned_stall {
                    let _ = fs::remove_file(run.join("stall-warning.json"));
                    warned_stall = false;
                    last_warn_at = None;
                }
            } else if let Some(limit) = stall_limit {
                let silent = (last_output_at.elapsed().as_secs_f64()
                    - (queued - queued_at_last_output).max(0.0))
                .max(0.0);
                if silent >= limit
                    && last_warn_at.is_none_or(|t| t.elapsed().as_secs_f64() >= limit)
                {
                    let elapsed = (started.elapsed().as_secs_f64() - queued).max(0.0).round() as u64;
                    let silent_u64 = silent.round() as u64;
                    let last_line = last_non_empty_line(&log_path);
                    let phase_zh = if kind == "generate" { "生成" } else { "验收" };
                    let detail = if last_line.is_empty() {
                        format!("{phase_zh}已跑 {elapsed}s（{silent_u64}s 无新输出）")
                    } else {
                        format!("{phase_zh}已跑 {elapsed}s（{silent_u64}s 无新输出），最后输出：{last_line}")
                    };
                    let at = iso();
                    let _ = write_json(
                        run.join("stall-warning.json"),
                        &serde_json::json!({
                            "kind": kind,
                            "cmd": clip(command, 180),
                            "elapsedSeconds": elapsed,
                            "silentSeconds": silent_u64,
                            "limitSeconds": limit,
                            "lastLine": last_line,
                            "detail": detail,
                            "at": at,
                        }),
                    );
                    crate::supervise::log_event(
                        run,
                        serde_json::json!({
                            "e": "stall_warning",
                            "kind": kind,
                            "cmd": clip(command, 180),
                            "elapsedSeconds": elapsed,
                            "silentSeconds": silent_u64,
                            "limitSeconds": limit,
                            "lastLine": last_line,
                            "detail": detail,
                            "at": at,
                        }),
                    );
                    let run_id = s(&meta, "run");
                    let repo = if !s(&meta["worktree"], "source").is_empty() {
                        s(&meta["worktree"], "source")
                    } else if !s(&meta, "top").is_empty() {
                        s(&meta, "top")
                    } else {
                        s(&meta, "workdir")
                    };
                    crate::events::emit(
                        run_id,
                        repo,
                        "stall_warning",
                        serde_json::json!({
                            "name": s(&meta, "name"),
                            "stallKind": kind,
                            "cmd": clip(command, 120),
                            "elapsedSeconds": elapsed,
                            "silentSeconds": silent_u64,
                            "limitSeconds": limit,
                            "lastLine": last_line,
                            "detail": detail,
                        }),
                    );
                    last_warn_at = Some(std::time::Instant::now());
                    warned_stall = true;
                }
            }
        }
    };
    if warned_stall {
        let _ = fs::remove_file(run.join("stall-warning.json"));
    }
    crate::cleanup::record_with_config(cleanup_dir, run, pid);
    end_group(pid, if timed { 5.0 } else { 2.0 });
    if let Some(h) = holder {
        h.store(0, std::sync::atomic::Ordering::SeqCst);
    }
    let _ = waiter.join();
    let code = if timed {
        124
    } else {
        use std::os::unix::process::ExitStatusExt;
        let status = child.wait().map_err(|e| e.to_string())?;
        status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(1))
    };
    if timed {
        let _ = child.wait();
    }
    Ok((code, timed))
}
/// Metadata fingerprint of a log file used to detect new output even across
/// tail-log truncation/rename (where file size shrinks and inode changes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileFingerprint {
    pub len: u64,
    pub modified: Option<std::time::SystemTime>,
    pub ino: u64,
}

fn fingerprint_from_meta(meta: &fs::Metadata) -> FileFingerprint {
    use std::os::unix::fs::MetadataExt;
    FileFingerprint {
        len: meta.len(),
        modified: meta.modified().ok(),
        ino: meta.ino(),
    }
}

fn fingerprint_advanced(
    cur: Option<FileFingerprint>,
    prev: &mut Option<FileFingerprint>,
) -> bool {
    if cur == *prev {
        return false;
    }
    let grew = cur.is_some_and(|fp| fp.len > 0);
    *prev = cur;
    grew
}

pub(crate) fn file_fingerprint(path: &Path) -> Option<FileFingerprint> {
    fs::metadata(path).ok().map(|m| fingerprint_from_meta(&m))
}

pub(crate) fn file_handle_fingerprint(file: &File) -> Option<FileFingerprint> {
    file.metadata().ok().map(|m| fingerprint_from_meta(&m))
}

/// Returns true if `path` has new non-empty output since `prev`, updating `prev`
/// in place. Creating or truncating a file to 0 bytes updates `prev` without
/// counting as progress.
pub(crate) fn file_output_advanced(path: &Path, prev: &mut Option<FileFingerprint>) -> bool {
    fingerprint_advanced(file_fingerprint(path), prev)
}

/// Same as `file_output_advanced`, but inspects an already-open `File` handle
/// directly (so callers using custom log filenames like `protect-generate.log`
/// are tracked accurately).
pub(crate) fn file_handle_output_advanced(
    file: &File,
    prev: &mut Option<FileFingerprint>,
) -> bool {
    fingerprint_advanced(file_handle_fingerprint(file), prev)
}

/// A queue/transport interruption is not evidence that the acceptance command failed.
pub fn shell_outcome(run: &Path, role: &str, code: i32, timed: bool) -> Value {
    let status = json(run.join(format!("{role}.remote.json")));
    let infrastructure = code == 75 && s(&status, "stage") == "infrastructure";
    serde_json::json!({"infrastructureFailure": infrastructure,
        "notExecuted": (infrastructure || timed) && status.is_object() && !b(&status, "executed"),
        "timedOut": timed, "remoteStage": status["stage"]})
}
pub fn finish_run(run: &std::path::Path, mut summary: serde_json::Value, code: i32) -> Res<()> {
    crate::runs::record_worktree_bytes(run);
    crate::source_build::record(run);
    let meta = json(run.join("meta.json"));
    if let Some(sources) = meta.get("configSources") {
        summary["configSources"] = sources.clone();
    }
    if meta["agentDeny"]
        .as_array()
        .is_some_and(|rules| !rules.is_empty())
    {
        summary["denied"] = serde_json::json!(crate::deny::count(run));
    }
    if meta["cmdOut"].is_object() || !s(&meta, "cmdOutGroup").is_empty() {
        let group = {
            let g = s(&meta, "cmdOutGroup");
            if !g.is_empty() {
                g
            } else {
                s(&meta["cmdOut"], "group")
            }
        };
        let stats = crate::cmd_out::count_stats(run);
        let configured = crate::cmd_out::parse_commands(&meta["cmdOut"]["commands"]);
        let (author_steps, compile_test_steps) =
            crate::cmd_out::count_author_steps(run, &configured);
        summary["cmdOut"] = serde_json::json!({
            "group": group,
            "enabled": b(&meta["cmdOut"], "enabled"),
            "triggers": stats.triggers,
            "truncatedBytes": stats.truncated_bytes,
            "authorSteps": author_steps,
            "compileTestSteps": compile_test_steps,
        });
    }
    summary["finishedAt"] = serde_json::json!(iso());
    write_json(run.join("summary.json"), &summary)?;
    crate::refs::mark_finished(run);
    write(run.join("exit_code"), format!("{code}\n"))?;
    let spawn_premerge = crate::premerge::mark_pending_before_finish(run, &meta, &summary);
    crate::history::record_finish(run);
    crate::board::refresh_for_run(run);
    crate::events::emit_run_finished(run, &meta, &summary);
    crate::eta::record_calibration_delivered(run, &meta, &summary);
    crate::work::on_run_finished(run, &meta, &summary);
    crate::work::check_stale_and_expired();
    crate::index::upsert_run(run);
    crate::dag_queue::advance();
    if spawn_premerge {
        crate::premerge::spawn_worker_for_meta(&meta);
    } else if s(&summary, "state") == "blocked_mainline" && summary["mainlineBlock"].is_object() {
        let src = s(&meta["worktree"], "source");
        if !src.is_empty() {
            let src_path = std::path::Path::new(src);
            let cur_head = read_git_head(src_path).unwrap_or_default();
            let blocked_base = s(&summary["mainlineBlock"], "blockedBaseHead");
            if !cur_head.is_empty() && cur_head != blocked_base {
                crate::premerge::spawn_worker_force(src_path);
            }
        }
    }
    Ok(())
}

pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, &ac) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &bc) in b.iter().enumerate() {
            let cost = usize::from(ac != bc);
            curr[j + 1] = (prev[j + 1] + 1)
                .min(curr[j] + 1)
                .min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

const FLAG_SYNONYM_GROUPS: &[&[&str]] = &[
    &["reason", "evidence", "why"],
    &["message", "msg", "prompt"],
    &["file", "prompt-file"],
    &["dir", "cwd", "workdir", "repo"],
    &["ro", "readonly", "read-only"],
    &["wt", "worktree"],
    &["test", "check", "accept"],
    &["quiet", "silent", "no-result"],
    &["verbose", "full"],
    &["limit", "top", "tail", "events-limit"],
    &["format", "json"],
    &["after", "after-applied", "after-delivered", "after-work", "after-time"],
    &["force", "overwrite"],
    &["cancel", "delete", "rm", "remove"],
    &["since", "window"],
    &["notes", "note"],
];

fn normalize_flag_key(raw: &str) -> String {
    let before_eq = raw.split_once('=').map_or(raw, |(k, _)| k);
    before_eq.trim_start_matches('-').to_ascii_lowercase()
}

pub fn suggest_flag<'a>(arg: &str, valid_flags: &[&'a str]) -> Option<&'a str> {
    let arg_is_flag = arg.starts_with('-');
    let arg_key = normalize_flag_key(arg);
    if arg_key.is_empty() {
        return None;
    }

    // 1. Exact synonym match among valid flags.
    for group in FLAG_SYNONYM_GROUPS {
        if group.contains(&arg_key.as_str()) {
            for &cand in valid_flags {
                if cand.starts_with('-') != arg_is_flag {
                    continue;
                }
                let cand_key = normalize_flag_key(cand);
                if cand_key != arg_key && group.contains(&cand_key.as_str()) {
                    return Some(cand);
                }
            }
        }
    }

    // 2. Levenshtein distance <= 2 (<= 1 for very short keys; skip 1-char candidates).
    let max_dist = if arg_key.len() <= 2 { 1 } else { 2 };
    let mut best: Option<(usize, &'a str)> = None;
    for &cand in valid_flags {
        if cand.starts_with('-') != arg_is_flag {
            continue;
        }
        let cand_key = normalize_flag_key(cand);
        if cand_key.len() < 2 || cand_key == arg_key {
            continue;
        }
        let d = levenshtein(&arg_key, &cand_key);
        if d <= max_dist && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, cand));
        }
    }
    best.map(|(_, cand)| cand)
}

/// Format an unrecognized CLI argument error with nearest-flag / synonym suggestion and 1-line usage.
/// `usage` may be a top-level command name (resolved via `help::usage_line`) or a full usage line.
pub fn unrecognized_arg(arg: &str, valid_flags: &[&str], usage: &str) -> String {
    let head = match suggest_flag(arg, valid_flags) {
        Some(suggestion) => format!("unrecognized argument '{arg}'; did you mean '{suggestion}'?"),
        None => format!("unrecognized argument '{arg}'"),
    };
    let resolved_usage = if usage.is_empty() {
        String::new()
    } else if let Some(stripped) = usage
        .strip_prefix("usage:")
        .or_else(|| usage.strip_prefix("Usage:"))
    {
        stripped.trim().to_string()
    } else if !usage.contains(' ') {
        crate::help::usage_line(usage).unwrap_or_else(|| format!("delegate {usage}"))
    } else {
        usage.to_string()
    };
    if resolved_usage.is_empty() {
        head
    } else {
        format!("{head}\nusage: {resolved_usage}")
    }
}

#[cfg(test)]
mod git_lock_tests {
    use super::*;
    use std::collections::HashMap;
    #[test]
    fn every_git_command_disables_optional_index_locks() {
        let command = git_command();
        assert!(command
            .get_envs()
            .any(|(key, value)| key == "GIT_OPTIONAL_LOCKS"
                && value == Some(std::ffi::OsStr::new("0"))));
    }

    #[test]
    fn caller_source_priority_respects_delegate_caller_and_antigravity_agent() {
        let lookup = |pairs: &[(&str, &str)]| {
            let map: HashMap<String, String> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            caller_source_with(move |k| map.get(k).cloned())
        };
        assert_eq!(
            lookup(&[
                ("DELEGATE_CALLER", "explicit"),
                ("ANTIGRAVITY_AGENT", "1"),
                ("ANTIGRAVITY_CONVERSATION_ID", "agy-1"),
                ("CLAUDE_CODE_SESSION_ID", "claude-1"),
            ]),
            Some(("explicit".into(), "DELEGATE_CALLER"))
        );
        assert_eq!(
            lookup(&[
                ("ANTIGRAVITY_AGENT", "1"),
                ("ANTIGRAVITY_CONVERSATION_ID", "agy-1"),
                ("CLAUDE_CODE_SESSION_ID", "claude-1"),
                ("CODEX_THREAD_ID", "codex-1"),
                ("PI_SESSION_ID", "pi-1"),
            ]),
            Some(("agy-1".into(), "ANTIGRAVITY_CONVERSATION_ID"))
        );
        assert_eq!(
            lookup(&[
                ("ANTIGRAVITY_CONVERSATION_ID", "agy-1"),
                ("CLAUDE_CODE_SESSION_ID", "claude-1"),
            ]),
            Some(("claude-1".into(), "CLAUDE_CODE_SESSION_ID"))
        );
        assert_eq!(
            lookup(&[("ANTIGRAVITY_CONVERSATION_ID", "agy-only")]),
            Some(("agy-only".into(), "ANTIGRAVITY_CONVERSATION_ID"))
        );
        assert_eq!(
            lookup(&[
                ("CODEX_THREAD_ID", "codex-1"),
                ("PI_SESSION_ID", "pi-1"),
            ]),
            Some(("codex-1".into(), "CODEX_THREAD_ID"))
        );
        assert_eq!(
            lookup(&[("PI_SESSION_ID", "pi-1")]),
            Some(("pi-1".into(), "PI_SESSION_ID"))
        );
        assert_eq!(lookup(&[]), None);
    }

    #[test]
    fn clean_env_strips_parent_controller_and_antigravity_vars() {
        let mut cmd = Command::new("true");
        clean_env(&mut cmd, &serde_json::json!({"CUSTOM_KEEP": "yes"}));
        let envs: HashMap<String, Option<String>> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|val| val.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for key in CONTROLLER_ENV {
            assert_eq!(envs.get(key), Some(&None), "expected {key} to be removed");
        }
        assert_eq!(envs.get("CUSTOM_KEEP"), Some(&Some("yes".into())));
    }

    #[test]
    fn unrecognized_arg_suggests_typos_synonyms_and_usage() {
        assert_eq!(levenshtein("comit", "commit"), 1);
        assert_eq!(levenshtein("readonl", "read-only"), 2);

        let err = unrecognized_arg("--comit", &["--commit", "--verify", "--json"], "apply");
        assert!(err.contains("unrecognized argument '--comit'; did you mean '--commit'?"), "{err}");
        assert!(err.contains("usage: delegate apply "), "{err}");

        let err2 = unrecognized_arg("--readonl", &["--read-only", "--worktree"], "start");
        assert!(err2.contains("unrecognized argument '--readonl'; did you mean '--read-only'?"), "{err2}");
        assert!(err2.contains("usage: delegate start "), "{err2}");

        let err3 = unrecognized_arg(
            "--evidence",
            &["--reason", "--json"],
            "delegate work drop <Wxx> --reason <text> [--json]",
        );
        assert!(err3.contains("unrecognized argument '--evidence'; did you mean '--reason'?"), "{err3}");
        assert!(err3.contains("usage: delegate work drop <Wxx>"), "{err3}");

        let err4 = unrecognized_arg("--wait-manual", &["--repo", "--json"], "delegate work add <title>");
        assert!(err4.starts_with("unrecognized argument '--wait-manual'\nusage: delegate work add <title>"), "{err4}");
    }

    #[test]
    fn file_handle_and_run_shell_track_custom_log_filename_progress() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!(
            "delegate-custom-log-fp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        // Direct handle test with a non-{role}.log filename.
        let custom_log_path = dir.join("protect-generate.log");
        let mut handle = File::create(&custom_log_path).unwrap();
        let mut prev = file_handle_fingerprint(&handle);
        assert!(prev.is_some());
        assert!(!file_handle_output_advanced(&handle, &mut prev));
        writeln!(handle, "building target").unwrap();
        handle.flush().unwrap();
        assert!(file_handle_output_advanced(&handle, &mut prev));
        assert!(!file_handle_output_advanced(&handle, &mut prev));

        // End-to-end run_shell test: role="generate" with custom log file
        // `protect-generate.log` (while `generate.log` does not exist) must not
        // emit stall_warning while stdout keeps growing past stall.generate.
        write_json(
            dir.join("meta.json"),
            &serde_json::json!({
                "run": "test-custom-log",
                "name": "custom-log",
                "workdir": dir.to_string_lossy(),
                "stall": {"generate": ".2s"}
            }),
        )
        .unwrap();
        let mut shell_log = File::create(&custom_log_path).unwrap();
        let (code, timed) = run_shell_with(
            (
                "i=0; while [ $i -lt 9 ]; do echo step $i; sleep 0.05; i=$((i+1)); done",
                "generate",
            ),
            &dir,
            &dir,
            &serde_json::json!({}),
            None,
            &mut shell_log,
            None,
            true,
        )
        .unwrap();
        assert_eq!((code, timed), (0, false));
        assert!(!dir.join("generate.log").exists());
        assert!(!dir.join("stall-warning.json").exists());
        let events = fs::read_to_string(dir.join("events.jsonl")).unwrap_or_default();
        assert!(
            !events.contains("stall_warning"),
            "unexpected stall_warning while custom log file was growing: {events}"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
