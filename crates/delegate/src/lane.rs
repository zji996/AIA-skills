use crate::common::*;
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

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
pub fn max_heavy() -> Res<usize> {
    Ok(number("MAX_HEAVY", 1)? as usize)
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
fn tickets() -> Vec<(PathBuf, Value)> {
    let Ok(entries) = fs::read_dir(lane_dir()) else {
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
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
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
pub struct Slot {
    ticket: Option<PathBuf>,
    _file: Option<File>,
    pub queued: f64,
}
impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(p) = self.ticket.take() {
            let _ = fs::remove_file(p);
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
    mut waiting: impl FnMut(usize, &[String]),
) -> Res<Slot> {
    let limit = max_heavy()?;
    if limit == 0 || std::env::var_os("DELEGATE_LANE_HELD").is_some() {
        return Ok(Slot {
            ticket: None,
            _file: None,
            queued: 0.0,
        });
    }
    let dir = lane_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let me = json!({"pid":std::process::id(),"label":label,"since":iso(),"epoch":epoch()});
    let ticket;
    let file;
    {
        let _guard = lock(&dir.join(".lock"), true, false).map_err(|e| e.to_string())?;
        ticket = dir.join(format!("{:020}-{}.ticket", now_ns(), std::process::id()));
        file = locked_file(&ticket, &me.to_string())?;
    }
    let marker = account.map(|p| p.join(format!("lane-waiting-{}", std::process::id())));
    let markfile = marker
        .as_ref()
        .map(|m| locked_file(m, &me.to_string()))
        .transpose()?;
    let started = Instant::now();
    let outcome = (|| {
        loop {
            let live = tickets();
            let pos = live.iter().position(|(p, _)| p == &ticket).unwrap_or(0);
            if pos < limit {
                break;
            }
            let labels: Vec<String> = live[..pos]
                .iter()
                .map(|(_, v)| s(v, "label").to_string())
                .collect();
            waiting(pos - limit + 1, &labels);
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
    if let Some(p) = marker {
        let _ = fs::remove_file(p);
    }
    drop(markfile);
    if let Some(a) = account {
        let _ = append(a.join("lane-wait"), &format!("{queued:.1}\n"));
    }
    if let Err(e) = outcome {
        let _ = fs::remove_file(ticket);
        return Err(e);
    }
    Ok(Slot {
        ticket: Some(ticket),
        _file: Some(file),
        queued,
    })
}
pub fn queued_seconds(run: &Path) -> f64 {
    let mut total = read(run.join("lane-wait"))
        .split_whitespace()
        .filter_map(|x| x.parse::<f64>().ok())
        .sum();
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
pub fn lane_command(args: &[String]) -> Res<i32> {
    let mut i = 0;
    let mut label = None;
    while i < args.len() {
        if args[i] == "--label" {
            i += 1;
            if i >= args.len() {
                return Err("argument --label: expected one argument".into());
            }
            label = Some(args[i].clone());
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
        for (label, since, running) in holders()? {
            println!(
                "{:<8} {since}  {label}",
                if running { "running" } else { "queued" }
            );
        }
        return Ok(0);
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
    let slot = acquire(&label, account.as_deref(), |ahead, labels| {
        if !shown {
            eprintln!(
                "delegate lane: queued behind {ahead}: {}",
                labels.join("; ")
            );
            shown = true;
        }
    })?;
    if shown {
        eprintln!(
            "delegate lane: started after {:.0}s in the queue",
            slot.queued
        );
    }
    let mut c = Command::new("sh");
    c.arg("-c")
        .arg(command)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env("DELEGATE_LANE_HELD", "1");
    group(&mut c);
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let pid = child.id() as i32;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            let sig = SIGNAL.load(Ordering::SeqCst);
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
            return Ok(128 + sig);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
