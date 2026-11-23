use crate::common::{json, now_ns, state_dir, Res};
use serde_json::{json as j, Value};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

static LEASE_SEQ: AtomicU64 = AtomicU64::new(1);

/// If a pre-staged token sits unread in a command's proxy FIFO for at least this long,
/// the command is treated as not wanting more parallelism (narrow graph / idle),
/// so surplus pool tokens flow to other active commands.
const STAGED_IDLE_MS: u64 = 15;
const RECONCILE_INTERVAL_MS: u64 = 500;

struct OwnedFd(RawFd);

impl OwnedFd {
    fn raw(&self) -> RawFd {
        self.0
    }
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe { libc::close(self.0) };
        }
    }
}

fn fifo_path(base: &Path) -> PathBuf {
    base.join("jobserver.fifo")
}

fn lock_path(base: &Path) -> PathBuf {
    base.join("jobserver.lock")
}

fn leases_dir(base: &Path) -> PathBuf {
    base.join("jobserver-leases")
}

fn ensure_fifo(path: &Path) -> Res<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_fifo() {
            return Ok(());
        }
        let _ = fs::remove_file(path);
    }
    let cpath = CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    let rc = unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EEXIST) {
            return Err(format!("mkfifo {}: {err}", path.display()));
        }
        let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if !meta.file_type().is_fifo() {
            return Err(format!("{} exists and is not a FIFO", path.display()));
        }
    }
    Ok(())
}

fn open_fifo_nonblock(path: &Path) -> Res<OwnedFd> {
    let cpath = CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(format!(
            "open fifo {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(OwnedFd(fd))
}

fn make_wake_pipe() -> Res<(OwnedFd, OwnedFd)> {
    let mut fds = [-1i32; 2];
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if rc != 0 {
        return Err(format!("pipe2: {}", std::io::Error::last_os_error()));
    }
    Ok((OwnedFd(fds[0]), OwnedFd(fds[1])))
}

fn fifo_bytes(fd: RawFd) -> usize {
    let mut n: libc::c_int = 0;
    if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut n) } == 0 && n > 0 {
        n as usize
    } else {
        0
    }
}

fn read_nonblock(fd: RawFd, max: usize) -> usize {
    if max == 0 {
        return 0;
    }
    let mut buf = [0u8; 256];
    let mut total = 0usize;
    while total < max {
        let want = (max - total).min(buf.len());
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, want) };
        if n > 0 {
            total += n as usize;
        } else if n < 0 {
            let err = std::io::Error::last_os_error().raw_os_error();
            if err == Some(libc::EINTR) {
                continue;
            }
            break;
        } else {
            break;
        }
    }
    total
}

fn drain_fifo(fd: RawFd) -> usize {
    let mut total = 0usize;
    let mut buf = [0u8; 256];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            total += n as usize;
        } else if n < 0 {
            let err = std::io::Error::last_os_error().raw_os_error();
            if err == Some(libc::EINTR) {
                continue;
            }
            break;
        } else {
            break;
        }
    }
    total
}

fn write_tokens(fd: RawFd, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let chunk = [b'+'; 256];
    let mut written = 0usize;
    while written < count {
        let want = (count - written).min(chunk.len());
        let n = unsafe { libc::write(fd, chunk.as_ptr() as *const libc::c_void, want) };
        if n > 0 {
            written += n as usize;
        } else if n < 0 {
            let err = std::io::Error::last_os_error().raw_os_error();
            if err == Some(libc::EINTR) {
                continue;
            }
            break;
        } else {
            break;
        }
    }
    written
}

fn poll_fds(fds: &[(RawFd, i16)], timeout_ms: i32) {
    let mut pfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&(fd, events)| libc::pollfd {
            fd,
            events,
            revents: 0,
        })
        .collect();
    let _ = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout_ms) };
}

fn try_lock_dead(path: &Path) -> Option<File> {
    let f = OpenOptions::new().read(true).write(true).open(path).ok()?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Some(f)
    } else {
        None
    }
}

fn rewrite_locked_file(file: &mut File, content: &str) {
    if file.seek(SeekFrom::Start(0)).is_ok()
        && file.write_all(content.as_bytes()).is_ok()
    {
        let _ = file.set_len(content.len() as u64);
    }
}

/// Compute shared jobserver pool target tokens: `budget - effective_max_heavy`
/// (each running cargo carries 1 implicit token of its own).
pub fn compute_pool_tokens(budget: usize, max_heavy: usize, default_max_heavy: usize) -> usize {
    let effective_mh = if max_heavy == 0 {
        default_max_heavy
    } else {
        max_heavy
    };
    budget.saturating_sub(effective_mh)
}

/// Compute dynamic weighted fair shares across `count` active commands ordered by seniority:
/// weights are `1, 1/2, 1/4, 1/8, ...`, `share = budget * weight / sum(weights)`,
/// with a floor of `2` per command (or `budget` when `budget < 2`), summing to `budget`
/// whenever `budget >= 2 * count`.
pub fn compute_fair_shares(budget: usize, count: usize) -> Vec<usize> {
    if count == 0 {
        return vec![];
    }
    let budget = budget.max(1);
    if count == 1 {
        return vec![budget];
    }
    let floor = 2.min(budget);
    if budget <= floor * count {
        return vec![floor; count];
    }

    let weights: Vec<f64> = (0..count)
        .map(|k| 1.0 / 2.0_f64.powi(k.min(30) as i32))
        .collect();

    let mut pinned = vec![false; count];
    let mut shares = vec![0usize; count];
    let mut rem_budget = budget;

    // Iteratively pin commands whose proportional share would fall below the floor.
    loop {
        let sum_w: f64 = weights
            .iter()
            .enumerate()
            .filter(|(i, _)| !pinned[*i])
            .map(|(_, &w)| w)
            .sum();
        if sum_w <= 0.0 {
            break;
        }
        let mut newly_pinned = false;
        for i in 0..count {
            if !pinned[i] {
                let raw = (rem_budget as f64) * weights[i] / sum_w;
                if raw < floor as f64 {
                    pinned[i] = true;
                    shares[i] = floor;
                    rem_budget = rem_budget.saturating_sub(floor);
                    newly_pinned = true;
                }
            }
        }
        if !newly_pinned {
            break;
        }
    }

    // Distribute remaining budget among unpinned commands via largest-remainder method.
    let unpinned: Vec<usize> = (0..count).filter(|&i| !pinned[i]).collect();
    if !unpinned.is_empty() {
        let sum_w: f64 = unpinned.iter().map(|&i| weights[i]).sum();
        let mut fracs: Vec<(usize, f64)> = Vec::with_capacity(unpinned.len());
        let mut assigned = 0usize;
        for &i in &unpinned {
            let exact = (rem_budget as f64) * weights[i] / sum_w;
            let base = (exact.floor() as usize).max(floor);
            shares[i] = base;
            assigned += base;
            fracs.push((i, exact - exact.floor()));
        }
        if assigned < rem_budget {
            let mut left = rem_budget - assigned;
            fracs.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.0.cmp(&b.0))
            });
            for (i, _) in fracs {
                if left == 0 {
                    break;
                }
                shares[i] += 1;
                left -= 1;
            }
        } else if assigned > rem_budget {
            let mut excess = assigned - rem_budget;
            for &i in unpinned.iter().rev() {
                if excess == 0 {
                    break;
                }
                let can_drop = shares[i].saturating_sub(floor);
                let drop = can_drop.min(excess);
                shares[i] -= drop;
                excess -= drop;
            }
        }
    }

    shares
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseView {
    pub id: String,
    pub pid: u32,
    pub run_started_ns: u64,
    pub label: String,
    pub borrowed: usize,
    pub staged: usize,
    pub held: usize,
    pub wanting: bool,
    pub explicit_cap: Option<usize>,
    pub share: usize,
    pub rank: usize, // 1-based seniority rank
}

impl LeaseView {
    /// Extra tokens beyond cargo's 1 implicit token.
    pub fn extra_share(&self) -> usize {
        self.share.saturating_sub(1)
    }

    /// Distance below share (higher = further behind share = higher priority).
    pub fn deficit(&self) -> i64 {
        (self.extra_share() as i64) - (self.borrowed as i64)
    }

    /// Below the guaranteed floor of 2 jobs (needs at least 1 pool token to move past implicit 1).
    pub fn below_floor(&self) -> bool {
        self.borrowed == 0 && self.extra_share() >= 1
    }

    /// Whether this lease is capped by an explicit `heavyJobs` ceiling.
    pub fn at_explicit_cap(&self) -> bool {
        self.explicit_cap
            .is_some_and(|cap| self.held >= cap.saturating_sub(1))
    }

    /// Effective active jobs (1 implicit cargo token + borrowed pool tokens).
    pub fn used_jobs(&self) -> usize {
        self.borrowed + 1
    }
}

/// Assign 1-based seniority ranks and dynamic fair shares to a list of alive leases.
/// Leases are sorted by `(run_started_ns, id)` ascending.
pub fn assign_ranks_and_shares(leases: &mut [LeaseView], budget: usize) {
    leases.sort_by(|a, b| {
        a.run_started_ns
            .cmp(&b.run_started_ns)
            .then_with(|| a.id.cmp(&b.id))
    });
    let dyn_indices: Vec<usize> = leases
        .iter()
        .enumerate()
        .filter(|(_, l)| l.explicit_cap.is_none())
        .map(|(i, _)| i)
        .collect();
    let explicit_sum: usize = leases
        .iter()
        .filter_map(|l| l.explicit_cap)
        .sum();
    let dyn_budget = if explicit_sum == 0 {
        budget
    } else {
        budget.saturating_sub(explicit_sum).max(1)
    };
    let dyn_shares = compute_fair_shares(dyn_budget, dyn_indices.len());
    for (pos, &idx) in dyn_indices.iter().enumerate() {
        leases[idx].share = dyn_shares[pos];
    }
    for (idx, lease) in leases.iter_mut().enumerate() {
        lease.rank = idx + 1;
        if let Some(cap) = lease.explicit_cap {
            lease.share = cap.max(1);
        }
    }
}

/// Priority tuple for comparing two `wanting` leases (higher tuple wins the next free token):
/// 1. `below_floor`: guarantees newly arrived commands reach floor >= 2 immediately.
/// 2. `deficit`: furthest below fair share (`extra_share - borrowed`).
/// 3. Seniority tie-breaker: smaller `rank` (older run) wins.
fn priority_key(l: &LeaseView) -> (bool, i64, std::cmp::Reverse<usize>) {
    (l.below_floor(), l.deficit(), std::cmp::Reverse(l.rank))
}

/// Pick the index of the highest-priority lease that wants a token and is not at an explicit cap.
pub fn pick_winner(leases: &[LeaseView]) -> Option<usize> {
    leases
        .iter()
        .enumerate()
        .filter(|(_, l)| l.wanting && !l.at_explicit_cap())
        .max_by_key(|(_, l)| priority_key(l))
        .map(|(i, _)| i)
}

/// Decide whether lease `me_idx` should immediately yield returned tokens back to the global pool
/// instead of keeping one pre-staged in its private proxy FIFO.
pub fn should_yield(me_idx: usize, leases: &[LeaseView]) -> bool {
    let me = &leases[me_idx];
    if me.at_explicit_cap() {
        return true;
    }
    let others_wanting: Vec<&LeaseView> = leases
        .iter()
        .enumerate()
        .filter(|&(i, l)| i != me_idx && l.wanting && !l.at_explicit_cap())
        .map(|(_, l)| l)
        .collect();
    if others_wanting.is_empty() {
        return false;
    }
    // If we are strictly over our fair share while someone else wants tokens, yield!
    if me.borrowed > me.extra_share() {
        return true;
    }
    // If another wanting command is below its guaranteed floor (borrowed == 0) and we hold >= 1, yield!
    if me.borrowed > 0 && others_wanting.iter().any(|o| o.below_floor()) && !me.below_floor() {
        return true;
    }
    // Hysteresis of 1 token prevents ping-pong at equilibrium while letting starving peers catch up.
    let max_other_deficit = others_wanting
        .iter()
        .map(|o| o.deficit())
        .max()
        .unwrap_or(i64::MIN);
    max_other_deficit > me.deficit() + 1 && me.borrowed > 0
}

#[derive(Debug, Clone)]
pub struct PoolStatus {
    pub fifo: PathBuf,
    pub target: usize,
    pub total: usize,
    pub free: usize,
    pub borrowed: usize,
    pub recovered: usize,
    pub leases: Vec<LeaseView>,
}

impl PoolStatus {
    /// Format e.g. `令牌 88/88 空闲 31`
    pub fn format_pool_part(&self) -> String {
        format!("令牌 {}/{} 空闲 {}", self.total, self.target, self.free)
    }

    /// Format per-command shares e.g. `#1 61/62 · #2 31/31`
    pub fn format_leases_strip(&self) -> Option<String> {
        if self.leases.is_empty() {
            return None;
        }
        let parts: Vec<String> = self
            .leases
            .iter()
            .map(|l| format!("#{} {}/{}", l.rank, l.used_jobs(), l.share))
            .collect();
        Some(parts.join(" · "))
    }

    pub fn to_json(&self) -> Value {
        let leases: Vec<Value> = self
            .leases
            .iter()
            .map(|l| {
                j!({
                    "id": l.id,
                    "pid": l.pid,
                    "rank": l.rank,
                    "label": l.label,
                    "borrowed": l.borrowed,
                    "staged": l.staged,
                    "held": l.held,
                    "used": l.used_jobs(),
                    "share": l.share,
                    "wanting": l.wanting,
                    "explicitCap": l.explicit_cap,
                })
            })
            .collect();
        j!({
            "fifo": self.fifo,
            "target": self.target,
            "total": self.total,
            "free": self.free,
            "borrowed": self.borrowed,
            "recovered": self.recovered,
            "summary": self.format_pool_part(),
            "leasesSummary": self.format_leases_strip(),
            "leases": leases,
        })
    }
}

/// Scan `<base>/jobserver-leases/`, reap dead (unlocked) lease files and their orphan proxy FIFOs,
/// and return `(alive_leases, dead_held_tokens)`.
/// Caller MUST hold `jobserver.lock`.
fn scan_leases_locked(base: &Path, budget: usize) -> (Vec<LeaseView>, usize) {
    let dir = leases_dir(base);
    let Ok(entries) = fs::read_dir(&dir) else {
        return (vec![], 0);
    };
    let mut alive = Vec::new();
    let mut dead_held = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lease") {
            continue;
        }
        if let Some(_dead_lock) = try_lock_dead(&path) {
            let v = json(&path);
            dead_held += v["held"].as_u64().unwrap_or(0) as usize;
            if let Some(pf) = v["proxyFifo"].as_str() {
                if !pf.is_empty() {
                    let _ = fs::remove_file(pf);
                }
            }
            let _ = fs::remove_file(&path);
            continue;
        }
        let v = json(&path);
        let id = v["id"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        let pid = v["pid"].as_u64().unwrap_or(0) as u32;
        let run_started_ns = v["runStartedNs"].as_u64().unwrap_or(0);
        let label = v["label"].as_str().unwrap_or("").to_string();
        let borrowed = v["borrowed"].as_u64().unwrap_or(0) as usize;
        let staged = v["staged"].as_u64().unwrap_or(0) as usize;
        let held = v["held"].as_u64().unwrap_or(0) as usize;
        let wanting = v["wanting"].as_bool().unwrap_or(false);
        let explicit_cap = v["explicitCap"]
            .as_u64()
            .filter(|&n| n >= 1)
            .map(|n| n as usize);
        alive.push(LeaseView {
            id,
            pid,
            run_started_ns,
            label,
            borrowed,
            staged,
            held,
            wanting,
            explicit_cap,
            share: 0,
            rank: 0,
        });
    }
    assign_ranks_and_shares(&mut alive, budget);
    (alive, dead_held)
}

/// Reconcile the machine-wide jobserver FIFO under `jobserver.lock`:
/// - Reaps dead leases (e.g. from `SIGKILL`ed `delegate lane` processes) and replenishes leaked tokens.
/// - When 0 live leases remain, resets the FIFO to exact `target_tokens` (if `global_fd` stays open).
/// - When `fifo_bytes > target_tokens`, drains excess tokens.
fn reconcile_locked(
    base: &Path,
    global_fd: RawFd,
    target_tokens: usize,
    budget: usize,
) -> PoolStatus {
    let (alive, dead_held) = scan_leases_locked(base, budget);
    let mut recovered = 0usize;
    if alive.is_empty() {
        let cur = fifo_bytes(global_fd);
        if cur != target_tokens {
            if cur > target_tokens {
                read_nonblock(global_fd, cur - target_tokens);
            } else {
                recovered = target_tokens - cur;
                write_tokens(global_fd, recovered);
            }
        }
        return PoolStatus {
            fifo: fifo_path(base),
            target: target_tokens,
            total: target_tokens,
            free: target_tokens,
            borrowed: 0,
            recovered: if dead_held > 0 { dead_held } else { recovered },
            leases: alive,
        };
    }

    let cur = fifo_bytes(global_fd);
    if dead_held > 0 {
        let sum_alive_held: usize = alive.iter().map(|l| l.held).sum();
        let max_add = target_tokens.saturating_sub(cur.saturating_add(sum_alive_held));
        let add = dead_held.min(target_tokens.saturating_sub(cur)).max(max_add);
        if add > 0 {
            recovered = write_tokens(global_fd, add);
        }
    }
    let cur_after = fifo_bytes(global_fd);
    if cur_after > target_tokens {
        read_nonblock(global_fd, cur_after - target_tokens);
    }

    let free = fifo_bytes(global_fd).min(target_tokens);
    let sum_held: usize = alive.iter().map(|l| l.held).sum();
    let sum_borrowed: usize = alive.iter().map(|l| l.borrowed).sum();
    let total = (free + sum_held).min(target_tokens);
    PoolStatus {
        fifo: fifo_path(base),
        target: target_tokens,
        total,
        free,
        borrowed: sum_borrowed,
        recovered,
        leases: alive,
    }
}

/// Public pool status + leak reconciliation entry point (called by `lane`, `status`, etc.).
pub fn reconcile_pool_in(base: &Path, target_tokens: usize, budget: usize) -> PoolStatus {
    let fifo = fifo_path(base);
    if fs::create_dir_all(base).is_err() || fs::create_dir_all(leases_dir(base)).is_err() {
        return PoolStatus {
            fifo,
            target: target_tokens,
            total: target_tokens,
            free: target_tokens,
            borrowed: 0,
            recovered: 0,
            leases: vec![],
        };
    }
    let Ok(_guard) = crate::locks::try_acquire_with_retry(
        crate::locks::LockKind::Jobserver,
        &lock_path(base),
        true,
        3,
        Duration::from_millis(15),
    ) else {
        return PoolStatus {
            fifo,
            target: target_tokens,
            total: target_tokens,
            free: target_tokens,
            borrowed: 0,
            recovered: 0,
            leases: vec![],
        };
    };
    if ensure_fifo(&fifo).is_err() {
        return PoolStatus {
            fifo,
            target: target_tokens,
            total: target_tokens,
            free: target_tokens,
            borrowed: 0,
            recovered: 0,
            leases: vec![],
        };
    }
    let Ok(global_fd) = open_fifo_nonblock(&fifo) else {
        return PoolStatus {
            fifo,
            target: target_tokens,
            total: target_tokens,
            free: target_tokens,
            borrowed: 0,
            recovered: 0,
            leases: vec![],
        };
    };
    reconcile_locked(base, global_fd.raw(), target_tokens, budget)
}

pub fn reconcile_pool(target_tokens: usize, budget: usize) -> PoolStatus {
    reconcile_pool_in(&state_dir(), target_tokens, budget)
}

/// Pure read-only snapshot of jobserver pool status (zero locks, zero file creation/deletion).
pub fn inspect_pool_readonly(target_tokens: usize, budget: usize) -> PoolStatus {
    let base = state_dir();
    let fifo = fifo_path(&base);
    let mut alive = Vec::new();
    if let Ok(entries) = fs::read_dir(leases_dir(&base)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("lease") {
                continue;
            }
            let v = json(&path);
            let pid = v["pid"].as_u64().unwrap_or(0) as u32;
            if pid == 0 || !crate::common::pid_alive(pid as i32) {
                continue;
            }
            let id = v["id"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| {
                    path.file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                });
            let run_started_ns = v["runStartedNs"].as_u64().unwrap_or(0);
            let label = v["label"].as_str().unwrap_or("").to_string();
            let borrowed = v["borrowed"].as_u64().unwrap_or(0) as usize;
            let staged = v["staged"].as_u64().unwrap_or(0) as usize;
            let held = v["held"].as_u64().unwrap_or(0) as usize;
            let wanting = v["wanting"].as_bool().unwrap_or(false);
            let explicit_cap = v["explicitCap"]
                .as_u64()
                .filter(|&n| n >= 1)
                .map(|n| n as usize);
            alive.push(LeaseView {
                id,
                pid,
                run_started_ns,
                label,
                borrowed,
                staged,
                held,
                wanting,
                explicit_cap,
                share: 0,
                rank: 0,
            });
        }
    }
    assign_ranks_and_shares(&mut alive, budget);
    let borrowed: usize = alive.iter().map(|l| l.borrowed).sum();
    let free = target_tokens.saturating_sub(borrowed);
    PoolStatus {
        fifo,
        target: target_tokens,
        total: target_tokens,
        free,
        borrowed,
        recovered: 0,
        leases: alive,
    }
}

#[allow(clippy::too_many_arguments)]
fn format_lease_json(
    id: &str,
    pid: u32,
    run_started_ns: u64,
    label: &str,
    proxy_fifo: &Path,
    borrowed: usize,
    staged: usize,
    held: usize,
    wanting: bool,
    explicit_cap: Option<usize>,
) -> String {
    j!({
        "id": id,
        "pid": pid,
        "runStartedNs": run_started_ns,
        "label": label,
        "proxyFifo": proxy_fifo,
        "borrowed": borrowed,
        "staged": staged,
        "held": held,
        "wanting": wanting,
        "explicitCap": explicit_cap,
    })
    .to_string()
}

/// Active per-command jobserver proxy running in a lightweight thread inside `delegate lane`.
/// Bridges `<state>/jobserver.fifo` <-> `<state>/lane/js-<id>.fifo` with dynamic fair-share
/// arbitration and exact token recovery when the child exits or is `SIGKILL`ed.
pub struct ProxyGuard {
    proxy_fifo: PathBuf,
    initial_jobs: usize,
    stop: Arc<AtomicBool>,
    wake_w: OwnedFd,
    handle: Option<JoinHandle<()>>,
}

impl ProxyGuard {
    pub fn start(
        base: PathBuf,
        target_tokens: usize,
        budget: usize,
        run_started_ns: u64,
        label: String,
        explicit_cap: Option<usize>,
    ) -> Res<Self> {
        fs::create_dir_all(&base).map_err(|e| e.to_string())?;
        fs::create_dir_all(leases_dir(&base)).map_err(|e| e.to_string())?;
        let lane_subdir = base.join("lane");
        fs::create_dir_all(&lane_subdir).map_err(|e| e.to_string())?;

        let pid = std::process::id();
        let seq = LEASE_SEQ.fetch_add(1, Ordering::Relaxed);
        let id = format!("{pid}-{seq}-{}", now_ns() % 1_000_000);
        let proxy_fifo = lane_subdir.join(format!("js-{id}.fifo"));
        let lease_path = leases_dir(&base).join(format!("{id}.lease"));

        ensure_fifo(&proxy_fifo)?;
        let proxy_fd = open_fifo_nonblock(&proxy_fifo)?;
        let (wake_r, wake_w) = make_wake_pipe()?;

        let global_fifo = fifo_path(&base);
        let (global_fd, mut lease_file, initial_jobs) = {
            let _guard = crate::locks::acquire(crate::locks::LockKind::Jobserver, &lock_path(&base), true).map_err(|e| e.to_string())?;
            ensure_fifo(&global_fifo)?;
            let global_fd = open_fifo_nonblock(&global_fifo)?;
            let _status = reconcile_locked(&base, global_fd.raw(), target_tokens, budget);

            let mut lf = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&lease_path)
                .map_err(|e| e.to_string())?;
            let rc = unsafe { libc::flock(lf.as_raw_fd(), libc::LOCK_EX) };
            if rc != 0 {
                return Err(format!("flock lease: {}", std::io::Error::last_os_error()));
            }
            let init_wanting = !explicit_cap.is_some_and(|c| c <= 1);
            let content = format_lease_json(
                &id,
                pid,
                run_started_ns,
                &label,
                &proxy_fifo,
                0,
                0,
                0,
                init_wanting,
                explicit_cap,
            );
            rewrite_locked_file(&mut lf, &content);

            let (alive, _) = scan_leases_locked(&base, budget);
            let initial_jobs = explicit_cap.unwrap_or_else(|| {
                alive
                    .iter()
                    .find(|l| l.id == id)
                    .map(|l| l.share)
                    .unwrap_or(budget.max(1))
            });
            (global_fd, lf, initial_jobs)
        };

        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);
        let proxy_fifo_bg = proxy_fifo.clone();

        let handle = thread::Builder::new()
            .name("delegate-jobserver-proxy".into())
            .spawn(move || {
                run_proxy_loop(
                    base,
                    id,
                    pid,
                    run_started_ns,
                    label,
                    proxy_fifo_bg,
                    lease_path,
                    &mut lease_file,
                    global_fd,
                    proxy_fd,
                    wake_r,
                    target_tokens,
                    budget,
                    explicit_cap,
                    stop_bg,
                );
            })
            .map_err(|e| format!("spawn jobserver proxy: {e}"))?;

        Ok(Self {
            proxy_fifo,
            initial_jobs,
            stop,
            wake_w,
            handle: Some(handle),
        })
    }

    #[cfg(test)]
    pub fn proxy_fifo(&self) -> &Path {
        &self.proxy_fifo
    }

    pub fn makeflags(&self) -> String {
        format!("-j --jobserver-auth=fifo:{}", self.proxy_fifo.display())
    }

    pub fn env_vars(&self) -> Vec<(String, String)> {
        let mf = self.makeflags();
        vec![
            ("MAKEFLAGS".into(), mf.clone()),
            ("CARGO_MAKEFLAGS".into(), mf),
            ("CARGO_BUILD_JOBS".into(), self.initial_jobs.to_string()),
        ]
    }
}

impl Drop for ProxyGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let b = [1u8];
        let _ = unsafe { libc::write(self.wake_w.raw(), b.as_ptr() as *const libc::c_void, 1) };
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_proxy_loop(
    base: PathBuf,
    id: String,
    pid: u32,
    run_started_ns: u64,
    label: String,
    proxy_fifo: PathBuf,
    lease_path: PathBuf,
    lease_file: &mut File,
    global_fd: OwnedFd,
    proxy_fd: OwnedFd,
    wake_r: OwnedFd,
    target_tokens: usize,
    budget: usize,
    explicit_cap: Option<usize>,
    stop: Arc<AtomicBool>,
) {
    let max_explicit_extra = explicit_cap.map(|c| c.saturating_sub(1));
    let mut held = 0usize;
    let mut staged_since: Option<Instant> = None;
    let mut last_reconcile = Instant::now();
    let mut idle_sleep_us = 200u64;
    let mut last_reported: Option<(usize, usize, usize, bool)> = None;

    let mut update_lease = |lf: &mut File,
                            borrowed: usize,
                            staged: usize,
                            held_now: usize,
                            wanting: bool| {
        let key = (borrowed, staged, held_now, wanting);
        if last_reported != Some(key) {
            let content = format_lease_json(
                &id,
                pid,
                run_started_ns,
                &label,
                &proxy_fifo,
                borrowed,
                staged,
                held_now,
                wanting,
                explicit_cap,
            );
            rewrite_locked_file(lf, &content);
            last_reported = Some(key);
        }
    };

    while !stop.load(Ordering::SeqCst) {
        let cur = fifo_bytes(proxy_fd.raw());
        let borrowed = held.saturating_sub(cur);
        if cur == 0 {
            staged_since = None;
        } else if staged_since.is_none() {
            staged_since = Some(Instant::now());
        }
        let at_cap = max_explicit_extra.is_some_and(|cap| held >= cap);
        let staged_stale = staged_since
            .is_some_and(|t| t.elapsed() >= Duration::from_millis(STAGED_IDLE_MS));
        let wanting = !at_cap && (cur == 0 || !staged_stale);

        // Update lease snapshot before reading arbitration state.
        update_lease(lease_file, borrowed, cur.min(held), held, wanting);

        // Arbitrate under jobserver.lock.
        let mut need_yield = false;
        let mut i_am_winner = false;
        let mut did_work = false;
        let mut global_has_tokens = false;

        if let Ok(_guard) = crate::locks::acquire(crate::locks::LockKind::Jobserver, &lock_path(&base), true) {
            if last_reconcile.elapsed() >= Duration::from_millis(RECONCILE_INTERVAL_MS) {
                let _ = reconcile_locked(&base, global_fd.raw(), target_tokens, budget);
                last_reconcile = Instant::now();
            }
            let (alive, _) = scan_leases_locked(&base, budget);
            if let Some(me_idx) = alive.iter().position(|l| l.id == id) {
                need_yield = should_yield(me_idx, &alive);
                let winner = pick_winner(&alive);
                i_am_winner = winner == Some(me_idx);
            }

            let cur_now = fifo_bytes(proxy_fd.raw());
            // 1. Return excess / yielded tokens from proxy_fd -> global_fd.
            let keep = if need_yield || at_cap { 0 } else { 1 };
            if cur_now > keep {
                let to_reclaim = (cur_now - keep).min(held);
                if to_reclaim > 0 {
                    let got = read_nonblock(proxy_fd.raw(), to_reclaim);
                    if got > 0 {
                        let room = target_tokens.saturating_sub(fifo_bytes(global_fd.raw()));
                        let _ = write_tokens(global_fd.raw(), got.min(room));
                        held = held.saturating_sub(got);
                        did_work = true;
                    }
                }
            }

            // 2. If child consumed staged token (cur == 0), not yielding, not at cap, and we won arbitration:
            //    borrow 1 token from global_fd and stage it into proxy_fd.
            let cur_after = fifo_bytes(proxy_fd.raw());
            let still_at_cap = max_explicit_extra.is_some_and(|cap| held >= cap);
            if cur_after == 0
                && !need_yield
                && !still_at_cap
                && i_am_winner
                && read_nonblock(global_fd.raw(), 1) == 1
            {
                held += 1;
                let _ = write_tokens(proxy_fd.raw(), 1);
                staged_since = Some(Instant::now());
                did_work = true;
            }
            global_has_tokens = fifo_bytes(global_fd.raw()) > 0;

            let cur_end = fifo_bytes(proxy_fd.raw());
            let borrowed_end = held.saturating_sub(cur_end);
            let at_cap_end = max_explicit_extra.is_some_and(|cap| held >= cap);
            let stale_end = staged_since
                .is_some_and(|t| t.elapsed() >= Duration::from_millis(STAGED_IDLE_MS));
            let wanting_end = !at_cap_end && (cur_end == 0 || !stale_end);
            update_lease(lease_file, borrowed_end, cur_end.min(held), held, wanting_end);
        }

        if did_work {
            idle_sleep_us = 200;
            continue;
        }

        let cur_sleep = fifo_bytes(proxy_fd.raw());
        if cur_sleep == 0 {
            if need_yield || max_explicit_extra.is_some_and(|cap| held >= cap) {
                // Waiting for child to return a token (or wake/stop).
                poll_fds(
                    &[
                        (proxy_fd.raw(), libc::POLLIN),
                        (wake_r.raw(), libc::POLLIN),
                    ],
                    15,
                );
            } else if i_am_winner {
                // Waiting for EITHER child return OR global token availability!
                poll_fds(
                    &[
                        (proxy_fd.raw(), libc::POLLIN),
                        (global_fd.raw(), libc::POLLIN),
                        (wake_r.raw(), libc::POLLIN),
                    ],
                    15,
                );
            } else if global_has_tokens {
                // Another peer has higher priority for the free token; yield CPU briefly so it takes the lock.
                poll_fds(&[(wake_r.raw(), libc::POLLIN)], 1);
            } else {
                poll_fds(
                    &[
                        (proxy_fd.raw(), libc::POLLIN),
                        (global_fd.raw(), libc::POLLIN),
                        (wake_r.raw(), libc::POLLIN),
                    ],
                    15,
                );
            }
        } else {
            // A token is staged in proxy_fd (cur >= 1): adaptively watch for consumption or idle timeout.
            if idle_sleep_us < 1000 {
                thread::sleep(Duration::from_micros(idle_sleep_us));
            } else {
                poll_fds(&[(wake_r.raw(), libc::POLLIN)], (idle_sleep_us / 1000) as i32);
            }
            idle_sleep_us = (idle_sleep_us * 2).min(5_000);
        }
    }

    // Shutdown: drain proxy FIFO, remove lease, and return EXACT `held` tokens to global_fd.
    // Even if the child was SIGKILLed and leaked tokens, `held` is the exact net debt to global_fd.
    let _ = drain_fifo(proxy_fd.raw());
    drop(proxy_fd);
    let _ = fs::remove_file(&proxy_fifo);

    if let Ok(_guard) = crate::locks::acquire(crate::locks::LockKind::Jobserver, &lock_path(&base), true) {
        let _ = fs::remove_file(&lease_path);
        // Check whether other proxies are still alive before returning tokens.
        let (alive_others, _) = scan_leases_locked(&base, budget);
        if alive_others.is_empty() {
            let cur = fifo_bytes(global_fd.raw());
            if cur < target_tokens {
                let _ = write_tokens(global_fd.raw(), target_tokens - cur);
            } else if cur > target_tokens {
                read_nonblock(global_fd.raw(), cur - target_tokens);
            }
        } else if held > 0 {
            let room = target_tokens.saturating_sub(fifo_bytes(global_fd.raw()));
            let _ = write_tokens(global_fd.raw(), held.min(room));
        }
    } else {
        let _ = fs::remove_file(&lease_path);
    }
    drop(global_fd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn temp_state_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "delegate-js-test-{tag}-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pool_tokens_and_fair_shares_derivation() {
        // 96 threads: budget=94, default maxHeavy = max(2, 94/12) = 7 -> pool = 87
        assert_eq!(compute_pool_tokens(94, 7, 7), 87);
        // 96 threads + explicit maxHeavy=6 -> pool = 88
        assert_eq!(compute_pool_tokens(94, 6, 7), 88);
        // 20 threads: budget=18, default maxHeavy = max(2, 18/12) = 2 -> pool = 16
        assert_eq!(compute_pool_tokens(18, 2, 2), 16);
        // 4 threads: budget=2, default maxHeavy = 2 -> pool = 0
        assert_eq!(compute_pool_tokens(2, 2, 2), 0);

        // Fair shares for 1, 2, 3, 6 commands at budget=94
        assert_eq!(compute_fair_shares(94, 1), vec![94]);
        let s2 = compute_fair_shares(94, 2);
        assert_eq!(s2, vec![63, 31]);
        assert_eq!(s2.iter().sum::<usize>(), 94);

        let s3 = compute_fair_shares(94, 3);
        assert_eq!(s3, vec![54, 27, 13]);
        assert_eq!(s3.iter().sum::<usize>(), 94);

        let s6 = compute_fair_shares(94, 6);
        assert_eq!(s6.len(), 6);
        assert_eq!(s6.iter().sum::<usize>(), 94);
        assert!(s6.iter().all(|&x| x >= 2), "floor >= 2: {s6:?}");
        assert!(s6.windows(2).all(|w| w[0] >= w[1]), "monotonic: {s6:?}");
    }

    #[test]
    fn second_command_gets_floor_and_share_within_n_returns_and_scales_to_full_after_first_exits() {
        let state = temp_state_dir("fair-share");
        // Budget = 12, pool = 10 tokens (12 - 2)
        let budget = 12;
        let target = 10;

        let p1 = ProxyGuard::start(state.clone(), target, budget, 1_000, "job1".into(), None)
            .expect("start p1");
        let fd1 = open_fifo_nonblock(p1.proxy_fifo()).unwrap();

        // Command #1 grabs all 10 tokens from the pool.
        let mut held1 = 0usize;
        let deadline = Instant::now() + Duration::from_secs(3);
        while held1 < target && Instant::now() < deadline {
            held1 += read_nonblock(fd1.raw(), target - held1);
            if held1 < target {
                thread::sleep(Duration::from_millis(2));
            }
        }
        assert_eq!(held1, target, "cmd #1 should fill the entire pool when alone");

        // Command #2 arrives while pool is full (shares for budget=12: #1=8 => extra 7, #2=4 => extra 3).
        let p2 = ProxyGuard::start(state.clone(), target, budget, 2_000, "job2".into(), None)
            .expect("start p2");
        let fd2 = open_fifo_nonblock(p2.proxy_fifo()).unwrap();

        // Wait briefly so p1 sees p2's lease and enters yield mode.
        thread::sleep(Duration::from_millis(30));

        // Command #1 returns tokens one by one (simulating rustc completions).
        // Within 4 returns, #2 must reach its floor (>=1 pool token => 2 jobs) and its share (3 pool tokens => 4 jobs).
        let mut held2 = 0usize;
        for step in 0..4 {
            assert_eq!(write_tokens(fd1.raw(), 1), 1);
            let step_deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < step_deadline {
                let got = read_nonblock(fd2.raw(), 4 - held2);
                if got > 0 {
                    held2 += got;
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            if step == 0 {
                assert!(held2 >= 1, "cmd #2 should receive floor token on first return");
            }
            if held2 >= 3 {
                break;
            }
        }
        assert!(held2 >= 3, "cmd #2 should reach its fair share (extra=3), got {held2}");

        // Now command #1 finishes and drops; command #2 must scale up to the full pool (10 tokens).
        drop(fd1);
        drop(p1);

        let scale_deadline = Instant::now() + Duration::from_secs(3);
        while held2 < target && Instant::now() < scale_deadline {
            held2 += read_nonblock(fd2.raw(), target - held2);
            if held2 < target {
                thread::sleep(Duration::from_millis(2));
            }
        }
        assert_eq!(held2, target, "cmd #2 must scale up to full pool after #1 exits");

        drop(fd2);
        drop(p2);
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn underutilized_command_yields_surplus_to_other_command() {
        let state = temp_state_dir("surplus");
        let budget = 12;
        let target = 10;

        // Command #1 starts (senior, share=8), but never consumes tokens (narrow graph).
        let p1 = ProxyGuard::start(state.clone(), target, budget, 1_000, "idle1".into(), None)
            .expect("start p1");
        // Wait past STAGED_IDLE_MS so p1 marks wanting=false (keeping at most 1 pre-staged token).
        thread::sleep(Duration::from_millis(40));

        // Command #2 starts (junior, share=4 => extra 3), demands all available tokens.
        let p2 = ProxyGuard::start(state.clone(), target, budget, 2_000, "busy2".into(), None)
            .expect("start p2");
        let fd2 = open_fifo_nonblock(p2.proxy_fifo()).unwrap();

        let mut held2 = 0usize;
        let deadline = Instant::now() + Duration::from_secs(3);
        while held2 < target - 1 && Instant::now() < deadline {
            held2 += read_nonblock(fd2.raw(), (target - 1) - held2);
            if held2 < target - 1 {
                thread::sleep(Duration::from_millis(2));
            }
        }
        assert!(
            held2 >= target - 1,
            "cmd #2 must absorb idle #1's surplus beyond its own share (3): got {held2}/{target}"
        );

        drop(fd2);
        drop(p2);
        drop(p1);
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn killed_child_and_dead_proxy_replenish_pool_tokens() {
        let state = temp_state_dir("kill-recover");
        let budget = 10;
        let target = 8;

        // Keep an anchor proxy alive so global FIFO buffer persists across #1's death.
        let anchor = ProxyGuard::start(state.clone(), target, budget, 500, "anchor".into(), None)
            .expect("start anchor");
        thread::sleep(Duration::from_millis(40)); // anchor is idle (holds at most 1 staged)

        // Part A: Child leaks 4 tokens (never writes them back, simulating SIGKILL of child) and p1 drops.
        {
            let p1 = ProxyGuard::start(state.clone(), target, budget, 1_000, "victim".into(), None)
                .expect("start victim");
            let fd1 = open_fifo_nonblock(p1.proxy_fifo()).unwrap();
            let mut stolen = 0usize;
            let deadline = Instant::now() + Duration::from_secs(2);
            while stolen < 4 && Instant::now() < deadline {
                stolen += read_nonblock(fd1.raw(), 4 - stolen);
                if stolen < 4 {
                    thread::sleep(Duration::from_millis(2));
                }
            }
            assert_eq!(stolen, 4);
            // Drop without returning bytes to fd1!
            drop(fd1);
            drop(p1);
        }

        // Pool must immediately have all target tokens accounted for (free + anchor.held == target).
        let st_a = reconcile_pool_in(&state, target, budget);
        assert_eq!(st_a.total, target, "child kill must not leak tokens: {st_a:?}");

        // Part B: Simulate a SIGKILLed delegate lane process (unlocked dead lease + missing tokens in global FIFO).
        {
            let _g = crate::locks::acquire(
                crate::locks::LockKind::Jobserver,
                &lock_path(&state),
                true,
            )
            .unwrap();
            let gfd = open_fifo_nonblock(&fifo_path(&state)).unwrap();
            let drained = read_nonblock(gfd.raw(), 3);
            assert_eq!(drained, 3);
            let dead_lease = leases_dir(&state).join("999999-1.lease");
            fs::write(
                &dead_lease,
                format_lease_json(
                    "999999-1",
                    999999,
                    2_000,
                    "crashed-lane",
                    &state.join("lane/js-999999-1.fifo"),
                    3,
                    0,
                    3,
                    true,
                    None,
                ),
            )
            .unwrap();
        }
        let st_b = reconcile_pool_in(&state, target, budget);
        assert_eq!(st_b.total, target, "dead lane lease must replenish tokens: {st_b:?}");
        assert!(st_b.recovered >= 3, "expected >=3 recovered tokens: {st_b:?}");
        assert!(!leases_dir(&state).join("999999-1.lease").exists());

        drop(anchor);
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn real_cargo_respects_three_token_pool_peak_at_most_four() {
        // Real cargo integration test: 8-crate workspace + 3-token pool => peak rustc <= 4.
        let root = temp_state_dir("real-cargo");
        let ws = root.join("ws");
        let state = root.join("state");
        fs::create_dir_all(&ws).unwrap();
        fs::create_dir_all(&state).unwrap();

        let members: Vec<String> = (0..8).map(|i| format!("\"c{i}\"")).collect();
        fs::write(
            ws.join("Cargo.toml"),
            format!("[workspace]\nmembers = [{}]\nresolver = \"2\"\n", members.join(", ")),
        )
        .unwrap();
        for i in 0..8 {
            let cdir = ws.join(format!("c{i}"));
            fs::create_dir_all(cdir.join("src")).unwrap();
            fs::write(
                cdir.join("Cargo.toml"),
                format!("[package]\nname = \"c{i}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            )
            .unwrap();
            fs::write(cdir.join("src/lib.rs"), format!("pub fn f{i}() -> i32 {{ {i} }}\n")).unwrap();
        }

        let log = root.join("rustc.log");
        let wrapper = root.join("wrap.sh");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"--crate-name c\"[0-9]*)\n    echo \"START $$\" >> {}\n    sleep 0.25\n    echo \"END $$\" >> {}\n    ;;\nesac\nexec \"$@\"\n",
                log.display(),
                log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        }

        // Pool with 3 tokens (budget=5, maxHeavy=2 => target_tokens=3).
        let proxy = ProxyGuard::start(state.clone(), 3, 5, 1_000, "real-cargo".into(), None)
            .expect("start proxy");

        let mut cmd = Command::new("cargo");
        cmd.arg("check")
            .arg("--workspace")
            .current_dir(&ws)
            .env_remove("MAKEFLAGS")
            .env_remove("CARGO_MAKEFLAGS")
            .env_remove("CARGO_BUILD_JOBS")
            .env("RUSTC_WRAPPER", &wrapper);
        for (k, v) in proxy.env_vars() {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("run cargo check");
        assert!(
            out.status.success(),
            "cargo check failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        drop(proxy);

        let lines = fs::read_to_string(&log).unwrap_or_default();
        let mut cur = 0usize;
        let mut peak = 0usize;
        let mut starts = 0usize;
        for ln in lines.lines() {
            if ln.starts_with("START ") {
                starts += 1;
                cur += 1;
                peak = peak.max(cur);
            } else if ln.starts_with("END ") {
                cur = cur.saturating_sub(1);
            }
        }
        assert_eq!(starts, 8, "all 8 crates should compile via wrapper");
        assert!(peak <= 4, "3 tokens + 1 implicit must cap rustc peak at <= 4, got {peak}");
        assert!(peak >= 2, "parallel build should use tokens (peak >= 2), got {peak}");

        // After exit, pool reconciles cleanly to 3/3 free.
        let st = reconcile_pool_in(&state, 3, 5);
        assert_eq!((st.total, st.free), (3, 3));

        let _ = fs::remove_dir_all(&root);
    }
}
