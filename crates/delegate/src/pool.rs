//! Worktree pool: a write task reuses one of a few fixed worktree paths instead of
//! creating a new one, so build caches inside the worktree (`target/`, incremental
//! state) stay valid. Cargo keys a path package by its absolute path and decides
//! freshness by file mtimes, so the slot must keep its path and switch commits with
//! `git checkout`, which rewrites only the files that differ.
//!
//! A slot belongs to the run whose directory its owner file names (and any reply chain
//! continuing from it). The claim is released when that run/chain is cleaned or applied
//! with no unapplied/active sharers left, and a slot whose owner run no longer exists or
//! points elsewhere is free again, so a crash or a removed run directory never leaks a slot.
//!
//! There is no per-slot target size cap: build caches grow freely while the disk has room,
//! and space is governed at the whole-disk level by `delegate disk prune` and optional
//! `overflowRoot` spill.
use crate::common::*;
use crate::disk::{self, DiskConfig, RealSpaceProbe, SpaceProbe, TargetDecision};
use std::fs;
use std::path::{Path, PathBuf};

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

/// Return the run directory currently recorded as owning `slot`, if any.
pub fn owner_of(slot: &Path) -> Option<PathBuf> {
    let raw = fs::read_to_string(owner_file(slot)).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// True when `owner` is `run` itself or a claiming ancestor in `run`'s reply chain.
pub fn owner_matches_run(owner: &Path, run: &Path) -> bool {
    let owner_canon = fs::canonicalize(owner).unwrap_or_else(|_| owner.to_path_buf());
    let run_canon = fs::canonicalize(run).unwrap_or_else(|_| run.to_path_buf());
    if owner_canon == run_canon {
        return true;
    }
    let Some(owner_name) = owner.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let meta = json(run.join("meta.json"));
    let claimed_by = s(&meta["worktree"], "claimedBy");
    if !claimed_by.is_empty() && claimed_by == owner_name {
        return true;
    }
    // Walk `parent` chain in case `claimedBy` was not populated on an intermediate round.
    let mut cur_parent = s(&meta, "parent").to_string();
    let mut seen = std::collections::HashSet::new();
    let run_parent_dir = run.parent().map(Path::to_path_buf);
    for _ in 0..64 {
        if cur_parent.is_empty() || !seen.insert(cur_parent.clone()) {
            break;
        }
        if cur_parent == owner_name {
            return true;
        }
        let parent_dir = run_parent_dir
            .as_ref()
            .map(|d| d.join(&cur_parent))
            .filter(|p| p.join("meta.json").is_file())
            .or_else(|| {
                let p = runs_root().join(&cur_parent);
                p.join("meta.json").is_file().then_some(p)
            });
        let Some(pdir) = parent_dir else {
            break;
        };
        let pmeta = json(pdir.join("meta.json"));
        let p_claimed = s(&pmeta["worktree"], "claimedBy");
        if !p_claimed.is_empty() && p_claimed == owner_name {
            return true;
        }
        cur_parent = s(&pmeta, "parent").to_string();
    }
    false
}

/// True when `slot` is still owned by `run` (or the claiming ancestor of `run`'s reply chain)
/// and has not been released or reassigned to another run.
pub fn slot_belongs_to_run(slot: &Path, run: &Path, meta: &serde_json::Value) -> bool {
    let Some(owner) = owner_of(slot) else {
        return false;
    };
    let owner_canon = fs::canonicalize(&owner).unwrap_or_else(|_| owner.clone());
    let run_canon = fs::canonicalize(run).unwrap_or_else(|_| run.to_path_buf());
    if owner_canon == run_canon {
        return true;
    }
    let claimed_by = s(&meta["worktree"], "claimedBy");
    if !claimed_by.is_empty()
        && owner
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == claimed_by)
        && owner.is_dir()
        && !owner.join(".applied-cleaned").exists()
    {
        return true;
    }
    owner_matches_run(&owner, run) && owner.is_dir() && !owner.join(".applied-cleaned").exists()
}

fn run_holds_slot(run: &Path, slot: &Path) -> bool {
    if !run.is_dir() {
        return false;
    }
    let meta = json(run.join("meta.json"));
    if s(&meta["worktree"], "path") != slot.to_string_lossy() {
        return false;
    }
    // An applied inactive run (without pending codegen) does not keep a released slot claimed.
    if run.join(".applied").exists()
        && !run.join(".generate-pending").exists()
        && !crate::runs::active(&crate::runs::state(run))
        && !crate::runs::agent_alive(run)
    {
        return false;
    }
    true
}

/// A slot is held while its owner run, or any other active/unapplied run record (a reply
/// continuing in it), still points at it.
fn claimed(slot: &Path) -> bool {
    // Between the claim and its finalized meta.json the owner run exists without worktree.path yet.
    let pending = |run: &Path| {
        run.is_dir()
            && !run.join("exit_code").exists()
            && (!run.join("meta.json").exists()
                || s(&json(run.join("meta.json"))["worktree"], "path").is_empty())
    };
    let owner = fs::read_to_string(owner_file(slot))
        .map(|owner| PathBuf::from(owner.trim()))
        .ok();
    owner
        .as_deref()
        .is_some_and(|run| run_holds_slot(run, slot) || pending(run))
        || crate::runs::all_runs()
            .iter()
            .any(|run| run_holds_slot(run, slot))
}

pub fn is_slot_held(slot: &Path) -> bool {
    claimed(slot)
}

/// Claim a free slot for `run` among `slots` slots of `source`; `None` when all are held.
pub fn claim(source: &Path, run: &Path, slots: u64) -> Option<PathBuf> {
    fs::create_dir_all(root()).ok()?;
    let _guard = crate::locks::acquire(
        crate::locks::LockKind::PoolClaim,
        &root().join(".claim.lock"),
        true,
    )
    .ok()?;
    let repo = source.file_name().unwrap_or_default().to_string_lossy();
    (1..=slots.min(MAX_SLOTS)).find_map(|n| {
        let slot = root().join(format!("{repo}-{n}"));
        if claimed(&slot) {
            return None;
        }
        atomic_write(owner_file(&slot), run.to_string_lossy().as_bytes()).ok()?;
        Some(slot)
    })
}

fn release_owner_only(slot: &Path, run: &Path) -> bool {
    let file = owner_file(slot);
    if let Ok(raw) = fs::read_to_string(&file) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() && owner_matches_run(Path::new(trimmed), run) {
            let _ = fs::remove_file(&file);
            return true;
        }
    }
    false
}

/// Give a slot back. Only its owner run (or a reply in the owner's chain) may: a stale
/// record that shares the path with the slot's current task must not free it.
pub fn release(slot: &Path, run: &Path) {
    match disk::load_config() {
        Ok(cfg) => {
            release_with(slot, run, &cfg, &RealSpaceProbe, &root());
        }
        Err(err) => {
            // Release ownership so a broken disk config does not leak slots, but do NOT
            // silently substitute defaults for target relocation/pruning.
            let _ = release_owner_only(slot, run);
            eprintln!("delegate: disk config error during pool release: {err}");
        }
    }
}

pub fn release_with(
    slot: &Path,
    run: &Path,
    cfg: &DiskConfig,
    probe: &dyn SpaceProbe,
    pool_root: &Path,
) -> bool {
    let released = release_owner_only(slot, run);
    if released && !claimed(slot) && !disk::is_slot_busy(slot, cfg) {
        let target = slot.join("target");
        let real_target = fs::canonicalize(&target).unwrap_or(target);
        if real_target.is_dir() {
            let id = format!(
                "slot:{}",
                slot.file_name().unwrap_or_default().to_string_lossy()
            );
            let _ = disk::zero_cost_cleanup(&real_target, Some(slot), true, &id, false);
        }
        let _ = disk::maybe_return_slot_to_primary(slot, cfg, probe, pool_root);
    }
    released
}

/// Release a pool slot immediately when `run` has `.applied` (no `.generate-pending`) and no
/// active or unapplied sharer remains on the same worktree path.
pub fn release_if_applied_chain(run: &Path) -> bool {
    if !run.is_dir() || !run.join(".applied").exists() || run.join(".generate-pending").exists() {
        return false;
    }
    let meta = json(run.join("meta.json"));
    let wt_path = s(&meta["worktree"], "path").to_string();
    if wt_path.is_empty() || wt_path.ends_with("#released") {
        return false;
    }
    let slot = Path::new(&wt_path);
    if !is_slot(slot) {
        return false;
    }
    let has_active_or_unapplied_sharer = crate::runs::all_runs().iter().any(|other| {
        let om = json(other.join("meta.json"));
        s(&om["worktree"], "path") == wt_path
            && (crate::runs::active(&crate::runs::state(other))
                || crate::runs::agent_alive(other)
                || !other.join(".applied").exists()
                || other.join(".generate-pending").exists())
    });
    if has_active_or_unapplied_sharer {
        return false;
    }
    match disk::load_config() {
        Ok(cfg) => release_with(slot, run, &cfg, &RealSpaceProbe, &root()),
        Err(err) => {
            let released = release_owner_only(slot, run);
            eprintln!("delegate: disk config error during pool release: {err}");
            released
        }
    }
}

fn git_in(dir: &Path, args: &[&str]) -> Result<(), String> {
    let out = git_command()
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
    let cfg = disk::load_config()?;
    reset_with_config(slot, commit, managed, &cfg, &RealSpaceProbe, &root())
}

fn reset_with_config(
    slot: &Path,
    commit: &str,
    managed: &[String],
    cfg: &DiskConfig,
    probe: &dyn SpaceProbe,
    pool_root: &Path,
) -> Result<bool, String> {
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
            attach_target_with(
                slot,
                cfg,
                probe,
                pool_root,
                Path::new("/proc"),
                &state_dir().join("lane"),
                &is_slot_held,
            )?;
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

/// Put a slot's `target/` where the disk config says: `<primaryRoot>/<slot>/target`, or
/// `<overflowRoot>/<slot>/target` when primary is at high watermark with no evictable
/// non-busy bytes (and overflow has headroom), or back to primary when primary falls to
/// `<= returnPct`. The symlink survives `git clean` (`/target` is excluded) and is
/// transparent to Cargo.
pub fn attach_target(slot: &Path) -> Result<(), String> {
    let cfg = disk::load_config()?;
    attach_target_with(
        slot,
        &cfg,
        &RealSpaceProbe,
        &root(),
        Path::new("/proc"),
        &state_dir().join("lane"),
        &is_slot_held,
    )
}

pub fn attach_target_with(
    slot: &Path,
    cfg: &DiskConfig,
    probe: &dyn SpaceProbe,
    pool_root: &Path,
    proc_root: &Path,
    lane_dir: &Path,
    slot_held_fn: &dyn Fn(&Path) -> bool,
) -> Result<(), String> {
    let decision =
        disk::decide_slot_target(slot, cfg, probe, pool_root, proc_root, lane_dir, slot_held_fn);
    let link = slot.join("target");
    match decision {
        TargetDecision::Local => {
            if link.is_symlink() {
                fs::remove_file(&link).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
        TargetDecision::Primary(dest) | TargetDecision::Overflow(dest) => {
            if link.is_symlink() && fs::read_link(&link).is_ok_and(|to| to == dest) {
                fs::create_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
                return Ok(());
            }
            fs::create_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
            if link.is_symlink() || link.is_file() {
                fs::remove_file(&link).map_err(|e| e.to_string())?;
            } else if link.is_dir() {
                // Builds moved off this disk: the old local cache is not carried over.
                fs::remove_dir_all(&link).map_err(|e| e.to_string())?;
            }
            std::os::unix::fs::symlink(&dest, &link).map_err(|e| e.to_string())
        }
        TargetDecision::ReturnToPrimary {
            new_dest,
            old_overflow_dir,
        } => {
            if let Some(dest) = new_dest {
                fs::create_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
                disk::atomic_replace_symlink(&dest, &link)?;
            } else if link.is_symlink() {
                fs::remove_file(&link).map_err(|e| e.to_string())?;
            }
            disk::cleanup_overflow_slot_dir(&old_overflow_dir, cfg.overflow_root.as_deref());
            Ok(())
        }
    }
}

/// Zero-cost maintenance on slot reuse: drop superseded incremental sessions, stale-hashed
/// own-crate artifacts, and test executables from the previous task. No per-slot size cap
/// is enforced; whole-disk watermarks are enforced by `delegate disk prune`.
fn trim_target(slot: &Path) {
    let target = slot.join("target");
    let real_target = fs::canonicalize(&target).unwrap_or(target);
    if !real_target.is_dir() {
        return;
    }
    let id = format!(
        "slot:{}",
        slot.file_name().unwrap_or_default().to_string_lossy()
    );
    let stats = disk::zero_cost_cleanup(&real_target, Some(slot), true, &id, false);
    if stats.total_bytes() >= 1 << 30 {
        eprintln!(
            "delegate: pool slot {} dropped {} GiB of zero-cost stale build artifacts",
            slot.display(),
            stats.total_bytes() >> 30
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{attach_target_with, owner_matches_run, release_with, reset_with_config, trim_target};
    use crate::common::{git_command, write_json};
    use crate::disk::{DiskConfig, DiskRole, DiskSpace, SpaceProbe};
    use serde_json::json;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    struct FixedProbe {
        primary: DiskSpace,
        overflow: DiskSpace,
    }

    impl SpaceProbe for FixedProbe {
        fn space(&self, role: DiskRole, _path: &Path) -> Option<DiskSpace> {
            Some(match role {
                DiskRole::Primary => self.primary,
                DiskRole::Overflow => self.overflow,
            })
        }
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = git_command()
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

        let cfg = DiskConfig::default();
        let probe = FixedProbe {
            primary: DiskSpace::from_total_avail(1000, 500),
            overflow: DiskSpace::from_total_avail(1000, 800),
        };
        assert!(reset_with_config(&slot, &second, &[], &cfg, &probe, &base).unwrap());
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

    /// With single-slot limits removed, a large warm slot is NOT evicted by per-slot LRU;
    /// only zero-cost garbage (superseded sessions, old hashes, released execs) is pruned.
    #[test]
    fn large_slot_is_not_evicted_by_per_slot_cap() {
        let base = std::env::temp_dir().join(format!("delegate-nocap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let slot = base.join("repo-1");
        let debug = slot.join("target/debug");
        fs::create_dir_all(debug.join("deps")).unwrap();
        fs::create_dir_all(debug.join(".fingerprint/old_3p-1111222233334444")).unwrap();
        fs::create_dir_all(debug.join("incremental/foo-abc/s-aaa-111-svh")).unwrap();
        fs::create_dir_all(debug.join("incremental/foo-abc/s-bbb-222-svh")).unwrap();

        // Warm 3p artifact with very old timestamps — would have been evicted by old per-slot LRU.
        let warm_lib = debug.join("deps/libold_3p-1111222233334444.rlib");
        fs::write(&warm_lib, vec![1u8; 512 << 10]).unwrap();
        let old = SystemTime::now() - Duration::from_secs(30 * 86400);
        let times = fs::FileTimes::new().set_accessed(old).set_modified(old);
        fs::File::options()
            .write(true)
            .open(&warm_lib)
            .unwrap()
            .set_times(times)
            .unwrap();

        // Superseded session should be pruned (zero-cost).
        fs::write(
            debug.join("incremental/foo-abc/s-aaa-111-svh/query-cache.bin"),
            vec![0u8; 8192],
        )
        .unwrap();
        fs::write(
            debug.join("incremental/foo-abc/s-bbb-222-svh/query-cache.bin"),
            vec![0u8; 8192],
        )
        .unwrap();

        trim_target(&slot);

        assert!(
            warm_lib.exists(),
            "per-slot cap is gone: warm artifacts must stay intact"
        );
        assert!(
            !debug.join("incremental/foo-abc/s-aaa-111-svh").exists(),
            "superseded incremental session must be pruned"
        );
        assert!(
            debug.join("incremental/foo-abc/s-bbb-222-svh").exists(),
            "newest incremental session must be kept"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// With a target root the slot's `target` links to `<root>/<slot>/target`, replacing a
    /// local directory; without one the link is dropped.
    #[test]
    fn a_target_root_links_the_slot_target_and_unset_drops_the_link() {
        let base = std::env::temp_dir().join(format!("delegate-troot-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let slot = base.join("repo-1");
        fs::create_dir_all(slot.join("target/debug")).unwrap();
        let root = base.join("fast-disk");
        let probe = FixedProbe {
            primary: DiskSpace::from_total_avail(1000, 600),
            overflow: DiskSpace::from_total_avail(1000, 800),
        };
        let cfg = DiskConfig {
            primary_root: Some(root.clone()),
            min_free_gib: 0,
            ..Default::default()
        };

        attach_target_with(&slot, &cfg, &probe, &base, Path::new("/nonexistent"), &base, &|_| false)
            .unwrap();
        attach_target_with(&slot, &cfg, &probe, &base, Path::new("/nonexistent"), &base, &|_| false)
            .unwrap();
        assert_eq!(
            fs::read_link(slot.join("target")).unwrap(),
            root.join("repo-1/target")
        );
        assert!(root.join("repo-1/target").is_dir());

        let unset_cfg = DiskConfig {
            min_free_gib: 0,
            ..Default::default()
        };
        attach_target_with(
            &slot,
            &unset_cfg,
            &probe,
            &base,
            Path::new("/nonexistent"),
            &base,
            &|_| false,
        )
        .unwrap();
        assert!(!slot.join("target").exists() && !slot.join("target").is_symlink());
        let _ = fs::remove_dir_all(&base);
    }

    /// Reply chain releases a slot owned by its claiming ancestor, while an unrelated run
    /// cannot steal/release the slot.
    #[test]
    fn release_recognizes_reply_chain_ancestor_owner_and_rejects_unrelated_run() {
        let base = std::env::temp_dir().join(format!("delegate-rel-chain-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let pool = base.join("pool");
        let runs = base.join("runs");
        let slot = pool.join("demo-1");
        fs::create_dir_all(&slot).unwrap();

        let r1 = runs.join("20261126-100000-root");
        let r2 = runs.join("20261126-101000-reply1");
        let r3 = runs.join("20261126-102000-reply2");
        let stranger = runs.join("20261126-103000-stranger");
        for d in [&r1, &r2, &r3, &stranger] {
            fs::create_dir_all(d).unwrap();
        }
        write_json(
            r1.join("meta.json"),
            &json!({"run": "20261126-100000-root", "worktree": {"path": slot, "claimedBy": "20261126-100000-root"}}),
        )
        .unwrap();
        write_json(
            r2.join("meta.json"),
            &json!({"run": "20261126-101000-reply1", "parent": "20261126-100000-root", "worktree": {"path": slot, "claimedBy": "20261126-100000-root"}}),
        )
        .unwrap();
        // r3 omits claimedBy to verify parent-chain walk also works.
        write_json(
            r3.join("meta.json"),
            &json!({"run": "20261126-102000-reply2", "parent": "20261126-101000-reply1", "worktree": {"path": slot}}),
        )
        .unwrap();
        write_json(
            stranger.join("meta.json"),
            &json!({"run": "20261126-103000-stranger", "worktree": {"path": slot, "claimedBy": "20261126-103000-stranger"}}),
        )
        .unwrap();

        let owner_file = pool.join("demo-1.owner");
        fs::write(&owner_file, r1.to_string_lossy().as_bytes()).unwrap();

        assert!(owner_matches_run(&r1, &r2));
        assert!(owner_matches_run(&r1, &r3));
        assert!(!owner_matches_run(&r1, &stranger));

        let cfg = DiskConfig::default();
        let probe = FixedProbe {
            primary: DiskSpace::from_total_avail(1000, 600),
            overflow: DiskSpace::from_total_avail(1000, 800),
        };
        assert!(!release_with(&slot, &stranger, &cfg, &probe, &pool));
        assert!(owner_file.exists(), "unrelated run must not remove owner file");

        assert!(release_with(&slot, &r3, &cfg, &probe, &pool));
        assert!(!owner_file.exists(), "reply descendant must release ancestor-owned slot");
        let _ = fs::remove_dir_all(&base);
    }
}
