//! Worktree pool: a write task reuses one of a few fixed worktree paths instead of
//! creating a new one, so build caches inside the worktree (`target/`, incremental
//! state) stay valid. Cargo keys a path package by its absolute path and decides
//! freshness by file mtimes, so the slot must keep its path and switch commits with
//! `git checkout`, which rewrites only the files that differ.
//!
//! A slot belongs to the run whose directory its owner file names. The claim is
//! released when that run is cleaned (or applied and cleaned), and a slot whose
//! owner run no longer exists or points elsewhere is free again, so a crash or a
//! removed run directory never leaks a slot.
use crate::common::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Upper bound for `worktree.pool`; more slots than heavy lanes buy nothing.
pub const MAX_SLOTS: u64 = 16;

pub fn root() -> PathBuf {
    cache_dir().join("pool")
}

fn owner_file(slot: &Path) -> PathBuf {
    slot.with_file_name(format!(
        "{}.owner",
        slot.file_name().unwrap_or_default().to_string_lossy()
    ))
}

pub fn is_slot(path: &Path) -> bool {
    path.parent() == Some(root().as_path())
}

/// A slot is held while its owner run, or any other run record (a reply continuing in
/// it), still points at it.
fn claimed(slot: &Path) -> bool {
    let points = |run: &Path| {
        run.is_dir()
            && s(&json(run.join("meta.json"))["worktree"], "path") == slot.to_string_lossy()
    };
    // Between the claim and its meta.json the owner run exists without a record yet.
    let pending = |run: &Path| run.is_dir() && !run.join("meta.json").exists();
    let owner = fs::read_to_string(owner_file(slot))
        .map(|owner| PathBuf::from(owner.trim()))
        .ok();
    owner
        .as_deref()
        .is_some_and(|run| points(run) || pending(run))
        || crate::runs::all_runs().iter().any(|run| points(run))
}

/// Claim a free slot for `run` among `slots` slots of `source`; `None` when all are held.
pub fn claim(source: &Path, run: &Path, slots: u64) -> Option<PathBuf> {
    fs::create_dir_all(root()).ok()?;
    let _guard = lock(&root().join(".claim.lock"), true, false).ok()?;
    let repo = source.file_name().unwrap_or_default().to_string_lossy();
    (1..=slots.min(MAX_SLOTS)).find_map(|n| {
        let slot = root().join(format!("{repo}-{n}"));
        if claimed(&slot) {
            return None;
        }
        fs::write(owner_file(&slot), run.to_string_lossy().as_bytes()).ok()?;
        Some(slot)
    })
}

/// Give a slot back. Only its owner run may: a stale record that shares the path
/// with the slot's current task must not free it.
pub fn release(slot: &Path, run: &Path) {
    let file = owner_file(slot);
    if fs::read_to_string(&file).is_ok_and(|owner| Path::new(owner.trim()) == run) {
        let _ = fs::remove_file(file);
    }
}

fn git_in(dir: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(clip(String::from_utf8_lossy(&out.stderr).trim(), 300))
    }
}

/// Bring a reused slot to `commit` touching only differing files. Managed copies and
/// links are removed first (prepare recreates them), every other untracked or ignored
/// path except `target/` goes, and the checkout is detached. `Ok(false)` means the
/// slot holds no usable worktree and the caller must add one.
pub fn reset(slot: &Path, commit: &str, managed: &[String]) -> Result<bool, String> {
    if !slot.join(".git").exists() {
        if slot.exists() {
            fs::remove_dir_all(slot).map_err(|e| e.to_string())?;
        }
        return Ok(false);
    }
    for item in managed {
        let path = slot.join(item);
        if path.is_symlink() || path.is_file() {
            let _ = fs::remove_file(&path);
        } else if path.is_dir() {
            let _ = fs::remove_dir_all(&path);
        }
        let _ = git_in(slot, &["update-index", "--no-skip-worktree", "--", item]);
    }
    // Refresh first: a file whose mtime moved but whose content did not would otherwise be
    // rewritten by the hard reset, and cargo would rebuild everything that includes it.
    let _ = git_in(slot, &["update-index", "-q", "--refresh"]);
    let step = git_in(slot, &["reset", "-q", "--hard"])
        .and_then(|()| git_in(slot, &["clean", "-ffdxq", "-e", "/target"]))
        .and_then(|()| git_in(slot, &["checkout", "-q", "--detach", commit]));
    match step {
        Ok(()) => {
            trim_target(slot);
            Ok(true)
        }
        Err(error) => {
            // A slot git cannot bring back is rebuilt from scratch rather than reused dirty.
            eprintln!(
                "delegate: pool slot {} reset failed ({error}); recreating it",
                slot.display()
            );
            let _ = fs::remove_dir_all(slot);
            Ok(false)
        }
    }
}

/// A slot's `target/` only grows (old artifacts stay beside new ones); past the limit
/// (`DELEGATE_POOL_TARGET_GIB`, default 60) it is emptied once and rebuilt cold.
fn trim_target(slot: &Path) {
    let target = slot.join("target");
    let limit_gib: u64 = setting("POOL_TARGET_GIB", "60").parse().unwrap_or(60);
    let Ok(out) = Command::new("du").arg("-sb").arg(&target).output() else {
        return;
    };
    let used: u64 = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    if used > limit_gib << 30 {
        eprintln!(
            "delegate: pool slot {} target is {} GiB (limit {limit_gib}); emptying it",
            slot.display(),
            used >> 30
        );
        let _ = fs::remove_dir_all(&target);
    }
}

#[cfg(test)]
mod tests {
    use super::reset;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Reuse keeps unchanged files' mtimes and `target/`, drops other leftovers, and lands
    /// on the new commit; that is what lets cargo rebuild only what changed.
    #[test]
    fn a_reused_slot_touches_only_changed_files_and_keeps_target() {
        let base = std::env::temp_dir().join(format!("delegate-pool-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let repo = base.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        fs::write(repo.join("same.rs"), "fn same() {}").unwrap();
        fs::write(repo.join("untouched.rs"), "fn untouched() {}").unwrap();
        fs::write(repo.join("changed.rs"), "fn old() {}").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "a"]);
        let first = git(&repo, &["rev-parse", "HEAD"]);
        fs::write(repo.join("changed.rs"), "fn new() {}").unwrap();
        git(&repo, &["commit", "-qam", "b"]);
        let second = git(&repo, &["rev-parse", "HEAD"]);

        let slot = base.join("slot-1");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                &slot.to_string_lossy(),
                &first,
            ],
        );
        let old = SystemTime::now() - Duration::from_secs(3600);
        let same = fs::File::options()
            .write(true)
            .open(slot.join("same.rs"))
            .unwrap();
        same.set_modified(old).unwrap();
        let untouched = fs::File::options()
            .write(true)
            .open(slot.join("untouched.rs"))
            .unwrap();
        untouched.set_modified(old).unwrap();
        fs::create_dir_all(slot.join("target/debug")).unwrap();
        fs::write(slot.join("target/debug/artifact"), "built").unwrap();
        fs::write(slot.join("leftover.txt"), "from the last task").unwrap();
        fs::write(slot.join("same.rs"), "fn edited_by_last_task() {}").unwrap();
        same.set_modified(old).unwrap();

        assert!(reset(&slot, &second, &[]).unwrap());
        assert_eq!(
            fs::read_to_string(slot.join("changed.rs")).unwrap(),
            "fn new() {}"
        );
        assert_eq!(
            fs::read_to_string(slot.join("same.rs")).unwrap(),
            "fn same() {}"
        );
        assert!(slot.join("target/debug/artifact").exists());
        assert!(!slot.join("leftover.txt").exists());
        assert_eq!(git(&slot, &["rev-parse", "HEAD"]), second);
        let kept = fs::metadata(slot.join("untouched.rs"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(kept, old, "an unchanged file must keep its mtime");
        let _ = fs::remove_dir_all(&base);
    }
}
