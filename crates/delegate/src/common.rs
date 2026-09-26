use serde_json::Value;
use std::env;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type Res<T> = Result<T, String>;
pub const GUARDS: [&str; 7] = [
    "DELEGATE_AGENT",
    "DELEGATE_PARENT_RUN",
    "PI_DELEGATE_ACTIVE",
    "DELEGATE_RUN_DIR",
    "DELEGATE_LANE_HELD",
    "PI_DELEGATE_AGENT",
    "PI_DELEGATE_PARENT_RUN",
];
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
pub fn format_time(local: bool, pattern: &str) -> String {
    let t = unsafe { libc::time(std::ptr::null_mut()) };
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
pub fn iso() -> String {
    format_time(false, "%Y-%m-%dT%H:%M:%SZ")
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
    env::current_exe()
        .unwrap_or_else(|_| PathBuf::from("delegate"))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("delegate"))
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
    git_top(&cwd).unwrap_or(cwd).join(".local/run/pi")
}
pub fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).unwrap_or_default()
}
pub fn json(path: impl AsRef<Path>) -> Value {
    serde_json::from_str(&read(path)).unwrap_or(Value::Null)
}
pub fn write(path: impl AsRef<Path>, data: impl AsRef<[u8]>) -> Res<()> {
    fs::write(path, data).map_err(|e| e.to_string())
}
pub fn write_json(path: impl AsRef<Path>, val: &Value) -> Res<()> {
    let path = path.as_ref();
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    write(&tmp, format!("{}\n", val))?;
    fs::rename(tmp, path).map_err(|e| e.to_string())
}
pub fn append(path: impl AsRef<Path>, data: &str) -> Res<()> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(data.as_bytes()))
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
    let tmp = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let mut f = lock(&tmp, true, false).map_err(|e| e.to_string())?;
    f.set_len(0).map_err(|e| e.to_string())?;
    f.write_all(content.as_bytes()).map_err(|e| e.to_string())?;
    fs::rename(tmp, path).map_err(|e| e.to_string())?;
    Ok(f)
}
pub fn cmd_output(mut c: Command) -> Res<Output> {
    c.output().map_err(|e| e.to_string())
}
pub fn git(top: &Path, args: &[&str]) -> Res<Vec<u8>> {
    let o = cmd_output({
        let mut c = Command::new("git");
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
    git_text(path, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(PathBuf::from)
}
pub fn clean_env(c: &mut Command, extra: &Value) {
    for k in GUARDS {
        c.env_remove(k);
    }
    if let Some(obj) = extra.as_object() {
        for (k, v) in obj {
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
        killpg(pid, sig);
        let start = std::time::Instant::now();
        while group_members(pid) && start.elapsed().as_secs_f64() < grace {
            std::thread::sleep(Duration::from_millis(50));
        }
        if !group_members(pid) {
            break;
        }
    }
}
pub fn kill_group(pid: i32, grace: f64) {
    killpg(pid, libc::SIGTERM);
    let start = std::time::Instant::now();
    while group_members(pid) && start.elapsed().as_secs_f64() < grace {
        std::thread::sleep(Duration::from_millis(100));
    }
    if group_members(pid) {
        killpg(pid, libc::SIGKILL);
    }
}
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        s.into()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}
pub fn run_shell(
    command: &str,
    cwd: &Path,
    extra: &Value,
    timeout: f64,
    log: &mut File,
    holder: Option<&std::sync::atomic::AtomicI32>,
) -> Res<(i32, bool)> {
    let mut c = Command::new("sh");
    c.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(Stdio::from(log.try_clone().map_err(|e| e.to_string())?));
    clean_env(&mut c, extra);
    c.env("DELEGATE_LANE_HELD", "1");
    group(&mut c);
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let pid = child.id() as i32;
    if let Some(h) = holder {
        h.store(pid, std::sync::atomic::Ordering::SeqCst);
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
    let timed = rx.recv_timeout(Duration::from_secs_f64(timeout)).is_err();
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
