use crate::common::*;
use std::io::Write;
use std::path::PathBuf;

pub const COVERED: i32 = 76;

// Keep the lock inode: unlinking it allows a new waiter to lock a different file.
pub fn claim(
    runs: &[PathBuf],
    exclusive: bool,
) -> Res<(Vec<PathBuf>, Vec<crate::locks::LockGuard>)> {
    let mut claimed = Vec::new();
    let mut held = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for run in runs {
        if !seen.insert(run) {
            continue;
        }
        let path = run.join("waiter.lock");
        match crate::locks::try_acquire_opts(
            crate::locks::LockKind::WaiterClaim,
            &path,
            true,
            false,
        ) {
            Ok(mut guard) => {
                let file = guard.file_mut();
                file.set_len(0).map_err(|e| e.to_string())?;
                writeln!(file, "{}", std::process::id()).map_err(|e| e.to_string())?;
                held.push(guard);
                claimed.push(run.clone());
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let name = run.file_name().unwrap_or_default().to_string_lossy();
                let holder = read(&path);
                let holder = holder.trim();
                if exclusive {
                    eprintln!(
                        "delegate: {name} 已由 pid {holder} 等待，结束时会通知那一个 (exit {COVERED})"
                    );
                } else {
                    let attach_path = run.join("waiter-attach.lock");
                    match crate::locks::try_acquire_opts(
                        crate::locks::LockKind::WaiterClaim,
                        &attach_path,
                        false,
                        false,
                    ) {
                        Ok(guard) => {
                            eprintln!("delegate: {name} 已由 pid {holder} 等待，附着等待结果");
                            held.push(guard);
                            claimed.push(run.clone());
                        }
                        Err(ae) => {
                            eprintln!("delegate: cannot attach waiter lock: {ae}");
                            return Err(ae.to_string());
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("delegate: cannot lock waiter: {e}");
                return Err(e.to_string());
            }
        }
    }
    Ok((claimed, held))
}
