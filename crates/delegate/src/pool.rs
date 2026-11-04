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
            attach_target(slot)?;
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

/// Put a slot's `target/` where `DELEGATE_POOL_TARGET_ROOT` says, e.g. a disk kept for
/// builds: `target` becomes a link to `<root>/<slot>/target`. The link survives `git clean`
/// (`/target` is excluded) and a recreated slot finds its build cache again, because the
/// slot path, which Cargo keys path packages by, does not change. Unset, a link left by an
/// earlier setting is dropped and the next build starts a local `target/` cold.
pub fn attach_target(slot: &Path) -> Result<(), String> {
    attach_target_to(slot, &setting("POOL_TARGET_ROOT", ""))
}

fn attach_target_to(slot: &Path, root: &str) -> Result<(), String> {
    let link = slot.join("target");
    if root.is_empty() {
        if link.is_symlink() {
            fs::remove_file(&link).map_err(|e| e.to_string())?;
        }
        return Ok(());
    }
    let name = slot.file_name().unwrap_or_default();
    let dest = Path::new(root).join(name).join("target");
    if link.is_symlink() && fs::read_link(&link).is_ok_and(|to| to == dest) {
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

/// A slot's `target/` only grows: old commits, feature sets and package-scoped builds each
/// leave their own artifacts beside the current ones. Past the limit
/// (`DELEGATE_POOL_TARGET_GIB`, default 60) the least recently used build units are
/// evicted until it is back under three quarters of it, so the variants tasks keep using
/// stay warm instead of the whole cache going cold at once.
fn trim_target(slot: &Path) {
    let limit_gib: u64 = setting("POOL_TARGET_GIB", "60").parse().unwrap_or(60);
    let limit = limit_gib << 30;
    let pruned = prune_sessions(&slot.join("target"));
    if pruned >= 1 << 30 {
        eprintln!(
            "delegate: pool slot {} dropped {} GiB of superseded incremental sessions",
            slot.display(),
            pruned >> 30
        );
    }
    let (used, freed) = evict_lru(&slot.join("target"), limit, limit / 4 * 3);
    if freed > 0 {
        eprintln!(
            "delegate: pool slot {} target was {} GiB (limit {limit_gib}); evicted {} GiB of least recently used build units",
            slot.display(),
            used >> 30,
            freed >> 30
        );
    }
}

/// Drop every incremental session but the newest of each crate. rustc keeps the previous
/// session beside the current one until that crate compiles again, and the dependency graph
/// and query cache in it are rewritten, not hard-linked, so each edit-and-rebuild left about
/// a third of the incremental state behind as garbage (7 of 28 GiB in one measured slot).
/// rustc only ever reads the newest finalized session; unfinished `-working` ones are left
/// alone. Runs while the slot is being claimed, so no compiler holds a session.
fn prune_sessions(target: &Path) -> u64 {
    let mut freed = 0;
    let mut stack = vec![(target.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if entry.file_name() == "incremental" {
                for krate in fs::read_dir(entry.path()).into_iter().flatten().flatten() {
                    freed += prune_crate_sessions(&krate.path());
                }
            } else if depth < 2 {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
    freed
}

fn prune_crate_sessions(krate: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    // `s-<timestamp>-<random>-<svh>`; the timestamp is base-36 microseconds, so the longer
    // or, at equal length, larger one is newer.
    let mut finished: Vec<(String, String)> = fs::read_dir(krate)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let mut parts = name.splitn(4, '-');
            let (Some("s"), Some(stamp), Some(random), Some(tail)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return None;
            };
            (tail != "working").then(|| (stamp.to_string(), format!("s-{stamp}-{random}")))
        })
        .collect();
    finished.sort_by(|a, b| (a.0.len(), &a.0).cmp(&(b.0.len(), &b.0)));
    finished.pop();
    let mut freed = 0;
    for (_, prefix) in finished {
        for entry in fs::read_dir(krate).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != format!("{prefix}.lock") && !name.starts_with(&format!("{prefix}-")) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                for file in fs::read_dir(&path).into_iter().flatten().flatten() {
                    // Files shared with the newest session by hard link free nothing.
                    if let Ok(meta) = file.metadata() {
                        if meta.nlink() == 1 {
                            freed += meta.blocks() * 512;
                        }
                    }
                }
                let _ = fs::remove_dir_all(&path);
            } else {
                let _ = fs::remove_file(&path);
            }
        }
    }
    freed
}

/// One evictable piece of a Cargo target: every file of a compiled unit (`deps/`,
/// `.fingerprint/`, `build/`, `examples/` entries sharing `<name>-<hash>`) or one crate's
/// incremental directory. Cargo rebuilds a unit whose output or fingerprint is missing,
/// so evicting any of them costs a rebuild, never a wrong build.
#[derive(Default)]
struct Unit {
    paths: Vec<PathBuf>,
    bytes: u64,
    /// Latest modification, or file access; `relatime` refreshes access times about
    /// daily, which is fine-grained enough to tell this week's variants from last week's.
    used: i64,
}

fn unit_key(dir: &str, name: &str) -> String {
    if dir == "incremental" {
        return name.to_string();
    }
    let (stem, ext) = name.split_once('.').unwrap_or((name, ""));
    let library =
        ["rlib", "rmeta", "so", "a", "dylib"].contains(&ext.rsplit('.').next().unwrap_or(""));
    match stem.strip_prefix("lib") {
        Some(rest) if library => rest.to_string(),
        _ => stem.to_string(),
    }
}

fn tree_usage(path: &Path, unit: &mut Unit) {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    unit.bytes += meta.blocks() * 512;
    unit.used = unit.used.max(meta.mtime());
    // A file's access time records Cargo reading it (linking, checking freshness); a
    // directory's would record this very scan listing it.
    if !meta.is_dir() {
        unit.used = unit.used.max(meta.atime());
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path).into_iter().flatten().flatten() {
            tree_usage(&entry.path(), unit);
        }
    }
}

/// Group a target's build units, then delete the oldest until `used <= low`; returns the
/// size before and the bytes freed. Nothing is touched while the target is under `limit`.
fn evict_lru(target: &Path, limit: u64, low: u64) -> (u64, u64) {
    let mut units: std::collections::BTreeMap<(PathBuf, String), Unit> = Default::default();
    let mut other = Unit::default();
    let mut stack = vec![(target.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir
                && ["deps", ".fingerprint", "build", "examples", "incremental"]
                    .contains(&name.as_str())
            {
                for item in fs::read_dir(&path).into_iter().flatten().flatten() {
                    let item_name = item.file_name().to_string_lossy().into_owned();
                    let unit = units
                        .entry((dir.clone(), unit_key(&name, &item_name)))
                        .or_default();
                    unit.paths.push(item.path());
                    tree_usage(&item.path(), unit);
                }
            } else if is_dir && depth < 2 {
                // `target/<profile>/` or `target/<triple>/<profile>/`.
                stack.push((path, depth + 1));
            } else {
                tree_usage(&path, &mut other);
            }
        }
    }
    let used = other.bytes + units.values().map(|u| u.bytes).sum::<u64>();
    if used <= limit {
        return (used, 0);
    }
    let mut order: Vec<Unit> = units.into_values().collect();
    order.sort_by_key(|u| u.used);
    let mut left = used;
    for unit in order {
        if left <= low {
            break;
        }
        for path in &unit.paths {
            let _ = if path.is_dir() && !path.is_symlink() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
        }
        left = left.saturating_sub(unit.bytes);
    }
    (used, used - left)
}

#[cfg(test)]
mod tests {
    use super::{attach_target_to, evict_lru, prune_sessions, reset};
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

    /// Over the limit, whole units go oldest first (a unit's rlib, fingerprint and
    /// incremental state together) and recently used units stay.
    #[test]
    fn an_oversized_target_evicts_least_recently_used_units() {
        let target = std::env::temp_dir().join(format!("delegate-lru-{}", std::process::id()));
        let _ = fs::remove_dir_all(&target);
        let debug = target.join("debug");
        let old = SystemTime::now() - Duration::from_secs(7 * 86400);
        let put = |rel: &str, when: Option<SystemTime>| {
            let path = debug.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, vec![1u8; 100 << 10]).unwrap();
            if let Some(when) = when {
                let times = fs::FileTimes::new().set_accessed(when).set_modified(when);
                fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_times(times)
                    .unwrap();
                // A unit's directories age with it, as they do when nothing rebuilds it.
                for dir in path
                    .ancestors()
                    .skip(1)
                    .take_while(|d| d.starts_with(&debug))
                {
                    let _ = fs::File::open(dir).and_then(|d| d.set_times(times));
                }
            }
        };
        put("deps/libold-1111.rlib", Some(old));
        put(".fingerprint/old-1111/lib-old", Some(old));
        put("incremental/old-x/s-1/query-cache.bin", Some(old));
        put("deps/libnew-2222.rlib", None);
        put("deps/new-2222.d", None);

        assert_eq!(
            evict_lru(&target, u64::MAX, 0).1,
            0,
            "under the limit nothing goes"
        );
        let (_, freed) = evict_lru(&target, 0, 250 << 10);
        assert!(freed >= 300 << 10);
        for gone in [
            "deps/libold-1111.rlib",
            ".fingerprint/old-1111",
            "incremental/old-x",
        ] {
            assert!(!debug.join(gone).exists(), "{gone} should be evicted");
        }
        assert!(debug.join("deps/libnew-2222.rlib").exists());
        assert!(debug.join("deps/new-2222.d").exists());
        let _ = fs::remove_dir_all(&target);
    }

    /// With a target root the slot's `target` links to `<root>/<slot>/target`, replacing a
    /// local directory; without one the link is dropped.
    #[test]
    fn a_target_root_links_the_slot_target_and_unset_drops_the_link() {
        let base = std::env::temp_dir().join(format!("delegate-troot-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let slot = base.join("repo-1");
        fs::create_dir_all(slot.join("target/debug")).unwrap();
        let root = base.join("optane");

        attach_target_to(&slot, &root.to_string_lossy()).unwrap();
        attach_target_to(&slot, &root.to_string_lossy()).unwrap();
        assert_eq!(
            fs::read_link(slot.join("target")).unwrap(),
            root.join("repo-1/target")
        );
        assert!(root.join("repo-1/target").is_dir());

        attach_target_to(&slot, "").unwrap();
        assert!(!slot.join("target").exists() && !slot.join("target").is_symlink());
        let _ = fs::remove_dir_all(&base);
    }

    /// Only each crate's newest finished session survives; an unfinished one is kept.
    #[test]
    fn older_incremental_sessions_are_pruned() {
        let target = std::env::temp_dir().join(format!("delegate-sess-{}", std::process::id()));
        let _ = fs::remove_dir_all(&target);
        let krate = target.join("debug/incremental/foo-abc");
        for dir in ["s-hnu2a-1x-svh", "s-hnu2b-2y-svh", "s-hnu2c-3z-working"] {
            fs::create_dir_all(krate.join(dir)).unwrap();
            fs::write(krate.join(dir).join("query-cache.bin"), vec![0u8; 8192]).unwrap();
        }
        for lock in ["s-hnu2a-1x.lock", "s-hnu2b-2y.lock", "s-hnu2c-3z.lock"] {
            fs::write(krate.join(lock), "").unwrap();
        }

        assert!(prune_sessions(&target) >= 8192);
        let mut left: Vec<String> = fs::read_dir(&krate)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "s-hnu2b-2y-svh",
                "s-hnu2b-2y.lock",
                "s-hnu2c-3z-working",
                "s-hnu2c-3z.lock"
            ]
        );
        let _ = fs::remove_dir_all(&target);
    }
}
