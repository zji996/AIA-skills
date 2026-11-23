use crate::common::*;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

thread_local! {
    static SLOT_ENV_STACK: RefCell<Vec<Vec<(String, String)>>> = const { RefCell::new(Vec::new()) };
}

pub fn current_slot_env() -> Option<Vec<(String, String)>> {
    SLOT_ENV_STACK.with(|s| s.borrow().last().cloned())
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
    let avail = std::thread::available_parallelism().ok().map(|n| n.get());
    let cgroup = detect_cgroup_v2_cpu_limit();
    let nproc_val = detect_nproc();
    effective_threads(avail, cgroup, nproc_val)
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
    Ok(tickets_in(&state_dir().join("lane-remote"))
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
pub struct Slot {
    ticket: Option<PathBuf>,
    _file: Option<File>,
    proxy: Option<crate::jobserver::ProxyGuard>,
    pushed_env: bool,
    pub queued: f64,
    acquired_at: Option<Instant>,
    event_run: String,
    event_repo: String,
    event_label: String,
    event_remote: bool,
}
impl Slot {
    pub fn env_vars(&self) -> Option<Vec<(String, String)>> {
        self.proxy.as_ref().map(|p| p.env_vars())
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
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
                crate::events::emit(
                    &self.event_run,
                    &self.event_repo,
                    "lane_released",
                    json!({
                        "label": self.event_label,
                        "heldSeconds": held,
                        "remote": self.event_remote,
                    }),
                );
            }
        }
    }
}
fn block_on(p: &Path) {
    if let Ok(f) = OpenOptions::new().read(true).open(p) {
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH) };
    }
}
pub fn acquire(
    label: &str,
    account: Option<&Path>,
    context: Option<&Path>,
    waiting: impl FnMut(usize, &[String]),
) -> Res<Slot> {
    acquire_in(&lane_dir(), heavy_limit(context)?, label, account, context, false, waiting)
}
pub fn acquire_remote(
    label: &str,
    account: Option<&Path>,
    remote: &Value,
    waiting: impl FnMut(usize, &[String]),
) -> Res<Slot> {
    let plan = crate::remote::remote_plan(remote, true);
    acquire_in(
        &state_dir().join("lane-remote"),
        plan.max_heavy,
        label,
        account,
        account,
        true,
        waiting,
    )
}
/// Shell steps use the remote lane, while the public `lane` command stays local.
pub fn acquire_shell(
    label: &str,
    account: Option<&Path>,
    context: Option<&Path>,
    waiting: impl FnMut(usize, &[String]),
) -> Res<Slot> {
    let meta = context
        .map(|r| json(r.join("meta.json")))
        .unwrap_or(Value::Null);
    if crate::remote::enabled(&meta["remote"]) {
        acquire_remote(label, account.or(context), &meta["remote"], waiting)
    } else {
        acquire(label, account, context, waiting)
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

fn acquire_in(
    dir: &Path,
    limit: usize,
    label: &str,
    account: Option<&Path>,
    context: Option<&Path>,
    remote: bool,
    mut waiting: impl FnMut(usize, &[String]),
) -> Res<Slot> {
    if std::env::var_os("DELEGATE_LANE_HELD").is_some() {
        return Ok(Slot {
            ticket: None,
            _file: None,
            proxy: None,
            pushed_env: false,
            queued: 0.0,
            acquired_at: None,
            event_run: String::new(),
            event_repo: String::new(),
            event_label: String::new(),
            event_remote: remote,
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
        return Ok(Slot {
            ticket: None,
            _file: None,
            proxy,
            pushed_env,
            queued: 0.0,
            acquired_at: None,
            event_run: String::new(),
            event_repo: String::new(),
            event_label: String::new(),
            event_remote: remote,
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
    let mut me = json!({"pid":std::process::id(),"label":label,"since":iso(),"epoch":epoch(),"queuedNs":stamp,"runStartedNs":run_started_ns,"run":account,"running":false});
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
            block_on(&live[pos - limit].0);
            LANE_WAITING.store(false, Ordering::SeqCst);
            if stopped() {
                return Err("stopped while queued for the heavy lane".into());
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
    crate::events::emit(
        &event_run,
        &event_repo,
        "lane_acquired",
        json!({
            "label": label,
            "queuedSeconds": (queued * 100.0).round() / 100.0,
            "remote": remote,
        }),
    );
    Ok(Slot {
        ticket: Some(ticket),
        _file: Some(file),
        proxy,
        pushed_env,
        queued,
        acquired_at: Some(Instant::now()),
        event_run,
        event_repo,
        event_label: label.to_string(),
        event_remote: remote,
    })
}
/// Keep only queued jobs, not processes: question cleanup still reclaims every child.
pub fn save_priority(run: &Path) -> Res<()> {
    let mut priority = json!({});
    for dir in [lane_dir(), state_dir().join("lane-remote")] {
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
    let local_items: Vec<Value> = local_tickets
        .into_iter()
        .map(|(pid, label, since, running)| {
            let mut item = json!({"label": label, "since": since, "running": running});
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
            json!({"label": label, "since": since, "running": running})
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
        if json_mode {
            let local_items: Vec<Value> = local_tickets
                .into_iter()
                .map(|(pid, label, since, running)| {
                    let mut item = json!({"label": label, "since": since, "running": running});
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
                    json!({"label": label, "since": since, "running": running})
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
            });
            if let Some(rp) = remote_plan {
                out["remoteConcurrency"] = rp.to_json();
            }
            println!("{out}");
            return Ok(0);
        }
        println!("{}", plan.format_summary_with_pool(&pool));
        if let Some(rp) = remote_plan {
            println!("remote {}", rp.format_summary());
        }
        for (label, since, running) in remote {
            println!(
                "remote {:<8} {since}  {label}",
                if running { "running" } else { "queued" }
            );
        }
        for (pid, label, since, running) in local_tickets {
            if running {
                if let Some(lease) = match_lease(pid, &label) {
                    println!(
                        "{:<8} {since}  #{} {}/{}  {label}",
                        "running",
                        lease.rank,
                        lease.used_jobs(),
                        lease.share
                    );
                    continue;
                }
            }
            println!(
                "{:<8} {since}  {label}",
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
    let slot = acquire(
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
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let pid = child.id() as i32;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            let sig = SIGNAL.load(Ordering::SeqCst);
            if let Some(dir) = &temporary {
                let _ = fs::remove_dir_all(dir);
            }
            return Ok(if sig != 0 {
                128 + sig
            } else {
                use std::os::unix::process::ExitStatusExt;
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
            });
        }
        let sig = SIGNAL.load(Ordering::SeqCst);
        if sig != 0 {
            end_group(pid, 3.0);
            let _ = child.wait();
            if let Some(dir) = &temporary {
                let _ = fs::remove_dir_all(dir);
            }
            return Ok(128 + sig);
        }
        std::thread::sleep(Duration::from_millis(20));
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
}
