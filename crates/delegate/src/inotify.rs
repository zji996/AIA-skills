//! Lightweight Linux `inotify` watcher for `<state>` directory changes (`events.seq`,
//! `events.jsonl`, `index.json`, `work.jsonl`). Zero external crate dependencies — uses
//! `libc::inotify_init1`, `libc::inotify_add_watch`, and `libc::poll`.
//!
//! Watching the parent directory with `IN_MOVED_TO | IN_CLOSE_WRITE | IN_CREATE | IN_MODIFY`
//! catches every atomic rename replacement (`rename(tmp, target)`) as well as appends, and works
//! even before the target files exist.

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct StateWatcher {
    fd: i32,
    _wd: i32,
    _dir: PathBuf,
}

impl StateWatcher {
    /// Start watching `dir`. Returns `None` if `inotify` is unavailable (callers fall back to sleep).
    pub fn watch(dir: &Path) -> Option<Self> {
        let _ = fs::create_dir_all(dir);
        let cpath = CString::new(dir.as_os_str().as_bytes()).ok()?;
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let mask = libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE | libc::IN_CREATE | libc::IN_MODIFY;
        let wd = unsafe { libc::inotify_add_watch(fd, cpath.as_ptr(), mask) };
        if wd < 0 {
            unsafe {
                libc::close(fd);
            }
            return None;
        }
        Some(Self {
            fd,
            _wd: wd,
            _dir: dir.to_path_buf(),
        })
    }

    /// Block until any watched file changes or `timeout` elapses.
    /// Returns `true` if one or more inotify events were observed, `false` on timeout / interrupt.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc <= 0 {
            return false;
        }
        self.drain();
        true
    }

    /// Drain any queued inotify events without blocking.
    pub fn drain(&self) {
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
        }
    }
}

impl Drop for StateWatcher {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe {
                libc::close(self.fd);
            }
            self.fd = -1;
        }
    }
}

/// Wait for a change in `watcher` (if available) or sleep up to `timeout`.
pub fn wait_or_sleep(watcher: Option<&StateWatcher>, timeout: Duration) -> bool {
    if let Some(w) = watcher {
        w.wait_timeout(timeout)
    } else {
        std::thread::sleep(timeout);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn watcher_wakes_promptly_on_atomic_rename() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "delegate-inotify-test-{nanos}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let watcher = StateWatcher::watch(&dir).expect("inotify available on Linux");
        let target = dir.join("events.seq");
        let tmp = dir.join(".events.seq.tmp");
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            fs::write(&tmp, "1\n").unwrap();
            fs::rename(&tmp, &target).unwrap();
        });
        let t0 = Instant::now();
        let woke = watcher.wait_timeout(Duration::from_secs(2));
        let elapsed = t0.elapsed();
        writer.join().unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(woke, "expected inotify wakeup");
        assert!(
            elapsed < Duration::from_millis(500),
            "inotify woke too slowly: {elapsed:?}"
        );
    }
}
