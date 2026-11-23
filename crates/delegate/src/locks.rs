//! Global file-lock ordering, non-blocking read-only helpers, and slow-wait diagnostics.
//!
//! All file locks in `delegate` MUST be acquired in strictly ascending `LockKind` order so
//! cross-process AB-BA deadlocks (e.g. `status` holding `.work.lock` waiting for `.start.lock`
//! while `start` holds `.start.lock` waiting for `.work.lock`) cannot occur.
//!
//! Read-only commands (`status`, `work` view, TUI collectors) should prefer lock-free reads of
//! atomically-renamed files, or `try_acquire_with_retry` with a snapshot fallback, so they never
//! block writers.
//!
//! When a blocking acquisition waits longer than `warn_threshold()` (default 10s), we emit a
//! diagnostic containing the lock holder's PID and command line (from `/proc/locks` and owner
//! metadata written into the lock file). Per ADR 0120, we only report — never force-kill.

use crate::common::{iso, setting};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Strict global acquisition order for all file locks in `delegate`.
/// Lower numeric rank MUST always be acquired before higher numeric rank.
///
/// Control-plane metadata (`start`/slot admission, `work.jsonl`, `events.jsonl`, `cursors.json`,
/// `dag-queue.json`, `repos.json`, `index.json`) shares a SINGLE short-lived lock:
/// `LockKind::State` (`<state>/.state.lock`, held < 5ms, no git / subprocess / sleep inside).
/// Read-only paths NEVER acquire `LockKind::State`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u16)]
pub enum LockKind {
    /// Short-lived background worker single-instance lock (`_gc-worker`, etc., `LOCK_NB`).
    /// Outermost role guard: acquired once at worker startup before any state/pool/history locks.
    Worker = 3,
    /// `<run>/waiter.lock` — outer role-claim lock held across `delegate wait` (non-blocking only).
    WaiterClaim = 5,
    /// `<run>/reply.lock` — per-run pending-reply mailbox lock (wraps `launch::reply` delivery).
    ReplyMailbox = 8,
    /// `<cache>/pool/.claim.lock` — worktree pool slot claim.
    PoolClaim = 30,
    /// `<source_watch>/apply.lock` — serializes apply writes against source repo.
    SourceWatchApply = 40,
    /// `<source_watch>/lock` — source-watch observation & baseline lock.
    SourceWatch = 50,
    /// `<state>/.state.lock` — THE single machine-wide short-lived control-plane write lock
    /// (<5ms, zero git/subprocess/sleep; never acquires outer pool/source_watch locks).
    State = 90,
    /// `<state>/.start.lock` — transition compatibility lock acquired ONLY if already present on disk.
    LegacyStart = 92,
    /// `<state>/.work.lock` — transition compatibility lock acquired ONLY if already present on disk.
    LegacyWork = 94,
    /// `<history>/<key>/history.lock` — per-repo history log.
    History = 110,
    /// `<history>/<key>/board.lock` — per-repo `BOARD.md` write lock.
    Board = 120,
    /// `<state>/jobserver.lock` — machine-wide cargo jobserver FIFO arbitration.
    Jobserver = 130,
    /// Heavy lane, measure queue, or remote lane lock.
    Lane = 140,
    /// Per-run or standalone local locks (`scopes.lock`, `cleanup.lock`, `supervisor.lock`, `fit`).
    RunLocal = 150,
}

impl LockKind {
    pub fn name(self) -> &'static str {
        match self {
            LockKind::WaiterClaim => "WaiterClaim",
            LockKind::ReplyMailbox => "ReplyMailbox",
            LockKind::State => "State",
            LockKind::LegacyStart => "LegacyStart",
            LockKind::LegacyWork => "LegacyWork",
            LockKind::PoolClaim => "PoolClaim",
            LockKind::SourceWatchApply => "SourceWatchApply",
            LockKind::SourceWatch => "SourceWatch",
            LockKind::History => "History",
            LockKind::Board => "Board",
            LockKind::Jobserver => "Jobserver",
            LockKind::Lane => "Lane",
            LockKind::Worker => "Worker",
            LockKind::RunLocal => "RunLocal",
        }
    }
}

thread_local! {
    static HELD_LOCKS: RefCell<Vec<(LockKind, PathBuf)>> = const { RefCell::new(Vec::new()) };
    static STATE_LOCK_DEPTH: RefCell<usize> = const { RefCell::new(0) };
}

/// Returns true if the current thread currently holds `state/.state.lock`.
pub fn holding_state_lock() -> bool {
    STATE_LOCK_DEPTH.with(|d| *d.borrow() > 0)
}

/// Guard against running git / subprocesses / sleeps while holding `state/.state.lock`.
pub fn assert_not_holding_state_lock(op: &str) {
    if holding_state_lock() {
        let msg = format!(
            "delegate: forbidden slow operation ({op}) while holding state/.state.lock"
        );
        #[cfg(debug_assertions)]
        panic!("{msg}");
        #[cfg(not(debug_assertions))]
        eprintln!("{msg}");
    }
}

/// Re-entrant guard for the single control-plane state write lock (`<state>/.state.lock`),
/// with fixed-order transition compatibility for legacy `.start.lock`/`.work.lock` if present.
#[derive(Debug)]
pub struct StateLockGuard {
    _state: Option<LockGuard>,
    _legacy_start: Option<LockGuard>,
    _legacy_work: Option<LockGuard>,
    acquired_at: Option<Instant>,
}

impl Drop for StateLockGuard {
    fn drop(&mut self) {
        if let Some(t0) = self.acquired_at {
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            if std::env::var("DELEGATE_MEASURE_STATE_LOCK").as_deref() == Ok("1") {
                eprintln!("STATE_LOCK_HOLD_MS={ms:.3}");
            }
            if std::env::var("DELEGATE_MEASURE_START_LOCK").as_deref() == Ok("1") {
                eprintln!("START_LOCK_HOLD_MS={ms:.3}");
            }
        }
        // Release legacy locks before `.state.lock` (reverse acquisition order).
        self._legacy_work.take();
        self._legacy_start.take();
        self._state.take();
        STATE_LOCK_DEPTH.with(|d| {
            let mut depth = d.borrow_mut();
            *depth = depth.saturating_sub(1);
        });
    }
}

/// Acquire `<state>/.state.lock` (re-entrant within the same thread).
/// Hold duration MUST remain < 5ms with zero git/subprocess/sleep calls inside.
pub fn acquire_state(state: &Path) -> io::Result<StateLockGuard> {
    let already_held = STATE_LOCK_DEPTH.with(|d| *d.borrow() > 0);
    if already_held {
        STATE_LOCK_DEPTH.with(|d| *d.borrow_mut() += 1);
        return Ok(StateLockGuard {
            _state: None,
            _legacy_start: None,
            _legacy_work: None,
            acquired_at: None,
        });
    }
    let state_guard = acquire(LockKind::State, &state.join(".state.lock"), true)?;
    // Transition compatibility with 6.31.x CLI: if legacy `.start.lock` or `.work.lock` already
    // exists on disk, acquire them in the same fixed ascending order (`.start.lock` < `.work.lock`)
    // without modifying their owner metadata. New state dirs have neither file (zero extra flock).
    let legacy_start_path = state.join(".start.lock");
    let legacy_start = if legacy_start_path.exists() {
        Some(acquire_opts(
            LockKind::LegacyStart,
            &legacy_start_path,
            true,
            false,
        )?)
    } else {
        None
    };
    let legacy_work_path = state.join(".work.lock");
    let legacy_work = if legacy_work_path.exists() {
        Some(acquire_opts(
            LockKind::LegacyWork,
            &legacy_work_path,
            true,
            false,
        )?)
    } else {
        None
    };
    STATE_LOCK_DEPTH.with(|d| *d.borrow_mut() += 1);
    Ok(StateLockGuard {
        _state: Some(state_guard),
        _legacy_start: legacy_start,
        _legacy_work: legacy_work,
        acquired_at: Some(Instant::now()),
    })
}

/// Non-blocking single attempt to acquire `<state>/.state.lock`.
pub fn try_acquire_state(state: &Path) -> io::Result<StateLockGuard> {
    let already_held = STATE_LOCK_DEPTH.with(|d| *d.borrow() > 0);
    if already_held {
        STATE_LOCK_DEPTH.with(|d| *d.borrow_mut() += 1);
        return Ok(StateLockGuard {
            _state: None,
            _legacy_start: None,
            _legacy_work: None,
            acquired_at: None,
        });
    }
    let state_guard = try_acquire(LockKind::State, &state.join(".state.lock"), true)?;
    let legacy_start_path = state.join(".start.lock");
    let legacy_start = if legacy_start_path.exists() {
        Some(try_acquire_opts(
            LockKind::LegacyStart,
            &legacy_start_path,
            true,
            false,
        )?)
    } else {
        None
    };
    let legacy_work_path = state.join(".work.lock");
    let legacy_work = if legacy_work_path.exists() {
        Some(try_acquire_opts(
            LockKind::LegacyWork,
            &legacy_work_path,
            true,
            false,
        )?)
    } else {
        None
    };
    STATE_LOCK_DEPTH.with(|d| *d.borrow_mut() += 1);
    Ok(StateLockGuard {
        _state: Some(state_guard),
        _legacy_start: legacy_start,
        _legacy_work: legacy_work,
        acquired_at: Some(Instant::now()),
    })
}

/// RAII guard holding an OS `flock` and tracking global lock-order compliance.
#[derive(Debug)]
pub struct LockGuard {
    file: File,
    kind: LockKind,
    path: PathBuf,
}

impl LockGuard {
    #[cfg(test)]
    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        HELD_LOCKS.with(|held| {
            let mut stack = held.borrow_mut();
            if let Some(pos) = stack
                .iter()
                .rposition(|(k, p)| *k == self.kind && *p == self.path)
            {
                stack.remove(pos);
            }
        });
    }
}

fn check_order(kind: LockKind, path: &Path) {
    HELD_LOCKS.with(|held| {
        let stack = held.borrow();
        for (held_kind, held_path) in stack.iter() {
            // Distinct per-run waiter claims, per-run local locks, or lane tickets may coexist.
            if matches!(
                (*held_kind, kind),
                (LockKind::WaiterClaim, LockKind::WaiterClaim)
                    | (LockKind::RunLocal, LockKind::RunLocal)
                    | (LockKind::Lane, LockKind::Lane)
            ) && held_path != path
            {
                continue;
            }
            if (*held_kind as u16) >= (kind as u16) {
                let msg = format!(
                    "delegate: lock order violation: acquiring {} ({}) while holding {} ({})",
                    kind.name(),
                    path.display(),
                    held_kind.name(),
                    held_path.display()
                );
                #[cfg(debug_assertions)]
                panic!("{msg}");
                #[cfg(not(debug_assertions))]
                eprintln!("{msg}");
            }
        }
    });
}

fn push_held(kind: LockKind, path: PathBuf) {
    HELD_LOCKS.with(|held| {
        held.borrow_mut().push((kind, path));
    });
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            let _ = fs::create_dir_all(parent);
        }
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
}

fn current_cmdline() -> String {
    let args: Vec<String> = std::env::args().collect();
    if args.is_empty() {
        return String::new();
    }
    let joined = args.join(" ");
    if joined.chars().count() > 200 {
        let mut s: String = joined.chars().take(200).collect();
        s.push_str("...");
        s
    } else {
        joined
    }
}

fn write_owner_metadata(f: &mut File, kind: LockKind) {
    let payload = json!({
        "pid": std::process::id(),
        "cmd": current_cmdline(),
        "kind": kind.name(),
        "at": iso(),
    });
    let text = format!("{payload}\n");
    if f.seek(SeekFrom::Start(0)).is_ok() && f.set_len(0).is_ok() {
        let _ = f.write_all(text.as_bytes());
        let _ = f.flush();
    }
}

fn warn_threshold() -> Duration {
    let raw = setting("LOCK_WARN_SECONDS", "10");
    let secs = raw
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(10.0);
    Duration::from_secs_f64(secs)
}

fn read_proc_cmdline(pid: u32) -> Option<String> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let parts: Vec<String> = bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(parts.join(" "))
}

/// Parse `/proc/locks` content and return holder PIDs matching `(major, minor, ino)`.
/// Skips blocked waiter lines (`->`).
pub(crate) fn parse_proc_locks_holders(content: &str, major: u64, minor: u64, ino: u64) -> Vec<u32> {
    let mut pids = Vec::new();
    for line in content.lines() {
        let mut fields = line.split_whitespace();
        let Some(_idx) = fields.next() else {
            continue;
        };
        let Some(kind) = fields.next() else {
            continue;
        };
        // Blocked waiter lines look like `1: -> FLOCK ADVISORY WRITE ...`
        if kind == "->" {
            continue;
        }
        let Some(_advisory) = fields.next() else {
            continue;
        };
        let Some(_mode) = fields.next() else {
            continue;
        };
        let Some(pid_str) = fields.next() else {
            continue;
        };
        let Some(dev_ino) = fields.next() else {
            continue;
        };
        let mut parts = dev_ino.split(':');
        let (Some(maj_s), Some(min_s), Some(ino_s)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (Ok(m_maj), Ok(m_min), Ok(m_ino)) = (
            u64::from_str_radix(maj_s, 16),
            u64::from_str_radix(min_s, 16),
            ino_s.parse::<u64>(),
        ) else {
            continue;
        };
        if m_maj == major && m_min == minor && m_ino == ino {
            if let Ok(pid) = pid_str.parse::<u32>() {
                if !pids.contains(&pid) {
                    pids.push(pid);
                }
            }
        }
    }
    pids
}

fn proc_locks_holders(file: &File) -> Vec<(u32, Option<String>)> {
    let Ok(meta) = file.metadata() else {
        return Vec::new();
    };
    let dev = meta.dev();
    let major = libc::major(dev) as u64;
    let minor = libc::minor(dev) as u64;
    let ino = meta.ino();
    let Ok(content) = fs::read_to_string("/proc/locks") else {
        return Vec::new();
    };
    parse_proc_locks_holders(&content, major, minor, ino)
        .into_iter()
        .map(|pid| (pid, read_proc_cmdline(pid)))
        .collect()
}

/// Strictly read-only, zero-lock check whether another process holds an advisory lock on `path`
/// (via `/proc/locks` `(major, minor, ino)` match). Returns `None` if `/proc/locks` is unreadable.
pub fn file_locked_by_other_readonly(path: &Path) -> Option<bool> {
    let meta = fs::metadata(path).ok()?;
    let content = fs::read_to_string("/proc/locks").ok()?;
    let dev = meta.dev();
    let major = libc::major(dev) as u64;
    let minor = libc::minor(dev) as u64;
    let ino = meta.ino();
    let my_pid = std::process::id();
    let holders = parse_proc_locks_holders(&content, major, minor, ino);
    Some(holders.into_iter().any(|pid| pid != my_pid))
}

fn read_owner_file(path: &Path) -> Option<(u32, String)> {
    let text = fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    let pid = v.get("pid")?.as_u64()? as u32;
    let cmd = v
        .get("cmd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some((pid, cmd))
}

/// Build a human-readable diagnostic describing who holds `path` (report only, never kill).
pub fn describe_holder(file: &File, path: &Path) -> String {
    let proc_holders = proc_locks_holders(file);
    let file_owner = read_owner_file(path);
    let mut parts = Vec::new();

    for (pid, cmd_opt) in &proc_holders {
        let cmd = cmd_opt
            .as_deref()
            .filter(|c| !c.is_empty())
            .or_else(|| {
                file_owner
                    .as_ref()
                    .filter(|(fpid, _)| fpid == pid)
                    .map(|(_, c)| c.as_str())
            })
            .unwrap_or("<unknown>");
        parts.push(format!("pid={pid} cmd={cmd:?}"));
    }

    if parts.is_empty() {
        if let Some((pid, cmd)) = file_owner {
            let live_cmd = read_proc_cmdline(pid).unwrap_or(cmd);
            let display = if live_cmd.is_empty() {
                "<unknown>".to_string()
            } else {
                live_cmd
            };
            parts.push(format!("pid={pid} cmd={display:?}"));
        } else {
            parts.push("holder=unknown".to_string());
        }
    }

    parts.join(", ")
}

fn flock_raw(fd: i32, exclusive: bool, nonblock: bool) -> io::Result<()> {
    let op = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    } | if nonblock { libc::LOCK_NB } else { 0 };
    loop {
        let rc = unsafe { libc::flock(fd, op) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

/// Non-blocking single attempt (`LOCK_NB`). Never waits.
pub fn try_acquire(kind: LockKind, path: &Path, exclusive: bool) -> io::Result<LockGuard> {
    try_acquire_opts(kind, path, exclusive, exclusive)
}

/// Non-blocking single attempt, with explicit control over whether to write owner metadata.
pub fn try_acquire_opts(
    kind: LockKind,
    path: &Path,
    exclusive: bool,
    record_owner: bool,
) -> io::Result<LockGuard> {
    check_order(kind, path);
    let mut f = open_lock_file(path)?;
    flock_raw(f.as_raw_fd(), exclusive, true)?;
    if exclusive && record_owner {
        write_owner_metadata(&mut f, kind);
    }
    push_held(kind, path.to_path_buf());
    Ok(LockGuard {
        file: f,
        kind,
        path: path.to_path_buf(),
    })
}

/// Bounded non-blocking retry helper for read-only or opportunistic callers.
/// Never blocks writers indefinitely; returns `WouldBlock` if still busy after `retries`.
pub fn try_acquire_with_retry(
    kind: LockKind,
    path: &Path,
    exclusive: bool,
    retries: usize,
    delay: Duration,
) -> io::Result<LockGuard> {
    for attempt in 0..=retries {
        match try_acquire(kind, path, exclusive) {
            Ok(guard) => return Ok(guard),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock && attempt < retries => {
                std::thread::sleep(delay);
            }
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::from(io::ErrorKind::WouldBlock))
}

/// Blocking acquisition enforcing global lock ordering and emitting a diagnostic if wait > 10s
/// (configurable via `DELEGATE_LOCK_WARN_SECONDS`). Never kills the holder (ADR 0120).
pub fn acquire(kind: LockKind, path: &Path, exclusive: bool) -> io::Result<LockGuard> {
    acquire_opts(kind, path, exclusive, exclusive)
}

/// Blocking acquisition with explicit control over whether owner metadata is written into the file.
pub fn acquire_opts(
    kind: LockKind,
    path: &Path,
    exclusive: bool,
    record_owner: bool,
) -> io::Result<LockGuard> {
    check_order(kind, path);
    let mut f = open_lock_file(path)?;
    // Fast uncontended path: 1 syscall, zero sleeping.
    match flock_raw(f.as_raw_fd(), exclusive, true) {
        Ok(()) => {
            if exclusive && record_owner {
                write_owner_metadata(&mut f, kind);
            }
            push_held(kind, path.to_path_buf());
            return Ok(LockGuard {
                file: f,
                kind,
                path: path.to_path_buf(),
            });
        }
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
        Err(err) => return Err(err),
    }

    let start = Instant::now();
    let warn_after = warn_threshold();
    let repeat_interval = warn_after.max(Duration::from_secs(30));
    let mut next_warn = warn_after;
    let mut sleep_ms = 5u64;

    loop {
        std::thread::sleep(Duration::from_millis(sleep_ms));
        sleep_ms = (sleep_ms * 2).min(50);

        match flock_raw(f.as_raw_fd(), exclusive, true) {
            Ok(()) => {
                if exclusive && record_owner {
                    write_owner_metadata(&mut f, kind);
                }
                push_held(kind, path.to_path_buf());
                return Ok(LockGuard {
                    file: f,
                    kind,
                    path: path.to_path_buf(),
                });
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                let elapsed = start.elapsed();
                if elapsed >= next_warn {
                    let holder = describe_holder(&f, path);
                    eprintln!(
                        "delegate: lock wait >{:.1}s on {} ({}, waited {:.1}s); {} (report only, ADR 0120)",
                        warn_after.as_secs_f64(),
                        path.display(),
                        kind.name(),
                        elapsed.as_secs_f64(),
                        holder
                    );
                    next_warn = elapsed + repeat_interval;
                }
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn temp_lock(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("delegate-locks-test-{nanos}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn parse_proc_locks_matches_holder_and_skips_waiters() {
        let sample = "\
1: POSIX  ADVISORY  READ 134201 103:03:97681669 128 128
2: FLOCK  ADVISORY  WRITE 424242 103:03:12345678 0 EOF
2: -> FLOCK  ADVISORY  WRITE 999999 103:03:12345678 0 EOF
3: FLOCK  ADVISORY  WRITE 555555 00:2a:12345678 0 EOF
";
        assert_eq!(
            parse_proc_locks_holders(sample, 0x103, 0x03, 12345678),
            vec![424242]
        );
        assert_eq!(
            parse_proc_locks_holders(sample, 0x00, 0x2a, 12345678),
            vec![555555]
        );
        assert!(parse_proc_locks_holders(sample, 0x103, 0x03, 999).is_empty());
    }

    #[test]
    fn exclusive_lock_records_owner_and_describe_holder_finds_pid() {
        let path = temp_lock("owner.lock");
        let guard = acquire(LockKind::State, &path, true).unwrap();
        let desc = describe_holder(guard.file(), &path);
        let expected_pid = format!("pid={}", std::process::id());
        assert!(desc.contains(&expected_pid), "expected {expected_pid} in {desc}");
        drop(guard);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn ascending_lock_order_is_allowed() {
        let p1 = temp_lock("a.lock");
        let p2 = p1.parent().unwrap().join("b.lock");
        let g1 = acquire(LockKind::SourceWatchApply, &p1, true).unwrap();
        let g2 = acquire(LockKind::State, &p2, true).unwrap();
        drop(g2);
        drop(g1);
        let _ = fs::remove_dir_all(p1.parent().unwrap());
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn reversed_lock_order_panics_in_debug() {
        let p1 = temp_lock("rev-a.lock");
        let p2 = p1.parent().unwrap().join("rev-b.lock");
        let _g1 = acquire(LockKind::State, &p1, true).unwrap();
        let _g2 = try_acquire(LockKind::SourceWatchApply, &p2, true);
    }

    #[test]
    fn state_lock_is_reentrant_and_tracks_depth() {
        let dir = temp_lock("s").parent().unwrap().to_path_buf();
        assert!(!holding_state_lock());
        let g1 = acquire_state(&dir).unwrap();
        assert!(holding_state_lock());
        let g2 = acquire_state(&dir).unwrap();
        assert!(holding_state_lock());
        drop(g2);
        assert!(holding_state_lock());
        drop(g1);
        assert!(!holding_state_lock());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn try_acquire_with_retry_returns_would_block_without_hanging() {
        let path = temp_lock("contended.lock");
        let holder = acquire(LockKind::State, &path, true).unwrap();
        let p_clone = path.clone();
        let handle = std::thread::spawn(move || {
            let t0 = Instant::now();
            let res = try_acquire_with_retry(
                LockKind::State,
                &p_clone,
                true,
                2,
                Duration::from_millis(10),
            );
            (res.unwrap_err().kind(), t0.elapsed())
        });
        let (kind, elapsed) = handle.join().unwrap();
        assert_eq!(kind, io::ErrorKind::WouldBlock);
        assert!(elapsed < Duration::from_secs(2));
        drop(holder);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
