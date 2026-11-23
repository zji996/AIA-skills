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
/// Where the caller's session id comes from, in order: an explicit id from any harness,
/// then the session variables Claude Code, Codex and Pi export to the commands they run.
pub const CALLER_ENV: [&str; 4] = [
    "DELEGATE_CALLER",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_THREAD_ID",
    "PI_SESSION_ID",
];
pub fn caller_source() -> Option<(String, &'static str)> {
    CALLER_ENV.iter().find_map(|name| {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(|value| (value, *name))
    })
}
pub fn caller() -> Option<String> {
    caller_source().map(|(id, _)| id)
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
    c.env("DELEGATE_LANE_HELD", "1")
        .env("DELEGATE_RUN_DIR", cleanup_dir);
    group(&mut c);
    let started = std::time::Instant::now();
    let queued_before = crate::lane::queued_seconds(run);
    let monitor_accept_stall = matches!(role, "accept" | "verify" | "premerge");
    let accept_stall_limit = if monitor_accept_stall {
        crate::agents::stall_seconds(&meta, "accept")
    } else {
        None
    };
    let log_path = run.join(format!("{role}.log"));
    let mut last_log_len = log.metadata().map(|m| m.len()).unwrap_or(0);
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
        if monitor_accept_stall {
            let cur_len = log.metadata().map(|m| m.len()).unwrap_or(last_log_len);
            if cur_len != last_log_len {
                last_log_len = cur_len;
                last_output_at = std::time::Instant::now();
                queued_at_last_output = queued;
                if warned_stall {
                    let _ = fs::remove_file(run.join("stall-warning.json"));
                    warned_stall = false;
                    last_warn_at = None;
                }
            } else if let Some(limit) = accept_stall_limit {
                let silent = (last_output_at.elapsed().as_secs_f64()
                    - (queued - queued_at_last_output).max(0.0))
                .max(0.0);
                if silent >= limit
                    && last_warn_at.is_none_or(|t| t.elapsed().as_secs_f64() >= limit)
                {
                    let elapsed = (started.elapsed().as_secs_f64() - queued).max(0.0).round() as u64;
                    let silent_u64 = silent.round() as u64;
                    let last_line = last_non_empty_line(&log_path);
                    let detail = if last_line.is_empty() {
                        format!("验收已跑 {elapsed}s（{silent_u64}s 无新输出）")
                    } else {
                        format!("验收已跑 {elapsed}s（{silent_u64}s 无新输出），最后输出：{last_line}")
                    };
                    let at = iso();
                    let _ = write_json(
                        run.join("stall-warning.json"),
                        &serde_json::json!({
                            "kind": "accept",
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
                            "kind": "accept",
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
                            "stallKind": "accept",
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
    if let Some(sources) = json(run.join("meta.json")).get("configSources") {
        summary["configSources"] = sources.clone();
    }
    if json(run.join("meta.json"))["agentDeny"]
        .as_array()
        .is_some_and(|rules| !rules.is_empty())
    {
        summary["denied"] = serde_json::json!(crate::deny::count(run));
    }
    summary["finishedAt"] = serde_json::json!(iso());
    write_json(run.join("summary.json"), &summary)?;
    crate::refs::mark_finished(run);
    write(run.join("exit_code"), format!("{code}\n"))?;
    let meta = json(run.join("meta.json"));
    let spawn_premerge = crate::premerge::mark_pending_before_finish(run, &meta, &summary);
    crate::history::record_finish(run);
    crate::board::refresh_for_run(run);
    crate::events::emit_run_finished(run, &meta, &summary);
    crate::work::on_run_finished(run, &meta, &summary);
    crate::work::check_stale_and_expired();
    crate::index::upsert_run(run);
    crate::dag_queue::advance();
    if spawn_premerge {
        crate::premerge::spawn_worker_for_meta(&meta);
    }
    Ok(())
}

#[cfg(test)]
mod git_lock_tests {
    use super::*;
    #[test]
    fn every_git_command_disables_optional_index_locks() {
        let command = git_command();
        assert!(command
            .get_envs()
            .any(|(key, value)| key == "GIT_OPTIONAL_LOCKS"
                && value == Some(std::ffi::OsStr::new("0"))));
    }
}
