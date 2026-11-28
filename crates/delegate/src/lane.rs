use crate::common::*;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader};
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneKind {
    Local,
    RemoteHeavy,
    RemoteLight,
}

impl LaneKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LaneKind::Local => "local",
            LaneKind::RemoteHeavy => "remote-heavy",
            LaneKind::RemoteLight => "remote-light",
        }
    }

    pub fn is_remote(self) -> bool {
        !matches!(self, LaneKind::Local)
    }
}

thread_local! {
    static SLOT_ENV_STACK: RefCell<Vec<Vec<(String, String)>>> = const { RefCell::new(Vec::new()) };
    static SLOT_KIND_STACK: RefCell<Vec<LaneKind>> = const { RefCell::new(Vec::new()) };
}

pub fn current_slot_env() -> Option<Vec<(String, String)>> {
    SLOT_ENV_STACK.with(|s| s.borrow().last().cloned())
}

pub fn current_lane_held_env() -> &'static str {
    SLOT_KIND_STACK.with(|s| match s.borrow().last().copied() {
        Some(LaneKind::RemoteLight) => "remote-light",
        _ => "1",
    })
}

fn already_held_for(kind: LaneKind) -> bool {
    match std::env::var("DELEGATE_LANE_HELD").ok().as_deref() {
        None => false,
        Some("remote-light") => matches!(kind, LaneKind::RemoteLight),
        Some(_) => true,
    }
}

pub static SIGNAL: AtomicI32 = AtomicI32::new(0);
static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);
static LANE_WAITING: AtomicBool = AtomicBool::new(false);
static SUPERVISOR_THREAD: AtomicUsize = AtomicUsize::new(0);
extern "C" fn signal_handler(sig: i32) {
    SIGNAL.store(sig, Ordering::SeqCst);
    let fd = SIGNAL_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = sig as u8;
        unsafe {
            libc::write(fd, (&byte as *const u8).cast(), 1);
        }
    }
}
extern "C" fn wake_handler(_: i32) {}
pub fn install_signals() {
    unsafe {
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = signal_handler as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = wake_handler as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut());
    }
}
pub fn supervisor_signal_pipe() -> io::Result<File> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = unsafe { libc::fcntl(fds[1], libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        return Err(io::Error::last_os_error());
    }
    SUPERVISOR_THREAD.store(unsafe { libc::pthread_self() } as usize, Ordering::SeqCst);
    SIGNAL_PIPE.store(fds[1], Ordering::SeqCst);
    install_signals();
    Ok(unsafe { File::from_raw_fd(fds[0]) })
}
pub fn wake_lane_waiter_after_stop() {
    let thread_id = SUPERVISOR_THREAD.load(Ordering::SeqCst);
    if thread_id != 0 {
        unsafe { libc::pthread_kill(thread_id as libc::pthread_t, libc::SIGUSR1) };
    }
    while LANE_WAITING.load(Ordering::SeqCst) {
        if thread_id != 0 {
            unsafe { libc::pthread_kill(thread_id as libc::pthread_t, libc::SIGUSR1) };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
pub fn stopped() -> bool {
    SIGNAL.load(Ordering::SeqCst) != 0
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConcurrencyPlan {
    pub threads: usize,
    pub budget: usize,
    pub max_heavy: usize,
    pub pool_tokens: usize,
    pub heavy_jobs: usize,
    pub max_heavy_explicit: bool,
    pub heavy_jobs_explicit: bool,
    pub remote: bool,
    pub fallback: bool,
    pub diagnostic: Option<String>,
}

impl ConcurrencyPlan {
    /// Human-readable summary:
    /// - Local: `线程 96 − 2 = 94 · 令牌 88/88 空闲 88` (plus `（显式 ×7）` when `heavyJobs` is explicit)
    /// - Remote (no FIFO/proxy): `线程 96 − 2 = 94 → 4 条 × 23`
    pub fn format_summary(&self) -> String {
        if self.remote {
            let lanes_str = if self.max_heavy == 0 {
                "不限".to_string()
            } else {
                format!("{} 条", self.max_heavy)
            };
            let suffix = if self.heavy_jobs_explicit {
                "（显式）"
            } else {
                ""
            };
            if self.fallback && self.threads == 0 {
                format!("{lanes_str} × {}{suffix}（回退配置）", self.heavy_jobs)
            } else {
                format!(
                    "线程 {} − 2 = {} → {} × {}{suffix}",
                    self.threads, self.budget, lanes_str, self.heavy_jobs
                )
            }
        } else {
            let explicit_suffix = if self.heavy_jobs_explicit {
                format!("（显式 ×{}）", self.heavy_jobs)
            } else {
                String::new()
            };
            format!(
                "线程 {} − 2 = {} · 令牌 {}/{} 空闲 {}{explicit_suffix}",
                self.threads, self.budget, self.pool_tokens, self.pool_tokens, self.pool_tokens
            )
        }
    }

    pub fn format_summary_with_pool(&self, pool: &crate::jobserver::PoolStatus) -> String {
        if self.remote {
            return self.format_summary();
        }
        let explicit_suffix = if self.heavy_jobs_explicit {
            format!("（显式 ×{}）", self.heavy_jobs)
        } else {
            String::new()
        };
        format!(
            "线程 {} − 2 = {} · {}{explicit_suffix}",
            self.threads,
            self.budget,
            pool.format_pool_part()
        )
    }

    pub fn to_json(&self) -> Value {
        let mut obj = json!({
            "threads": self.threads,
            "budget": self.budget,
            "maxHeavy": self.max_heavy,
            "poolTokens": self.pool_tokens,
            "heavyJobs": self.heavy_jobs,
            "maxHeavyExplicit": self.max_heavy_explicit,
            "heavyJobsExplicit": self.heavy_jobs_explicit,
            "summary": self.format_summary(),
        });
        if self.remote {
            obj["remote"] = json!(true);
        }
        if self.fallback {
            obj["fallback"] = json!(true);
        }
        if let Some(ref diag) = self.diagnostic {
            obj["diagnostic"] = json!(diag);
        }
        obj
    }

    pub fn to_json_with_pool(&self, pool: &crate::jobserver::PoolStatus) -> Value {
        let mut obj = self.to_json();
        obj["summary"] = json!(self.format_summary_with_pool(pool));
        obj["pool"] = pool.to_json();
        obj
    }
}

/// Parse a cgroup v2 `cpu.max` line (`<quota> <period>`). Returns `ceil(quota / period)` >= 1,
/// or `None` when unlimited (`max`) or unparseable.
pub fn parse_cgroup_v2_cpu_max(text: &str) -> Option<usize> {
    let mut parts = text.split_whitespace();
    let quota_str = parts.next()?;
    let period_str = parts.next()?;
    if quota_str == "max" {
        return None;
    }
    let quota: u64 = quota_str.parse().ok()?;
    let period: u64 = period_str.parse().ok()?;
    if quota == 0 || period == 0 {
        return None;
    }
    let ceil = quota.div_ceil(period).max(1);
    usize::try_from(ceil).ok()
}

/// Walk upward from the current process's cgroup v2 path (`0::<rel>` in `/proc/self/cgroup`)
/// to `cgroup_root`, returning the tightest numeric `cpu.max` limit found along the chain.
pub fn detect_cgroup_v2_cpu_limit_in(cgroup_root: &Path, proc_cgroup: &str) -> Option<usize> {
    let rel = proc_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|p| p.trim().trim_start_matches('/'))?;
    let mut cur = if rel.is_empty() {
        cgroup_root.to_path_buf()
    } else {
        cgroup_root.join(rel)
    };
    let mut tightest: Option<usize> = None;
    loop {
        let cpu_max = cur.join("cpu.max");
        if let Ok(text) = fs::read_to_string(&cpu_max) {
            if let Some(lim) = parse_cgroup_v2_cpu_max(&text) {
                tightest = Some(tightest.map_or(lim, |prev| prev.min(lim)));
            }
        }
        if cur == cgroup_root {
            break;
        }
        let Some(parent) = cur.parent() else {
            break;
        };
        if !parent.starts_with(cgroup_root) {
            break;
        }
        cur = parent.to_path_buf();
    }
    tightest
}

pub fn detect_cgroup_v2_cpu_limit() -> Option<usize> {
    let proc_cg = fs::read_to_string("/proc/self/cgroup").ok()?;
    detect_cgroup_v2_cpu_limit_in(Path::new("/sys/fs/cgroup"), &proc_cg)
}

#[allow(dead_code)]
pub fn detect_nproc() -> Option<usize> {
    let out = Command::new("nproc").output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|&n| n > 0)
}

/// Effective available threads = min(available_parallelism, cgroup v2 cpu.max, nproc), floor 1.
pub fn effective_threads(
    avail_parallelism: Option<usize>,
    cgroup_limit: Option<usize>,
    nproc_val: Option<usize>,
) -> usize {
    [avail_parallelism, cgroup_limit, nproc_val]
        .into_iter()
        .flatten()
        .filter(|&n| n > 0)
        .min()
        .unwrap_or(1)
        .max(1)
}

pub fn detect_local_threads() -> usize {
    static CACHED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let avail = std::thread::available_parallelism().ok().map(|n| n.get());
        let cgroup = detect_cgroup_v2_cpu_limit();
        effective_threads(avail, cgroup, None)
    })
}

/// Derive local build concurrency from available threads and optional explicit `maxHeavy` / `heavyJobs`:
/// - `budget = max(1, threads - 2)`
/// - default `maxHeavy = max(2, budget / 12)` (memory/IO guard; CPU is governed by the jobserver pool)
/// - shared pool tokens = `budget - maxHeavy` (each cargo brings 1 implicit token)
/// - local per-command parallelism is dynamically fair-shared by the in-process proxy FIFO up to `budget`;
///   explicit `heavyJobs` caps each command at `heavyJobs` and marks `heavy_jobs_explicit = true`.
pub fn derive_concurrency(
    threads: usize,
    explicit_max_heavy: Option<usize>,
    explicit_heavy_jobs: Option<usize>,
) -> ConcurrencyPlan {
    let threads = threads.max(1);
    let budget = threads.saturating_sub(2).max(1);
    let default_max_heavy = (budget / 12).max(2);
    let (max_heavy, max_heavy_explicit) = match explicit_max_heavy {
        Some(n) => (n, true),
        None => (default_max_heavy, false),
    };
    let pool_tokens = crate::jobserver::compute_pool_tokens(budget, max_heavy, default_max_heavy);
    let (heavy_jobs, heavy_jobs_explicit) = match explicit_heavy_jobs {
        Some(n) => (n.max(1), true),
        None => (budget, false),
    };
    ConcurrencyPlan {
        threads,
        budget,
        max_heavy,
        pool_tokens,
        heavy_jobs,
        max_heavy_explicit,
        heavy_jobs_explicit,
        remote: false,
        fallback: false,
        diagnostic: None,
    }
}

/// Derive remote build concurrency (no FIFO/proxy on remote hosts: divides `budget / maxHeavy`
/// and injects `CARGO_BUILD_JOBS`).
pub fn derive_remote_concurrency(
    threads: usize,
    explicit_max_heavy: Option<usize>,
    explicit_heavy_jobs: Option<usize>,
) -> ConcurrencyPlan {
    let threads = threads.max(1);
    let budget = threads.saturating_sub(2).max(1);
    let default_max_heavy = (budget / 12).max(2);
    let (max_heavy, max_heavy_explicit) = match explicit_max_heavy {
        Some(n) => (n, true),
        None => (default_max_heavy, false),
    };
    let derived_jobs = budget
        .checked_div(max_heavy)
        .unwrap_or(budget / default_max_heavy)
        .max(1);
    let (heavy_jobs, heavy_jobs_explicit) = match explicit_heavy_jobs {
        Some(n) => (n.max(1), true),
        None => (derived_jobs, false),
    };
    ConcurrencyPlan {
        threads,
        budget,
        max_heavy,
        pool_tokens: 0,
        heavy_jobs,
        max_heavy_explicit,
        heavy_jobs_explicit,
        remote: true,
        fallback: false,
        diagnostic: None,
    }
}

pub fn default_local_max_heavy() -> usize {
    derive_concurrency(detect_local_threads(), None, None).max_heavy
}

pub fn local_plan_for_config(config: &Value) -> Res<ConcurrencyPlan> {
    let explicit_mh = crate::config::explicit_max_heavy(config)?;
    let explicit_hj = crate::config::explicit_heavy_jobs(config)?;
    Ok(derive_concurrency(detect_local_threads(), explicit_mh, explicit_hj))
}

pub fn local_plan(run: Option<&Path>) -> Res<ConcurrencyPlan> {
    let config = crate::config::context_capacity(run)?;
    local_plan_for_config(&config)
}

pub fn local_heavy_jobs(run: Option<&Path>) -> usize {
    local_plan(run)
        .map(|p| p.heavy_jobs)
        .unwrap_or_else(|_| derive_concurrency(detect_local_threads(), None, None).heavy_jobs)
}

pub fn max_heavy() -> Res<usize> {
    heavy_limit(None)
}
fn heavy_limit(run: Option<&Path>) -> Res<usize> {
    Ok(local_plan(run)?.max_heavy)
}
fn lane_dir() -> PathBuf {
    state_dir().join("lane")
}
fn remote_heavy_dir() -> PathBuf {
    state_dir().join("lane-remote")
}
fn remote_light_dir() -> PathBuf {
    state_dir().join("lane-remote-light")
}
fn held(p: &Path) -> bool {
    let Ok(f) = OpenOptions::new().read(true).open(p) else {
        return false;
    };
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) != 0 }
}
fn tickets_in(dir: &Path) -> Vec<(PathBuf, Value)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return vec![];
    };
    let mut out = vec![];
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "ticket") {
            if held(&p) {
                out.push((p.clone(), json(&p)));
            } else {
                let _ = fs::remove_file(&p);
            }
        }
    }
    // A resumed ticket may have an older timestamp, but must never displace a holder.
    out.sort_by(|a, z| b(&z.1, "running").cmp(&b(&a.1, "running")).then(a.0.cmp(&z.0)));
    out
}
fn tickets() -> Vec<(PathBuf, Value)> {
    tickets_in(&lane_dir())
}
pub fn remote_holders() -> Res<Vec<(String, String, bool)>> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let cfg = crate::config::load(git_top(&cwd).as_deref())?;
    let lim = crate::remote::remote_plan(&cfg.value["remote"], false).max_heavy;
    Ok(tickets_in(&remote_heavy_dir())
        .iter()
        .enumerate()
        .map(|(i, (_, v))| {
            (
                s(v, "label").into(),
                s(v, "since").into(),
                lim == 0 || i < lim,
            )
        })
        .collect())
}
pub fn remote_light_holders() -> Res<Vec<(String, String, bool)>> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let cfg = crate::config::load(git_top(&cwd).as_deref())?;
    let lim = crate::remote::max_light(&cfg.value["remote"]);
    Ok(tickets_in(&remote_light_dir())
        .iter()
        .enumerate()
        .map(|(i, (_, v))| {
            (
                s(v, "label").into(),
                s(v, "since").into(),
                lim == 0 || i < lim,
            )
        })
        .collect())
}
pub fn holders() -> Res<Vec<(String, String, bool)>> {
    let lim = max_heavy()?;
    Ok(tickets()
        .iter()
        .enumerate()
        .map(|(i, (_, v))| {
            (
                s(v, "label").into(),
                s(v, "since").into(),
                lim == 0 || i < lim,
            )
        })
        .collect())
}

/// Lock-free lookup of a run's active/queued lane ticket across remote-heavy, remote-light, and local queues.
pub fn run_active_lane(run: &Path) -> Option<Value> {
    let run_val = json!(run);
    let fname = run.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let matches_run = |v: &Value| -> bool {
        if v["run"] == run_val || v["context"] == run_val {
            return true;
        }
        if !fname.is_empty() {
            let lbl = s(v, "label");
            if lbl == format!("accept {fname}")
                || lbl == format!("verify {fname}")
                || lbl == format!("setup {fname}")
                || lbl == format!("regenerate {fname}")
            {
                return true;
            }
        }
        false
    };
    let cwd = std::env::current_dir().ok();
    let cfg = cwd
        .as_deref()
        .and_then(|d| crate::config::load(git_top_fast(d).as_deref()).ok());
    let heavy_lim = cfg
        .as_ref()
        .map(|c| crate::remote::remote_plan(&c.value["remote"], false).max_heavy)
        .unwrap_or(4);
    let light_lim = cfg
        .as_ref()
        .map(|c| crate::remote::max_light(&c.value["remote"]))
        .unwrap_or(crate::remote::DEFAULT_MAX_LIGHT);
    let local_lim = cfg
        .as_ref()
        .and_then(|c| local_plan_for_config(&c.value).ok())
        .map(|p| p.max_heavy)
        .unwrap_or_else(default_local_max_heavy);

    let mut found: Option<Value> = None;
    for (dir, kind, lim) in [
        (remote_heavy_dir(), LaneKind::RemoteHeavy, heavy_lim),
        (remote_light_dir(), LaneKind::RemoteLight, light_lim),
        (lane_dir(), LaneKind::Local, local_lim),
    ] {
        for (i, (_, v)) in tickets_in(&dir).into_iter().enumerate() {
            if matches_run(&v) {
                let running = b(&v, "running") || lim == 0 || i < lim;
                let raw_label = s(&v, "label");
                let cmd = match raw_label.split_once(": ") {
                    Some((prefix, rest))
                        if !rest.trim().is_empty()
                            && (prefix.starts_with('/') || prefix.contains("worktrees")) =>
                    {
                        rest.trim()
                    }
                    _ => raw_label.trim(),
                };
                let since = s(&v, "since");
                let elapsed_secs = v["epoch"]
                    .as_f64()
                    .map(|ep| (epoch() - ep).max(0.0) as i64)
                    .or_else(|| iso_epoch(since).map(|ep| (epoch() as i64 - ep).max(0)));
                let mut info = json!({
                    "queue": kind.as_str(),
                    "running": running,
                    "label": raw_label,
                    "cmd": cmd,
                });
                if !since.is_empty() {
                    info["since"] = json!(since);
                }
                if let Some(secs) = elapsed_secs {
                    info["elapsedSeconds"] = json!(secs);
                }
                if !running {
                    return Some(info);
                }
                if found.is_none() {
                    found = Some(info);
                }
            }
        }
    }
    found
}

/// Return the processes that currently hold a running heavy-lane slot.
///
/// The public lane listing intentionally keeps its small, human-facing tuple.
/// Measurement reservations need the ticket PID as well so they can move the
/// holder process tree without changing that listing's semantics.
pub fn holder_processes() -> Res<Vec<(String, i32)>> {
    let lim = max_heavy()?;
    Ok(tickets()
        .iter()
        .filter_map(|(_, value)| {
            if lim != 0 && !b(value, "running") {
                return None;
            }
            let pid = value["pid"].as_i64()?.try_into().ok()?;
            Some((s(value, "label").to_string(), pid))
        })
        .collect())
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChildRusage {
    pub cpu_user_secs: f64,
    pub cpu_sys_secs: f64,
    pub max_rss_kb: Option<u64>,
}

fn timeval_to_secs(tv: libc::timeval) -> f64 {
    (tv.tv_sec as f64) + (tv.tv_usec as f64) / 1_000_000.0
}

fn round_ms(secs: f64) -> f64 {
    if !secs.is_finite() || secs <= 0.0 {
        0.0
    } else {
        (secs * 1000.0).round() / 1000.0
    }
}

fn rusage_from_raw(ru: &libc::rusage) -> ChildRusage {
    let cpu_user_secs = timeval_to_secs(ru.ru_utime).max(0.0);
    let cpu_sys_secs = timeval_to_secs(ru.ru_stime).max(0.0);
    let raw_rss = ru.ru_maxrss.max(0) as u64;
    #[cfg(target_os = "macos")]
    let rss_kb = raw_rss / 1024;
    #[cfg(not(target_os = "macos"))]
    let rss_kb = raw_rss;
    ChildRusage {
        cpu_user_secs,
        cpu_sys_secs,
        max_rss_kb: if rss_kb > 0 { Some(rss_kb) } else { None },
    }
}

fn rusage_delta(start: ChildRusage, end: ChildRusage) -> ChildRusage {
    ChildRusage {
        cpu_user_secs: (end.cpu_user_secs - start.cpu_user_secs).max(0.0),
        cpu_sys_secs: (end.cpu_sys_secs - start.cpu_sys_secs).max(0.0),
        max_rss_kb: end.max_rss_kb.filter(|&kb| kb > 0),
    }
}

/// Snapshot cumulative rusage for all waited-for child processes (entire reaped subtree).
pub fn getrusage_children() -> Option<ChildRusage> {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) } == 0 {
        Some(rusage_from_raw(&ru))
    } else {
        None
    }
}

/// Non-blocking `wait4` for a specific child PID; returns raw wait status and subtree rusage.
pub fn try_wait4(pid: i32) -> io::Result<Option<(i32, Option<ChildRusage>)>> {
    let mut status: libc::c_int = 0;
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        let rc = unsafe { libc::wait4(pid, &mut status, libc::WNOHANG, &mut ru) };
        if rc == 0 {
            return Ok(None);
        }
        if rc > 0 {
            return Ok(Some((status, Some(rusage_from_raw(&ru)))));
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

/// Blocking `wait4` for a specific child PID; returns raw wait status and subtree rusage.
pub fn wait4_blocking(pid: i32) -> io::Result<(i32, Option<ChildRusage>)> {
    let mut status: libc::c_int = 0;
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        let rc = unsafe { libc::wait4(pid, &mut status, 0, &mut ru) };
        if rc > 0 {
            return Ok((status, Some(rusage_from_raw(&ru))));
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

pub fn wait_status_exit_code(status: i32) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

pub fn format_cpu_summary(wall_secs: f64, user: Option<f64>, sys: Option<f64>) -> Option<String> {
    let (u, s) = (user?, sys?);
    let total = (u + s).max(0.0);
    let wall = wall_secs.max(0.0);
    let ratio = if wall > 0.0005 {
        format!("{:.2}x", total / wall)
    } else {
        "—".to_string()
    };
    Some(format!(
        "CPU user {u:.2}s / sys {s:.2}s · 墙钟 {wall:.2}s · CPU/墙钟 {ratio}"
    ))
}

pub struct Slot {
    ticket: Option<PathBuf>,
    _file: Option<File>,
    proxy: Option<crate::jobserver::ProxyGuard>,
    pushed_env: bool,
    pushed_kind: bool,
    pub queued: f64,
    acquired_at: Option<Instant>,
    rusage_start: Option<ChildRusage>,
    recorded_rusage: Option<ChildRusage>,
    event_run: String,
    event_repo: String,
    event_label: String,
    event_kind: LaneKind,
}
impl Slot {
    pub fn env_vars(&self) -> Option<Vec<(String, String)>> {
        self.proxy.as_ref().map(|p| p.env_vars())
    }
    pub fn set_rusage(&mut self, rusage: Option<ChildRusage>) {
        self.recorded_rusage = rusage;
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        if self.pushed_kind {
            SLOT_KIND_STACK.with(|s| {
                s.borrow_mut().pop();
            });
            self.pushed_kind = false;
        }
        if self.pushed_env {
            SLOT_ENV_STACK.with(|s| {
                s.borrow_mut().pop();
            });
            self.pushed_env = false;
        }
        // Stop proxy & return tokens before removing the lane ticket so pool state stays consistent.
        self.proxy.take();
        if let Some(p) = self.ticket.take() {
            let _ = fs::remove_file(p);
            if let Some(at) = self.acquired_at.take() {
                let held = (at.elapsed().as_secs_f64() * 100.0).round() / 100.0;
                let usage = self.recorded_rusage.or_else(|| {
                    let start = self.rusage_start?;
                    let end = getrusage_children()?;
                    Some(rusage_delta(start, end))
                });
                let (cpu_user, cpu_sys, max_rss_kb) = match usage {
                    Some(u) => (
                        json!(round_ms(u.cpu_user_secs)),
                        json!(round_ms(u.cpu_sys_secs)),
                        u.max_rss_kb.map_or(Value::Null, |kb| json!(kb)),
                    ),
                    None => (Value::Null, Value::Null, Value::Null),
                };
                crate::events::emit(
                    &self.event_run,
                    &self.event_repo,
                    "lane_released",
                    json!({
                        "label": self.event_label,
                        "heldSeconds": held,
                        "remote": self.event_kind.is_remote(),
                        "lane": self.event_kind.as_str(),
                        "cpuUserSecs": cpu_user,
                        "cpuSysSecs": cpu_sys,
                        "maxRssKb": max_rss_kb,
                    }),
                );
            }
        }
    }
}
fn block_on(p: &Path, cancel: Option<&AtomicBool>) {
    let Ok(f) = OpenOptions::new().read(true).open(p) else {
        return;
    };
    if let Some(flag) = cancel {
        loop {
            if flag.load(Ordering::SeqCst) || stopped() {
                return;
            }
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    } else {
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH) };
    }
}
pub fn acquire(
    label: &str,
    account: Option<&Path>,
    context: Option<&Path>,
    waiting: impl FnMut(usize, &[String]),
) -> Res<Slot> {
    acquire_in(
        &lane_dir(),
        heavy_limit(context)?,
        label,
        account,
        context,
        LaneKind::Local,
        None,
        waiting,
    )
}
pub fn acquire_remote(
    label: &str,
    account: Option<&Path>,
    remote: &Value,
    waiting: impl FnMut(usize, &[String]),
) -> Res<Slot> {
    let plan = crate::remote::remote_plan(remote, true);
    acquire_in(
        &remote_heavy_dir(),
        plan.max_heavy,
        label,
        account,
        account,
        LaneKind::RemoteHeavy,
        None,
        waiting,
    )
}
pub fn acquire_remote_light(
    label: &str,
    account: Option<&Path>,
    remote: &Value,
    waiting: impl FnMut(usize, &[String]),
) -> Res<Slot> {
    let lim = crate::remote::max_light(remote);
    acquire_in(
        &remote_light_dir(),
        lim,
        label,
        account,
        account,
        LaneKind::RemoteLight,
        None,
        waiting,
    )
}
/// Shell steps use the remote lane (heavy or light), while the public `lane` command stays local.
pub fn acquire_shell(
    label: &str,
    account: Option<&Path>,
    context: Option<&Path>,
    waiting: impl FnMut(usize, &[String]),
) -> Res<Slot> {
    acquire_shell_for_commands_cancellable(label, &[], account, context, None, waiting)
}
pub fn acquire_shell_for_commands(
    label: &str,
    commands: &[&str],
    account: Option<&Path>,
    context: Option<&Path>,
    waiting: impl FnMut(usize, &[String]),
) -> Res<Slot> {
    acquire_shell_for_commands_cancellable(label, commands, account, context, None, waiting)
}
pub fn acquire_shell_for_commands_cancellable(
    label: &str,
    commands: &[&str],
    account: Option<&Path>,
    context: Option<&Path>,
    cancel: Option<&AtomicBool>,
    waiting: impl FnMut(usize, &[String]),
) -> Res<Slot> {
    if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
        return Err("cancelled while queued for the heavy lane".into());
    }
    let meta = context
        .map(|r| json(r.join("meta.json")))
        .unwrap_or(Value::Null);
    if crate::remote::enabled(&meta["remote"]) {
        if crate::remote::shell_commands_are_light(&meta["remote"], commands) {
            let lim = crate::remote::max_light(&meta["remote"]);
            acquire_in(
                &remote_light_dir(),
                lim,
                label,
                account.or(context),
                account.or(context),
                LaneKind::RemoteLight,
                cancel,
                waiting,
            )
        } else {
            let plan = crate::remote::remote_plan(&meta["remote"], true);
            acquire_in(
                &remote_heavy_dir(),
                plan.max_heavy,
                label,
                account.or(context),
                account.or(context),
                LaneKind::RemoteHeavy,
                cancel,
                waiting,
            )
        }
    } else {
        acquire_in(
            &lane_dir(),
            heavy_limit(context)?,
            label,
            account,
            context,
            LaneKind::Local,
            cancel,
            waiting,
        )
    }
}
fn run_started_ns_for(run: Option<&Path>, fallback_ns: u64) -> u64 {
    let Some(r) = run else {
        return fallback_ns;
    };
    let meta = json(r.join("meta.json"));
    if let Some(ns) = meta["startedNs"].as_u64().filter(|&n| n > 0) {
        return ns;
    }
    if let Some(ep) = meta["startedEpoch"].as_u64().filter(|&n| n > 0) {
        return ep.saturating_mul(1_000_000_000);
    }
    fallback_ns
}

fn start_local_proxy(
    label: &str,
    context: Option<&Path>,
    run_started_ns: u64,
) -> (Option<crate::jobserver::ProxyGuard>, bool) {
    if std::env::var_os("DELEGATE_LANE_HELD").is_some() {
        return (None, false);
    }
    let Ok(plan) = local_plan(context) else {
        return (None, false);
    };
    let explicit_cap = if plan.heavy_jobs_explicit {
        Some(plan.heavy_jobs)
    } else {
        None
    };
    match crate::jobserver::ProxyGuard::start(
        state_dir(),
        plan.pool_tokens,
        plan.budget,
        run_started_ns,
        label.to_string(),
        explicit_cap,
    ) {
        Ok(proxy) => {
            let vars = proxy.env_vars();
            SLOT_ENV_STACK.with(|s| s.borrow_mut().push(vars));
            (Some(proxy), true)
        }
        Err(_) => (None, false),
    }
}

#[allow(clippy::too_many_arguments)]
fn acquire_in(
    dir: &Path,
    limit: usize,
    label: &str,
    account: Option<&Path>,
    context: Option<&Path>,
    kind: LaneKind,
    cancel: Option<&AtomicBool>,
    mut waiting: impl FnMut(usize, &[String]),
) -> Res<Slot> {
    let remote = kind.is_remote();
    if already_held_for(kind) {
        return Ok(Slot {
            ticket: None,
            _file: None,
            proxy: None,
            pushed_env: false,
            pushed_kind: false,
            queued: 0.0,
            acquired_at: None,
            rusage_start: None,
            recorded_rusage: None,
            event_run: String::new(),
            event_repo: String::new(),
            event_label: String::new(),
            event_kind: kind,
        });
    }
    let event_run_path = account.or(context);
    let stamp_now = now_ns() as u64;
    let run_started_ns = run_started_ns_for(event_run_path, stamp_now);
    if limit == 0 {
        let (proxy, pushed_env) = if !remote {
            start_local_proxy(label, context, run_started_ns)
        } else {
            (None, false)
        };
        SLOT_KIND_STACK.with(|s| s.borrow_mut().push(kind));
        return Ok(Slot {
            ticket: None,
            _file: None,
            proxy,
            pushed_env,
            pushed_kind: true,
            queued: 0.0,
            acquired_at: None,
            rusage_start: None,
            recorded_rusage: None,
            event_run: String::new(),
            event_repo: String::new(),
            event_label: String::new(),
            event_kind: kind,
        });
    }
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let event_run = event_run_path
        .map(|r| crate::events::run_id_for(r, &json(r.join("meta.json"))))
        .unwrap_or_default();
    let event_repo = event_run_path
        .map(crate::events::repo_for_run)
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|cwd| git_top(&cwd))
                .map(|p| fs::canonicalize(&p).unwrap_or(p).to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    let priority = account.map(|r| json(r.join("meta.json"))["lanePriority"][label].as_u64().unwrap_or(0)).unwrap_or(0);
    let stamp = if priority == 0 { stamp_now } else { priority };
    let mut me = json!({
        "pid": std::process::id(),
        "label": label,
        "lane": kind.as_str(),
        "since": iso(),
        "epoch": epoch(),
        "queuedNs": stamp,
        "runStartedNs": run_started_ns,
        "run": account,
        "context": context,
        "running": false,
    });
    let ticket;
    let mut file;
    {
        let _guard = crate::locks::acquire(crate::locks::LockKind::Lane, &dir.join(".lock"), true).map_err(|e| e.to_string())?;
        ticket = dir.join(format!("{stamp:020}-{}.ticket", std::process::id()));
        file = locked_file(&ticket, &me.to_string())?;
    }
    let marker = account.map(|p| p.join(format!("lane-waiting-{}", std::process::id())));
    let markfile = marker
        .as_ref()
        .map(|m| locked_file(m, &me.to_string()))
        .transpose()?;
    let started = Instant::now();
    let mut emitted_queued = false;
    let outcome = (|| {
        loop {
            let guard = crate::locks::acquire(crate::locks::LockKind::Lane, &dir.join(".lock"), true).map_err(|e| e.to_string())?;
            let live = tickets_in(dir);
            let pos = live.iter().position(|(p, _)| p == &ticket).unwrap_or(0);
            if pos < limit {
                use std::io::{Seek, SeekFrom, Write};
                me["running"] = json!(true);
                file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
                file.set_len(0).map_err(|e| e.to_string())?;
                file.write_all(me.to_string().as_bytes())
                    .map_err(|e| e.to_string())?;
                let _ = file.sync_data();
                break;
            }
            drop(guard);
            let ahead = pos - limit + 1;
            if !emitted_queued {
                emitted_queued = true;
                crate::events::emit(
                    &event_run,
                    &event_repo,
                    "queued",
                    json!({
                        "scope": "lane",
                        "label": label,
                        "ahead": ahead,
                        "remote": remote,
                        "lane": kind.as_str(),
                    }),
                );
            }
            let labels: Vec<String> = live[..pos]
                .iter()
                .map(|(_, v)| s(v, "label").to_string())
                .collect();
            waiting(ahead, &labels);
            LANE_WAITING.store(true, Ordering::SeqCst);
            if stopped() {
                LANE_WAITING.store(false, Ordering::SeqCst);
                return Err("stopped while queued for the heavy lane".into());
            }
            if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
                LANE_WAITING.store(false, Ordering::SeqCst);
                return Err("cancelled while queued for the heavy lane".into());
            }
            block_on(&live[pos - limit].0, cancel);
            LANE_WAITING.store(false, Ordering::SeqCst);
            if stopped() {
                return Err("stopped while queued for the heavy lane".into());
            }
            if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
                return Err("cancelled while queued for the heavy lane".into());
            }
        }
        Ok(())
    })();
    let queued = started.elapsed().as_secs_f64();
    // Persist before removing the live marker, so timeout readers never see a gap.
    if let Some(a) = account {
        let _ = append(a.join("lane-wait"), &format!("{queued:.3}\n"));
    }
    if let Some(p) = marker {
        let _ = fs::remove_file(p);
    }
    drop(markfile);
    if let Err(e) = outcome {
        let _ = fs::remove_file(ticket);
        return Err(e);
    }
    let (proxy, pushed_env) = if !remote {
        start_local_proxy(label, context, run_started_ns)
    } else {
        (None, false)
    };
    SLOT_KIND_STACK.with(|s| s.borrow_mut().push(kind));
    crate::events::emit(
        &event_run,
        &event_repo,
        "lane_acquired",
        json!({
            "label": label,
            "queuedSeconds": (queued * 100.0).round() / 100.0,
            "remote": remote,
            "lane": kind.as_str(),
        }),
    );
    Ok(Slot {
        ticket: Some(ticket),
        _file: Some(file),
        proxy,
        pushed_env,
        pushed_kind: true,
        queued,
        acquired_at: Some(Instant::now()),
        rusage_start: getrusage_children(),
        recorded_rusage: None,
        event_run,
        event_repo,
        event_label: label.to_string(),
        event_kind: kind,
    })
}
/// Keep only queued jobs, not processes: question cleanup still reclaims every child.
pub fn save_priority(run: &Path) -> Res<()> {
    let mut priority = json!({});
    for dir in [lane_dir(), remote_heavy_dir(), remote_light_dir()] {
        if !dir.is_dir() { continue; }
        let _guard = crate::locks::acquire(crate::locks::LockKind::Lane, &dir.join(".lock"), true).map_err(|e| e.to_string())?;
        for (_, ticket) in tickets_in(&dir) {
            if ticket["run"] == json!(run) && !b(&ticket, "running") {
                let label = s(&ticket, "label");
                let stamp = ticket["queuedNs"].as_u64().unwrap_or(0);
                let old = priority[label].as_u64().unwrap_or(u64::MAX);
                priority[label] = json!(old.min(stamp));
            }
        }
    }
    write_json(run.join("lane-priority.json"), &priority)
}
pub fn queued_seconds(run: &Path) -> f64 {
    let mut total = read(run.join("lane-wait"))
        .split_whitespace()
        .filter_map(|x| x.parse::<f64>().ok())
        .sum();
    total += crate::measure::queued_seconds(run);
    if let Ok(entries) = fs::read_dir(run) {
        for e in entries.flatten() {
            let p = e.path();
            if p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .starts_with("lane-waiting-")
                && held(&p)
            {
                let v = json(&p);
                total += (epoch() - v["epoch"].as_f64().unwrap_or(epoch())).max(0.0);
            }
        }
    }
    total
}
/// Zero-lock, zero-side-effect JSON snapshot of local + remote lane & jobserver pool state.
pub fn status_json_readonly() -> Res<Value> {
    let remote = remote_holders().unwrap_or_default();
    let remote_light = remote_light_holders().unwrap_or_default();
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let cfg = crate::config::load(git_top_fast(&cwd).as_deref())?;
    let plan = local_plan_for_config(&cfg.value)?;
    let pool = crate::jobserver::inspect_pool_readonly(plan.pool_tokens, plan.budget);
    let lim = plan.max_heavy;
    let local_tickets: Vec<(u32, String, String, bool)> = tickets()
        .iter()
        .enumerate()
        .map(|(i, (_, v))| {
            (
                v["pid"].as_u64().unwrap_or(0) as u32,
                s(v, "label").to_string(),
                s(v, "since").to_string(),
                lim == 0 || i < lim,
            )
        })
        .collect();
    let match_lease = |pid: u32, lbl: &str| -> Option<&crate::jobserver::LeaseView> {
        pool.leases
            .iter()
            .find(|l| pid != 0 && l.pid == pid)
            .or_else(|| pool.leases.iter().find(|l| l.label == lbl))
    };
    let remote_enabled = crate::remote::enabled(&cfg.value["remote"]);
    let remote_plan = if remote_enabled {
        Some(crate::remote::remote_plan(&cfg.value["remote"], false))
    } else {
        None
    };
    let remote_limit = remote_plan
        .as_ref()
        .map(|p| p.max_heavy)
        .unwrap_or_else(|| cfg.value["remote"]["maxHeavy"].as_u64().unwrap_or(4) as usize);
    let remote_light_limit = crate::remote::max_light(&cfg.value["remote"]);
    let local_items: Vec<Value> = local_tickets
        .into_iter()
        .map(|(pid, label, since, running)| {
            let mut item = json!({"label": label, "since": since, "running": running, "lane": "local"});
            if running {
                if let Some(lease) = match_lease(pid, &label) {
                    item["rank"] = json!(lease.rank);
                    item["borrowed"] = json!(lease.borrowed);
                    item["used"] = json!(lease.used_jobs());
                    item["share"] = json!(lease.share);
                }
            }
            item
        })
        .collect();
    let remote_items: Vec<Value> = remote
        .into_iter()
        .map(|(label, since, running)| {
            json!({"label": label, "since": since, "running": running, "lane": "remote-heavy"})
        })
        .collect();
    let remote_light_items: Vec<Value> = remote_light
        .into_iter()
        .map(|(label, since, running)| {
            json!({"label": label, "since": since, "running": running, "lane": "remote-light"})
        })
        .collect();
    let mut out = json!({
        "maxHeavy": plan.max_heavy,
        "heavyJobs": plan.heavy_jobs,
        "threads": plan.threads,
        "budget": plan.budget,
        "poolTokens": plan.pool_tokens,
        "maxHeavyExplicit": plan.max_heavy_explicit,
        "heavyJobsExplicit": plan.heavy_jobs_explicit,
        "concurrencySummary": plan.format_summary_with_pool(&pool),
        "leasesSummary": pool.format_leases_strip(),
        "pool": pool.to_json(),
        "concurrency": plan.to_json_with_pool(&pool),
        "local": local_items,
        "remoteMaxHeavy": remote_limit,
        "remote": remote_items,
        "remoteMaxLight": remote_light_limit,
        "remoteLight": remote_light_items,
    });
    if let Some(rp) = remote_plan {
        out["remoteConcurrency"] = rp.to_json();
    }
    Ok(out)
}

pub fn lane_command(args: &[String]) -> Res<i32> {
    let mut i = 0;
    let mut label = None;
    let mut measure = false;
    let mut json_mode = false;
    while i < args.len() {
        if args[i] == "--label" {
            i += 1;
            if i >= args.len() {
                return Err("argument --label: expected one argument".into());
            }
            label = Some(args[i].clone());
            i += 1;
        } else if args[i] == "--measure" {
            measure = true;
            i += 1;
        } else if args[i] == "--json" {
            json_mode = true;
            i += 1;
        } else {
            break;
        }
    }
    let words = if args.get(i).is_some_and(|x| x == "--") {
        &args[i + 1..]
    } else {
        &args[i..]
    };
    if words.is_empty() {
        let remote = remote_holders()?;
        let remote_light = remote_light_holders()?;
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let cfg = crate::config::load(git_top(&cwd).as_deref())?;
        let plan = local_plan_for_config(&cfg.value)?;
        let pool = crate::jobserver::reconcile_pool(plan.pool_tokens, plan.budget);
        let lim = plan.max_heavy;
        let local_tickets: Vec<(u32, String, String, bool)> = tickets()
            .iter()
            .enumerate()
            .map(|(i, (_, v))| {
                (
                    v["pid"].as_u64().unwrap_or(0) as u32,
                    s(v, "label").to_string(),
                    s(v, "since").to_string(),
                    lim == 0 || i < lim,
                )
            })
            .collect();
        let match_lease = |pid: u32, lbl: &str| -> Option<&crate::jobserver::LeaseView> {
            pool.leases
                .iter()
                .find(|l| pid != 0 && l.pid == pid)
                .or_else(|| pool.leases.iter().find(|l| l.label == lbl))
        };
        let remote_enabled = crate::remote::enabled(&cfg.value["remote"]);
        let remote_plan = if remote_enabled {
            Some(crate::remote::remote_plan(&cfg.value["remote"], false))
        } else {
            None
        };
        let remote_limit = remote_plan
            .as_ref()
            .map(|p| p.max_heavy)
            .unwrap_or_else(|| cfg.value["remote"]["maxHeavy"].as_u64().unwrap_or(4) as usize);
        let remote_light_limit = crate::remote::max_light(&cfg.value["remote"]);
        if json_mode {
            let local_items: Vec<Value> = local_tickets
                .into_iter()
                .map(|(pid, label, since, running)| {
                    let mut item = json!({"label": label, "since": since, "running": running, "lane": "local"});
                    if running {
                        if let Some(lease) = match_lease(pid, &label) {
                            item["rank"] = json!(lease.rank);
                            item["borrowed"] = json!(lease.borrowed);
                            item["used"] = json!(lease.used_jobs());
                            item["share"] = json!(lease.share);
                        }
                    }
                    item
                })
                .collect();
            let remote_items: Vec<Value> = remote
                .into_iter()
                .map(|(label, since, running)| {
                    json!({"label": label, "since": since, "running": running, "lane": "remote-heavy"})
                })
                .collect();
            let remote_light_items: Vec<Value> = remote_light
                .into_iter()
                .map(|(label, since, running)| {
                    json!({"label": label, "since": since, "running": running, "lane": "remote-light"})
                })
                .collect();
            let mut out = json!({
                "maxHeavy": plan.max_heavy,
                "heavyJobs": plan.heavy_jobs,
                "threads": plan.threads,
                "budget": plan.budget,
                "poolTokens": plan.pool_tokens,
                "maxHeavyExplicit": plan.max_heavy_explicit,
                "heavyJobsExplicit": plan.heavy_jobs_explicit,
                "concurrencySummary": plan.format_summary_with_pool(&pool),
                "leasesSummary": pool.format_leases_strip(),
                "pool": pool.to_json(),
                "concurrency": plan.to_json_with_pool(&pool),
                "local": local_items,
                "remoteMaxHeavy": remote_limit,
                "remote": remote_items,
                "remoteMaxLight": remote_light_limit,
                "remoteLight": remote_light_items,
            });
            if let Some(rp) = remote_plan {
                out["remoteConcurrency"] = rp.to_json();
            }
            println!("{out}");
            return Ok(0);
        }
        println!("{}", plan.format_summary_with_pool(&pool));
        if let Some(rp) = remote_plan {
            let has_light = !crate::remote::light_patterns(&cfg.value["remote"]).is_empty()
                || !remote_light.is_empty();
            if has_light {
                let light_str = if remote_light_limit == 0 {
                    "不限".to_string()
                } else {
                    format!("{remote_light_limit} 条")
                };
                println!("remote {} · 轻队列 {light_str}", rp.format_summary());
            } else {
                println!("remote {}", rp.format_summary());
            }
        }
        for (label, since, running) in remote {
            println!(
                "remote {:<8} {since}  [重队列]  {label}",
                if running { "running" } else { "queued" }
            );
        }
        for (label, since, running) in remote_light {
            println!(
                "remote {:<8} {since}  [轻队列]  {label}",
                if running { "running" } else { "queued" }
            );
        }
        for (pid, label, since, running) in local_tickets {
            if running {
                if let Some(lease) = match_lease(pid, &label) {
                    println!(
                        "{:<8} {since}  [本机]  #{} {}/{}  {label}",
                        "running",
                        lease.rank,
                        lease.used_jobs(),
                        lease.share
                    );
                    continue;
                }
            }
            println!(
                "{:<8} {since}  [本机]  {label}",
                if running { "running" } else { "queued" }
            );
        }
        return Ok(0);
    }
    if json_mode {
        return Err("--json only applies when listing the lane".into());
    }
    let command = if words.len() == 1 {
        words[0].clone()
    } else {
        words
            .iter()
            .map(|x| shell_quote(x))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let label = label.unwrap_or_else(|| {
        format!(
            "{}: {}",
            std::env::current_dir()
                .unwrap_or_default()
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            clip(&command, 80)
        )
    });
    install_signals();
    let mut shown = false;
    let account = std::env::var_os("DELEGATE_RUN_DIR").map(PathBuf::from);
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    if measure {
        let config = crate::config::load(git_top(&cwd).as_deref())?;
        let meta = json!({"measure":config.value["measure"],"measureCpus":config.value["measureCpus"],"workdir":cwd});
        return crate::measure::run_lane(&command, &cwd, account.as_deref(), &meta);
    }
    let mut slot = acquire(
        &label,
        account.as_deref(),
        account.as_deref(),
        |ahead, labels| {
            if !shown {
                eprintln!(
                    "delegate lane: queued behind {ahead}: {}",
                    labels.join("; ")
                );
                shown = true;
            }
        },
    )?;
    if shown {
        eprintln!(
            "delegate lane: started after {:.0}s in the queue",
            slot.queued
        );
    }
    let mut c = if let Some(cpus) = crate::measure::remaining_cpus() {
        let mut c = Command::new("taskset");
        c.args(["-c", &cpus, "sh", "-c", &command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(&command);
        c
    };
    let temporary = if account.is_none() {
        let dir = state_dir().join(format!("lane-resource-{}-{}", std::process::id(), now_ns()));
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        Some(dir)
    } else {
        None
    };
    if let Some(dir) = &temporary {
        let config = crate::config::load(git_top(&cwd).as_deref())?;
        let meta = json!({"resources":config.value["resources"],"measure":config.value["measure"],"measureCpus":config.value["measureCpus"],"agentDeny":[],"workdir":cwd,"env":config.value["env"]});
        write_json(dir.join("meta.json"), &meta)?;
        crate::deny::prepare(&meta, dir)?;
        if let Some(path) = crate::deny::resource_path(&meta, dir)? {
            c.env("PATH", path);
        }
    }
    let plan = local_plan(account.as_deref())?;
    c.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env("DELEGATE_LANE_HELD", "1");
    if let Some(vars) = slot.env_vars() {
        c.env_remove("CARGO_BUILD_JOBS")
            .env_remove("MAKEFLAGS")
            .env_remove("CARGO_MAKEFLAGS");
        for (k, v) in vars {
            c.env(k, v);
        }
    } else if std::env::var_os("DELEGATE_LANE_HELD").is_none() {
        c.env("CARGO_BUILD_JOBS", plan.heavy_jobs.to_string());
    }
    group(&mut c);
    let child = c.spawn().map_err(|e| e.to_string())?;
    let pid = child.id() as i32;
    let mut stall_monitor = LaneStallMonitor::new(
        pid as u32,
        account.as_deref(),
        &slot.event_repo,
        &command,
    );
    loop {
        if let Some((status, rusage)) = try_wait4(pid).map_err(|e| e.to_string())? {
            drop(stall_monitor);
            slot.set_rusage(rusage);
            let sig = SIGNAL.load(Ordering::SeqCst);
            if let Some(dir) = &temporary {
                let _ = fs::remove_dir_all(dir);
            }
            return Ok(if sig != 0 {
                128 + sig
            } else {
                wait_status_exit_code(status)
            });
        }
        let sig = SIGNAL.load(Ordering::SeqCst);
        if sig != 0 {
            drop(stall_monitor);
            end_group(pid, 3.0);
            if let Ok((_, rusage)) = wait4_blocking(pid) {
                slot.set_rusage(rusage);
            }
            if let Some(dir) = &temporary {
                let _ = fs::remove_dir_all(dir);
            }
            return Ok(128 + sig);
        }
        if let Some(monitor) = stall_monitor.as_mut() {
            monitor.poll(pid as u32);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

const LANE_STALL_MIN_PREFIX_SAMPLES: usize = 3;
const LANE_STALL_DEFAULT_MULTIPLIER: f64 = 2.0;
const LANE_STALL_DEFAULT_WINDOW_SECS: f64 = 60.0;
const LANE_STALL_CPU_RATIO_LIMIT: f64 = 0.01;

fn is_env_assignment(tok: &str) -> bool {
    let Some((k, _)) = tok.split_once('=') else {
        return false;
    };
    !k.is_empty()
        && k.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn token_basename(tok: &str) -> &str {
    let trimmed = tok.trim_matches(|c| c == '\'' || c == '"');
    if trimmed.contains('/') {
        Path::new(trimmed)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(trimmed)
    } else {
        trimmed
    }
}

/// Normalize a `lane_released` label or raw shell command into a stable command prefix
/// (e.g. `"norvane: cd foo && cargo test -p xtask"` -> `"cargo test"`).
pub(crate) fn normalize_command_prefix(raw: &str) -> String {
    let mut text = raw.trim();
    if let Some((prefix, rest)) = text.split_once(": ") {
        if !prefix.is_empty() && !prefix.chars().any(char::is_whitespace) {
            text = rest.trim();
        }
    }
    let mut chosen_tokens: Vec<String> = Vec::new();
    for seg in text.split(&[';', '|'][..]).flat_map(|s| s.split("&&")) {
        let mut iter = seg.split_whitespace().peekable();
        while let Some(&tok) = iter.peek() {
            if is_env_assignment(tok) {
                iter.next();
            } else {
                break;
            }
        }
        let Some(first_raw) = iter.next() else {
            continue;
        };
        let first = token_basename(first_raw);
        if matches!(first, "cd" | "export" | "set" | "true" | ":" | "unset") {
            continue;
        }
        chosen_tokens.push(first.to_string());
        if let Some(second_raw) = iter.next() {
            let second = second_raw.trim_matches(|c| c == '\'' || c == '"');
            if !second.is_empty() && !second.starts_with('-') && !is_env_assignment(second) {
                chosen_tokens.push(token_basename(second).to_string());
            }
        }
        break;
    }
    chosen_tokens.join(" ")
}

fn percentile_75(samples: &mut [f64]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_by(f64::total_cmp);
    if samples.len() == 1 {
        return Some(samples[0]);
    }
    let pos = (samples.len() - 1) as f64 * 0.75;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let frac = pos - lo as f64;
    Some(samples[lo] + (samples[hi] - samples[lo]) * frac)
}

fn lane_stall_multiplier() -> Option<f64> {
    let raw = setting("LANE_STALL_MULTIPLIER", "2");
    if raw.trim() == "0" {
        return None;
    }
    Some(
        raw.trim()
            .parse::<f64>()
            .ok()
            .filter(|&v| v > 0.0 && v.is_finite())
            .unwrap_or(LANE_STALL_DEFAULT_MULTIPLIER),
    )
}

fn lane_stall_window_secs() -> f64 {
    let raw = setting("LANE_STALL_WINDOW", "60");
    raw.trim()
        .parse::<f64>()
        .ok()
        .or_else(|| seconds(&raw).ok())
        .filter(|&v| v > 0.0 && v.is_finite())
        .unwrap_or(LANE_STALL_DEFAULT_WINDOW_SECS)
}

pub(crate) fn lane_stall_threshold_secs(
    events_path: &Path,
    repo: &str,
    command: &str,
    meta: &Value,
) -> Option<f64> {
    let multiplier = lane_stall_multiplier()?;
    let target_prefix = normalize_command_prefix(command);
    let mut prefix_samples = Vec::new();
    let mut repo_samples = Vec::new();
    if let Ok(f) = File::open(events_path) {
        let reader = BufReader::new(f);
        for line in reader.lines().map_while(Result::ok) {
            if !line.contains("\"lane_released\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if s(&v, "kind") != "lane_released" {
                continue;
            }
            if !repo.is_empty() && s(&v, "repo") != repo {
                continue;
            }
            let Some(held) = v["payload"]["heldSeconds"]
                .as_f64()
                .filter(|&h| h > 0.0 && h.is_finite())
            else {
                continue;
            };
            repo_samples.push(held);
            if !target_prefix.is_empty()
                && normalize_command_prefix(s(&v["payload"], "label")) == target_prefix
            {
                prefix_samples.push(held);
            }
        }
    }
    let mut chosen = if prefix_samples.len() >= LANE_STALL_MIN_PREFIX_SAMPLES {
        prefix_samples
    } else if !repo_samples.is_empty() {
        repo_samples
    } else {
        return crate::agents::stall_seconds(meta, "accept");
    };
    let p75 = percentile_75(&mut chosen)?;
    Some(p75 * multiplier)
}

#[derive(Debug, Clone)]
struct DeepestProc {
    pid: u32,
    comm: String,
    wchan: String,
}

#[derive(Debug, Clone)]
struct ProcessTreeSample {
    total_cpu_ticks: u64,
    deepest: DeepestProc,
}

fn clk_tck() -> f64 {
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 { v as f64 } else { 100.0 }
}

fn parse_proc_stat_cpu(txt: &str) -> Option<(char, u32, u64)> {
    let rparen = txt.rfind(')')?;
    let fields: Vec<&str> = txt.get(rparen + 2..)?.split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let ppid = fields.get(1)?.parse::<u32>().ok()?;
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    let cutime = fields.get(13)?.parse::<u64>().unwrap_or(0);
    let cstime = fields.get(14)?.parse::<u64>().unwrap_or(0);
    Some((
        state,
        ppid,
        utime
            .saturating_add(stime)
            .saturating_add(cutime)
            .saturating_add(cstime),
    ))
}

fn read_proc_comm_wchan(pid: u32) -> (String, String) {
    let comm = fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let wchan = fs::read_to_string(format!("/proc/{pid}/wchan"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if !wchan.is_empty() && wchan != "0" {
        return (if comm.is_empty() { "?".into() } else { comm }, wchan);
    }
    if let Ok(rd) = fs::read_dir(format!("/proc/{pid}/task")) {
        for ent in rd.flatten() {
            let tid_path = ent.path();
            let tw = fs::read_to_string(tid_path.join("wchan"))
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if !tw.is_empty() && tw != "0" {
                let tc = fs::read_to_string(tid_path.join("comm"))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                let final_comm = if !tc.is_empty() {
                    tc
                } else if !comm.is_empty() {
                    comm
                } else {
                    "?".into()
                };
                return (final_comm, tw);
            }
        }
    }
    (
        if comm.is_empty() { "?".into() } else { comm },
        if wchan.is_empty() { "-".into() } else { wchan },
    )
}

fn inspect_process_tree(root_pid: u32) -> Option<ProcessTreeSample> {
    let mut info: HashMap<u32, (char, u64)> = HashMap::new();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let Ok(rd) = fs::read_dir("/proc") else {
        return None;
    };
    for ent in rd.flatten() {
        let Some(name) = ent.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some((state, ppid, ticks)) = parse_proc_stat_cpu(&stat) else {
            continue;
        };
        info.insert(pid, (state, ticks));
        children.entry(ppid).or_default().push(pid);
    }
    if !info.contains_key(&root_pid) {
        return None;
    }
    let mut total_cpu_ticks = 0u64;
    let mut best: Option<(bool, usize, bool, u32)> = None;
    let mut queue = VecDeque::new();
    queue.push_back((root_pid, 0usize));
    while let Some((pid, depth)) = queue.pop_front() {
        if let Some(&(state, ticks)) = info.get(&pid) {
            total_cpu_ticks = total_cpu_ticks.saturating_add(ticks);
            let non_zombie = state != 'Z';
            let blocked = matches!(state, 'S' | 'D');
            let key = (non_zombie, depth, blocked, pid);
            if best.is_none_or(|b| key > b) {
                best = Some(key);
            }
        }
        if let Some(kids) = children.get(&pid) {
            for &kid in kids {
                queue.push_back((kid, depth + 1));
            }
        }
    }
    let deepest_pid = best.map(|b| b.3).unwrap_or(root_pid);
    let (comm, wchan) = read_proc_comm_wchan(deepest_pid);
    Some(ProcessTreeSample {
        total_cpu_ticks,
        deepest: DeepestProc {
            pid: deepest_pid,
            comm,
            wchan,
        },
    })
}

struct LaneStallMonitor {
    started: Instant,
    threshold_secs: f64,
    window_secs: f64,
    sample_interval: Duration,
    clk_tck: f64,
    last_sample: Instant,
    history: VecDeque<(Instant, u64, Option<u64>)>,
    warned: bool,
    run_dir: Option<PathBuf>,
    run_id: String,
    repo: String,
    run_name: String,
    command: String,
}

impl LaneStallMonitor {
    fn new(
        root_pid: u32,
        run_dir: Option<&Path>,
        repo: &str,
        command: &str,
    ) -> Option<Self> {
        let meta = run_dir
            .map(|r| json(r.join("meta.json")))
            .unwrap_or(Value::Null);
        let threshold_secs =
            lane_stall_threshold_secs(&crate::events::events_path(), repo, command, &meta)?;
        let window_secs = lane_stall_window_secs();
        let sample_interval = Duration::from_secs_f64((window_secs / 4.0).clamp(0.05, 2.0));
        let now = Instant::now();
        let mut history = VecDeque::new();
        if let Some(sample) = inspect_process_tree(root_pid) {
            let active = run_dir.and_then(crate::cmd_out::read_active_bytes);
            history.push_back((now, sample.total_cpu_ticks, active));
        }
        let run_id = run_dir
            .map(|r| crate::events::run_id_for(r, &meta))
            .unwrap_or_default();
        let run_name = s(&meta, "name").to_string();
        Some(Self {
            started: now,
            threshold_secs,
            window_secs,
            sample_interval,
            clk_tck: clk_tck(),
            last_sample: now,
            history,
            warned: false,
            run_dir: run_dir.map(Path::to_path_buf),
            run_id,
            repo: repo.to_string(),
            run_name,
            command: command.to_string(),
        })
    }

    fn poll(&mut self, root_pid: u32) {
        if self.last_sample.elapsed() < self.sample_interval {
            return;
        }
        let now = Instant::now();
        self.last_sample = now;
        let Some(sample) = inspect_process_tree(root_pid) else {
            return;
        };
        let active_bytes = self
            .run_dir
            .as_deref()
            .and_then(crate::cmd_out::read_active_bytes);
        self.history
            .push_back((now, sample.total_cpu_ticks, active_bytes));
        while self.history.len() > 2
            && now.duration_since(self.history[1].0).as_secs_f64() >= self.window_secs
        {
            self.history.pop_front();
        }
        let Some(&(old_at, old_ticks, old_active)) = self.history.front() else {
            return;
        };
        let window_elapsed = now.duration_since(old_at).as_secs_f64();
        if window_elapsed < self.window_secs {
            return;
        }
        let cpu_delta_secs =
            sample.total_cpu_ticks.saturating_sub(old_ticks) as f64 / self.clk_tck;
        let cpu_low = (cpu_delta_secs / window_elapsed) < LANE_STALL_CPU_RATIO_LIMIT;
        let active_unchanged = match (old_active, active_bytes) {
            (Some(old), Some(cur)) => old == cur,
            (None, None) => true,
            _ => false,
        };
        let stalled_now = cpu_low && active_unchanged;
        let elapsed_secs = self.started.elapsed().as_secs_f64();
        if stalled_now && elapsed_secs >= self.threshold_secs {
            if !self.warned {
                self.emit_warning(&sample.deepest, elapsed_secs, window_elapsed);
                self.warned = true;
            }
        } else if !stalled_now && self.warned {
            if let Some(ref run) = self.run_dir {
                clear_stall_warning_for_kind(run, "lane");
            }
            self.warned = false;
        }
    }

    fn emit_warning(&self, proc: &DeepestProc, elapsed_secs: f64, window_elapsed: f64) {
        let elapsed_u64 = elapsed_secs.round() as u64;
        let window_u64 = window_elapsed.round().max(1.0) as u64;
        let threshold_u64 = self.threshold_secs.round() as u64;
        eprintln!(
            "delegate lane: 疑似卡住（pid={} comm={} wchan={}，已运行 {}s，近 {}s 无明显 CPU/输出进展；不会自动终止）",
            proc.pid, proc.comm, proc.wchan, elapsed_u64, window_u64
        );
        let Some(ref run) = self.run_dir else {
            return;
        };
        let at = iso();
        let detail = format!(
            "pid={} comm={} wchan={}，已运行 {}s（近 {}s CPU <1% 且无新输出）",
            proc.pid, proc.comm, proc.wchan, elapsed_u64, window_u64
        );
        let seq = crate::events::emit(
            &self.run_id,
            &self.repo,
            "stall_warning",
            json!({
                "name": self.run_name,
                "stallKind": "lane",
                "cmd": clip(&self.command, 120),
                "pid": proc.pid,
                "comm": proc.comm,
                "wchan": proc.wchan,
                "elapsedSeconds": elapsed_u64,
                "windowSeconds": window_u64,
                "thresholdSeconds": threshold_u64,
                "detail": detail,
            }),
        );
        let mut warning = json!({
            "kind": "lane",
            "cmd": clip(&self.command, 180),
            "pid": proc.pid,
            "comm": proc.comm,
            "wchan": proc.wchan,
            "elapsedSeconds": elapsed_u64,
            "windowSeconds": window_u64,
            "thresholdSeconds": threshold_u64,
            "detail": detail,
            "at": at,
        });
        if let Some(seq_num) = seq {
            warning["seq"] = json!(seq_num);
        }
        write_stall_warning_for_kind(run, "lane", &warning);
        crate::supervise::log_event(
            run,
            json!({
                "e": "stall_warning",
                "kind": "lane",
                "cmd": clip(&self.command, 180),
                "pid": proc.pid,
                "comm": proc.comm,
                "wchan": proc.wchan,
                "elapsedSeconds": elapsed_u64,
                "windowSeconds": window_u64,
                "thresholdSeconds": threshold_u64,
                "detail": detail,
                "at": at,
            }),
        );
    }
}

impl Drop for LaneStallMonitor {
    fn drop(&mut self) {
        if self.warned {
            if let Some(ref run) = self.run_dir {
                clear_stall_warning_for_kind(run, "lane");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_concurrency_for_96_20_4_2_1_threads() {
        let p96 = derive_concurrency(96, None, None);
        assert_eq!(
            (p96.threads, p96.budget, p96.max_heavy, p96.pool_tokens),
            (96, 94, 7, 87)
        );
        assert!(!p96.max_heavy_explicit && !p96.heavy_jobs_explicit);
        assert_eq!(p96.format_summary(), "线程 96 − 2 = 94 · 令牌 87/87 空闲 87");

        let p20 = derive_concurrency(20, None, None);
        assert_eq!(
            (p20.threads, p20.budget, p20.max_heavy, p20.pool_tokens),
            (20, 18, 2, 16)
        );

        let p4 = derive_concurrency(4, None, None);
        assert_eq!(
            (p4.threads, p4.budget, p4.max_heavy, p4.pool_tokens),
            (4, 2, 2, 0)
        );

        let p2 = derive_concurrency(2, None, None);
        assert_eq!(
            (p2.threads, p2.budget, p2.max_heavy, p2.pool_tokens),
            (2, 1, 2, 0)
        );

        let p1 = derive_concurrency(1, None, None);
        assert_eq!(
            (p1.threads, p1.budget, p1.max_heavy, p1.pool_tokens),
            (1, 1, 2, 0)
        );
    }

    #[test]
    fn explicit_max_heavy_and_heavy_jobs_overrides() {
        // Local: 96 threads + explicit maxHeavy=6 => pool_tokens = 94 - 6 = 88
        let p = derive_concurrency(96, Some(6), None);
        assert_eq!((p.budget, p.max_heavy, p.pool_tokens), (94, 6, 88));
        assert!(p.max_heavy_explicit && !p.heavy_jobs_explicit);
        assert_eq!(p.format_summary(), "线程 96 − 2 = 94 · 令牌 88/88 空闲 88");

        // Remote still divides budget / maxHeavy without FIFO/proxy
        let pr = derive_remote_concurrency(96, Some(6), None);
        assert_eq!((pr.budget, pr.max_heavy, pr.heavy_jobs), (94, 6, 15));
        assert_eq!(pr.format_summary(), "线程 96 − 2 = 94 → 6 条 × 15");

        // Remote explicit maxHeavy exceeding budget never lets jobs drop below 1
        let pr_huge = derive_remote_concurrency(4, Some(20), None);
        assert_eq!((pr_huge.budget, pr_huge.max_heavy, pr_huge.heavy_jobs), (2, 20, 1));

        // Explicit heavyJobs overrides per-command cap and marks source as 「显式」
        let p_exp = derive_concurrency(96, Some(6), Some(8));
        assert_eq!((p_exp.max_heavy, p_exp.pool_tokens, p_exp.heavy_jobs), (6, 88, 8));
        assert!(p_exp.heavy_jobs_explicit);
        assert_eq!(
            p_exp.format_summary(),
            "线程 96 − 2 = 94 · 令牌 88/88 空闲 88（显式 ×8）"
        );
    }

    #[test]
    fn cgroup_v2_cpu_max_walks_up_and_caps_below_nproc() {
        let dir = std::env::temp_dir().join(format!(
            "delegate-cg-test-{}-{}",
            std::process::id(),
            now_ns()
        ));
        let leaf = dir.join("user.slice/user-1000.slice/session-1.scope");
        fs::create_dir_all(&leaf).unwrap();
        // Root has 16 cores, parent slice caps at 5.5 -> ceil 6 cores, leaf is unlimited ("max").
        fs::write(dir.join("cpu.max"), "1600000 100000\n").unwrap();
        fs::write(dir.join("user.slice/cpu.max"), "550000 100000\n").unwrap();
        fs::write(leaf.join("cpu.max"), "max 100000\n").unwrap();

        let proc_cg = "0::/user.slice/user-1000.slice/session-1.scope\n";
        let cg_lim = detect_cgroup_v2_cpu_limit_in(&dir, proc_cg);
        assert_eq!(cg_lim, Some(6));

        // Effective threads takes min(available_parallelism=96, cgroup=6, nproc=96) = 6
        let eff = effective_threads(Some(96), cg_lim, Some(96));
        assert_eq!(eff, 6);
        let plan = derive_concurrency(eff, None, None);
        assert_eq!(
            (plan.threads, plan.budget, plan.max_heavy, plan.pool_tokens),
            (6, 4, 2, 2)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[allow(clippy::zombie_processes)] // Reaped directly via raw wait4_blocking(pid) below.
    fn wait4_records_child_tree_cpu_seconds_and_rss() {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
        let started = Instant::now();
        // Run CPU work inside a nested subshell so we verify process-tree rusage coverage.
        let child = Command::new("sh")
            .args([
                "-c",
                "( i=0; while [ $i -lt 80000 ]; do i=$((i + 1)); done )",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cpu-burning sh");
        let pid = child.id() as i32;
        let (status, rusage) = wait4_blocking(pid).expect("wait4 child");
        let wall = started.elapsed().as_secs_f64();
        assert_eq!(wait_status_exit_code(status), 0);
        let ru = rusage.expect("rusage present");
        assert!(
            ru.cpu_user_secs > 0.0,
            "expected cpuUserSecs > 0, got {} (wall={wall:.4}s)",
            ru.cpu_user_secs
        );
        // Allow small timer granularity margin on very fast machines.
        let upper = (wall + 0.05) * cores;
        assert!(
            ru.cpu_user_secs <= upper,
            "expected cpuUserSecs ({}) <= wall ({wall:.4}s) * cores ({cores})",
            ru.cpu_user_secs
        );
        assert!(
            ru.max_rss_kb.is_some_and(|kb| kb > 0),
            "expected maxRssKb > 0, got {:?}",
            ru.max_rss_kb
        );
        let summary = format_cpu_summary(wall, Some(ru.cpu_user_secs), Some(ru.cpu_sys_secs))
            .expect("cpu summary");
        assert!(summary.contains("CPU user "), "summary={summary}");
        assert!(summary.contains(" / sys "), "summary={summary}");
        assert!(summary.contains("墙钟 "), "summary={summary}");
        assert!(summary.contains("CPU/墙钟 "), "summary={summary}");
    }

    #[test]
    fn lane_released_human_format_handles_cpu_and_null() {
        let ev_with_cpu = json!({
            "seq": 7,
            "at": "2026-11-24T21:00:00Z",
            "repo": "/tmp/demo",
            "run": "",
            "kind": "lane_released",
            "payload": {
                "label": "demo: cargo test",
                "heldSeconds": 2.5,
                "remote": false,
                "cpuUserSecs": 4.2,
                "cpuSysSecs": 0.3,
                "maxRssKb": 123456
            }
        });
        let line = crate::events::format_human(&ev_with_cpu);
        assert!(line.contains("CPU user 4.20s / sys 0.30s"), "line={line}");
        assert!(line.contains("墙钟 2.50s"), "line={line}");
        assert!(line.contains("CPU/墙钟 1.80x"), "line={line}");

        let ev_null = json!({
            "seq": 8,
            "at": "2026-11-24T21:00:01Z",
            "repo": "/tmp/demo",
            "run": "",
            "kind": "lane_released",
            "payload": {
                "label": "demo: noop",
                "heldSeconds": 1.0,
                "remote": false,
                "cpuUserSecs": Value::Null,
                "cpuSysSecs": Value::Null,
                "maxRssKb": Value::Null
            }
        });
        let line_null = crate::events::format_human(&ev_null);
        assert!(line_null.contains("(持 1s)"), "line_null={line_null}");
    }

    #[test]
    fn normalize_command_prefix_and_threshold_from_history() {
        assert_eq!(
            normalize_command_prefix("norvane: cd crates && cargo test -p xtask"),
            "cargo test"
        );
        assert_eq!(
            normalize_command_prefix("RUST_LOG=debug ./scripts/verify.sh --fast"),
            "verify.sh"
        );
        assert_eq!(
            normalize_command_prefix("python3 scripts/run-tests.py -k lane"),
            "python3 run-tests.py"
        );

        let dir = std::env::temp_dir().join(format!(
            "delegate-lane-hist-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&dir).unwrap();
        let ev_path = dir.join("events.jsonl");

        // Cold start (no events file): falls back to stall_seconds(meta, "accept").
        let meta = json!({"stall": {"accept": "45s"}});
        assert_eq!(
            lane_stall_threshold_secs(&ev_path, "/repo/a", "cargo test -p foo", &meta),
            Some(45.0)
        );

        // Seed 3 prefix-matching events (10, 20, 30 -> p75 = 25) and 1 other-prefix event (200).
        let lines = [
            json!({"kind":"lane_released","repo":"/repo/a","payload":{"label":"a: cargo test -p x","heldSeconds":10.0}}),
            json!({"kind":"lane_released","repo":"/repo/a","payload":{"label":"a: cargo test -p y","heldSeconds":20.0}}),
            json!({"kind":"lane_released","repo":"/repo/a","payload":{"label":"a: cargo test -p z","heldSeconds":30.0}}),
            json!({"kind":"lane_released","repo":"/repo/a","payload":{"label":"a: pnpm build","heldSeconds":200.0}}),
            json!({"kind":"lane_released","repo":"/repo/b","payload":{"label":"b: cargo test","heldSeconds":999.0}}),
        ];
        let body = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&ev_path, body).unwrap();

        // Prefix match (>=3 samples): p75(10, 20, 30) = 25 * 2 = 50.
        let t_prefix =
            lane_stall_threshold_secs(&ev_path, "/repo/a", "cargo test -p xtask", &meta).unwrap();
        assert!((t_prefix - 50.0).abs() < 1e-6, "t_prefix={t_prefix}");

        // Insufficient prefix samples (<3 for pnpm build): falls back to all repo /repo/a samples (10, 20, 30, 200).
        // pos = 3 * 0.75 = 2.25 => 30 + (200 - 30) * 0.25 = 72.5; * 2 = 145.0.
        let t_fallback =
            lane_stall_threshold_secs(&ev_path, "/repo/a", "pnpm build", &meta).unwrap();
        assert!((t_fallback - 145.0).abs() < 1e-6, "t_fallback={t_fallback}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn inspect_process_tree_finds_deepest_blocked_child() {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 20"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sh sleep");
        let root_pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut found = None;
        while Instant::now() < deadline {
            if let Some(sample) = inspect_process_tree(root_pid) {
                if sample.deepest.pid != root_pid && sample.deepest.comm == "sleep" {
                    found = Some(sample);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        let sample = found.expect("expected deepest child to be sleep");
        assert_eq!(sample.deepest.comm, "sleep");
        assert!(!sample.deepest.wchan.is_empty());
        assert_ne!(sample.deepest.wchan, "-");
    }

    #[test]
    fn stall_warning_kind_isolation() {
        let dir = std::env::temp_dir().join(format!(
            "delegate-stall-iso-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&dir).unwrap();
        assert!(write_stall_warning_for_kind(
            &dir,
            "lane",
            &json!({"kind": "lane", "pid": 42})
        ));
        // A different kind must not overwrite an active lane warning.
        assert!(!write_stall_warning_for_kind(
            &dir,
            "command",
            &json!({"kind": "command"})
        ));
        assert_eq!(s(&json(dir.join("stall-warning.json")), "kind"), "lane");
        // Clearing a different kind must not remove the lane warning.
        clear_stall_warning_for_kind(&dir, "command");
        assert!(dir.join("stall-warning.json").is_file());
        // Clearing matching kind removes it.
        clear_stall_warning_for_kind(&dir, "lane");
        assert!(!dir.join("stall-warning.json").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
