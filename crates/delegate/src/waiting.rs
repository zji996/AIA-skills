use crate::common::*;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

pub const COVERED: i32 = 76;

// Keep the lock inode: unlinking it allows a new waiter to lock a different file.
pub fn claim(runs: &[PathBuf]) -> Res<(Vec<PathBuf>, Vec<File>)> {
    let mut claimed = Vec::new();
    let mut held = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for run in runs {
        if !seen.insert(run) {
            continue;
        }
        let path = run.join("waiter.lock");
        match lock(&path, true, true) {
            Ok(mut file) => {
                file.set_len(0).map_err(|e| e.to_string())?;
                writeln!(file, "{}", std::process::id()).map_err(|e| e.to_string())?;
                held.push(file);
                claimed.push(run.clone());
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                eprintln!(
                    "delegate: {} 已由 pid {} 等待，结束时会通知那一个 (exit {COVERED})",
                    run.file_name().unwrap_or_default().to_string_lossy(),
                    read(&path).trim()
                );
            }
            Err(e) => {
                eprintln!("delegate: cannot lock waiter: {e}");
                return Err(e.to_string());
            }
        }
    }
    Ok((claimed, held))
}
