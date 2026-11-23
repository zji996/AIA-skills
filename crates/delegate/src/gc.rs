//! Asynchronous trash & garbage-collection worker (`delegate _gc-worker`) for <50ms `clean`,
//! plus V2 premerge fast-forward interface reservation (`premerge`).
//!
//! `clean` atomically renames the run directory and non-pooled worktree into same-filesystem
//! trash directories and writes a job file under `<cache>/trash/jobs/<id>.json`, then spawns a
//! short-lived detached `_gc-worker` (single-instance via `LOCK_NB` on `<state>/.gc-worker.lock`)
//! to run `cleanup::containers`, `remote::clean`, `git worktree prune`, `rm -rf`, and orphan
//! container / stale work maintenance in the background.

use crate::common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn trash_root() -> PathBuf {
    cache_dir().join("trash")
}

pub fn jobs_dir() -> PathBuf {
    trash_root().join("jobs")
}

fn unique_trash_id(run: &Path) -> String {
    let base = run
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("run");
    format!("{base}-{}-{:08x}", std::process::id(), (now_ns() as u32))
}

/// Resolve the worktree's git admin dir (`.git/worktrees/<name>`) and owning common git dir
/// directly from `<wt>/.git` without spawning git (<0.1ms).
fn worktree_git_dirs_fast(wt: &Path) -> Option<(PathBuf, PathBuf)> {
    let dot_git = wt.join(".git");
    if dot_git.is_file() {
        let text = fs::read_to_string(&dot_git).ok()?;
        let raw = text.lines().next()?.trim().strip_prefix("gitdir:")?.trim();
        let git_dir = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            wt.join(raw)
        };
        let git_dir = fs::canonicalize(&git_dir).unwrap_or(git_dir);
        let common = if let Ok(common_raw) = fs::read_to_string(git_dir.join("commondir")) {
            let trimmed = common_raw.trim();
            if !trimmed.is_empty() {
                let c = PathBuf::from(trimmed);
                let resolved = if c.is_absolute() { c } else { git_dir.join(c) };
                fs::canonicalize(&resolved).unwrap_or(resolved)
            } else {
                git_dir
                    .parent()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| git_dir.clone())
            }
        } else {
            git_dir
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| git_dir.clone())
        };
        return Some((git_dir, common));
    }
    None
}

/// Rename `src` into `preferred_dst`, falling back to a sibling `.trash` dir on cross-device rename.
fn rename_to_trash(src: &Path, preferred_dst: &Path, id: &str) -> Res<PathBuf> {
    if let Some(parent) = preferred_dst.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::rename(src, preferred_dst) {
        Ok(()) => Ok(preferred_dst.to_path_buf()),
        Err(_) => {
            // Cross-filesystem fallback (e.g. runs under repo disk, cache on tmpfs):
            // keep the atomic rename on the SAME filesystem under `<src_parent>/.trash/<id>`.
            let fallback_dir = src
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(".trash");
            fs::create_dir_all(&fallback_dir).map_err(|e| e.to_string())?;
            let fallback = fallback_dir.join(id);
            fs::rename(src, &fallback).map_err(|e| e.to_string())?;
            Ok(fallback)
        }
    }
}

/// Atomically stage a run directory and (if unshared & non-pooled) its worktree into trash,
/// recording a GC job JSON for `_gc-worker`. O(1) rename on the same filesystem (<2ms).
pub fn stage_clean_to_trash(
    run: &Path,
    meta: &Value,
    remove_worktree: bool,
) -> Res<PathBuf> {
    let id = unique_trash_id(run);
    let jobs = jobs_dir();
    fs::create_dir_all(&jobs).map_err(|e| e.to_string())?;

    let wt_path_str = s(&meta["worktree"], "path").to_string();
    let source_str = s(&meta["worktree"], "source").to_string();
    let mut trash_wt = String::new();
    let mut git_owner = String::new();
    let mut pooled = false;

    if remove_worktree && !wt_path_str.is_empty() {
        let wt_path = Path::new(&wt_path_str);
        if wt_path.is_absolute() && wt_path != Path::new(&source_str) {
            if crate::pool::is_slot(wt_path) {
                pooled = true;
                crate::pool::release(wt_path, run);
            } else if wt_path.exists() {
                let git_dirs = worktree_git_dirs_fast(wt_path);
                git_owner = git_dirs
                    .as_ref()
                    .map(|(_, common)| common.to_string_lossy().into_owned())
                    .unwrap_or_else(|| source_str.clone());
                let dst = trash_root().join("wt").join(&id);
                let moved = rename_to_trash(wt_path, &dst, &id)?;
                trash_wt = moved.to_string_lossy().into_owned();
                // Unregister the worktree admin entry (`.git/worktrees/<name>`, ~4 tiny files)
                // directly without spawning git (<0.2ms) so `git worktree list` is immediately clean.
                if let Some((admin_dir, _)) = git_dirs {
                    if admin_dir
                        .parent()
                        .and_then(|p| p.file_name())
                        .is_some_and(|n| n == "worktrees")
                    {
                        let _ = fs::remove_dir_all(&admin_dir);
                    }
                }
            }
        }
    }

    let trash_run = if run.exists() {
        let dst = trash_root().join("runs").join(&id);
        rename_to_trash(run, &dst, &id)?
            .to_string_lossy()
            .into_owned()
    } else {
        String::new()
    };

    let job_path = jobs.join(format!("{id}.json"));
    let job = json!({
        "id": id,
        "runId": s(meta, "run"),
        "trashRunDir": trash_run,
        "worktreePath": if remove_worktree { wt_path_str } else { String::new() },
        "trashWorktreeDir": trash_wt,
        "gitOwner": git_owner,
        "sourceRepo": source_str,
        "pooled": pooled,
        "remote": meta["remote"],
        "meta": meta,
        "createdAt": iso(),
    });
    write_json(&job_path, &job)?;
    Ok(job_path)
}

/// Execute a single GC job (used both by `_gc-worker` and by `clean --sync`).
pub fn execute_job(job_path: &Path) -> crate::cleanup::Containers {
    execute_job_inner(job_path, true)
}

pub fn execute_job_inner(job_path: &Path, run_containers: bool) -> crate::cleanup::Containers {
    let job = json(job_path);
    if !job.is_object() {
        let _ = fs::remove_file(job_path);
        return crate::cleanup::Containers::default();
    }

    let wt_path_str = s(&job, "worktreePath");
    let trash_wt_str = s(&job, "trashWorktreeDir");
    let trash_run_str = s(&job, "trashRunDir");
    let git_owner_str = s(&job, "gitOwner");
    let source_repo_str = s(&job, "sourceRepo");
    let pooled = b(&job, "pooled");

    let containers = if run_containers && !wt_path_str.is_empty() {
        let wt_p = Path::new(wt_path_str);
        if wt_p.is_absolute() && wt_p != Path::new(source_repo_str) {
            crate::cleanup::containers(wt_p, false)
        } else {
            crate::cleanup::Containers::default()
        }
    } else {
        crate::cleanup::Containers::default()
    };

    if job["meta"].is_object() {
        crate::remote::clean(&job["meta"]);
    }

    if !trash_wt_str.is_empty() {
        let _ = fs::remove_dir_all(trash_wt_str);
    }

    if !pooled && !wt_path_str.is_empty() {
        let owner = if !git_owner_str.is_empty() {
            Path::new(git_owner_str)
        } else {
            Path::new(source_repo_str)
        };
        if owner.exists() {
            let _ = git(owner, &["worktree", "prune"]);
        }
    }

    if !trash_run_str.is_empty() {
        let _ = fs::remove_dir_all(trash_run_str);
    }

    let _ = fs::remove_file(job_path);
    containers
}

/// Spawn a detached background `delegate _gc-worker` (non-blocking, returns immediately).
pub fn spawn_worker() {
    let mut c = Command::new(script());
    c.arg("_gc-worker")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("DELEGATE_LANE_HELD");
    group(&mut c);
    let _ = c.spawn();
}

/// `delegate _gc-worker` — single-instance short-lived background GC worker.
/// Acquires `<state>/.gc-worker.lock` non-blocking (`LOCK_NB`); exits immediately if another
/// worker already holds it. Drains trash jobs, cleans orphan containers, and runs deferred
/// maintenance (`check_stale_and_expired`, legacy backfill).
pub fn worker_command(_args: &[String]) -> Res<i32> {
    let state = state_dir();
    let _ = fs::create_dir_all(&state);
    let Ok(_guard) = crate::locks::try_acquire(
        crate::locks::LockKind::Worker,
        &state.join(".gc-worker.lock"),
        true,
    ) else {
        // Another `_gc-worker` instance is already draining the queue.
        return Ok(0);
    };

    let jobs = jobs_dir();
    for _pass in 0..8 {
        let mut pending: Vec<PathBuf> = fs::read_dir(&jobs)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
            .collect();
        if pending.is_empty() {
            break;
        }
        pending.sort();
        for job in pending {
            let _ = execute_job(&job);
        }
    }

    // Sweep orphan docker containers for already-deleted worktree directories.
    let _ = crate::cleanup::containers(&cache_dir().join("worktrees"), true);

    // Opportunistic maintenance moved out of read paths.
    crate::work::check_stale_and_expired();
    crate::refs::backfill_all();
    crate::history::ensure_backfilled();

    Ok(0)
}

/// V2 interface reservation for background premerge -> fast-forward `apply` (Review §3 & §5).
/// V1 reserves the fail-closed contract & schema without running background `_merge-worker` yet.
#[allow(dead_code)]
pub mod premerge {
    use crate::common::*;
    use serde_json::Value;
    use std::path::{Path, PathBuf};

    pub fn premerge_path(run: &Path) -> PathBuf {
        run.join("premerge.json")
    }

    /// Fail-closed validation of a `premerge.json` artifact against the current source `HEAD`.
    /// Returns `Some(merged_tree)` ONLY if `baseHead == current_head`, `acceptOk == true`,
    /// `conflicts` is empty, and `mergedTree` is non-empty.
    pub fn validate_fast_forward(run: &Path, source: &Path) -> Option<Value> {
        let doc = json(premerge_path(run));
        if !doc.is_object() {
            return None;
        }
        let base_head = s(&doc, "baseHead");
        let merged_tree = s(&doc, "mergedTree");
        if base_head.is_empty() || merged_tree.is_empty() || !b(&doc, "acceptOk") {
            return None;
        }
        if doc["conflicts"]
            .as_array()
            .is_some_and(|c| !c.is_empty())
        {
            return None;
        }
        let current_head = read_git_head(source)?;
        if current_head != base_head {
            return None;
        }
        Some(doc)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;
        use std::fs;

        #[test]
        fn stale_premerge_base_head_is_rejected_fail_closed() {
            let dir = std::env::temp_dir().join(format!(
                "delegate-premerge-v2-stub-{}-{}",
                std::process::id(),
                now_ns()
            ));
            let repo = dir.join("repo");
            let run = dir.join("run");
            fs::create_dir_all(repo.join(".git/refs/heads")).unwrap();
            fs::create_dir_all(&run).unwrap();
            let head_a = "1111111111111111111111111111111111111111";
            let head_b = "2222222222222222222222222222222222222222";
            fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
            fs::write(repo.join(".git/refs/heads/main"), format!("{head_b}\n")).unwrap();
            write_json(
                premerge_path(&run),
                &json!({
                    "baseHead": head_a,
                    "mergedTree": "3333333333333333333333333333333333333333",
                    "acceptOk": true,
                    "conflicts": []
                }),
            )
            .unwrap();
            assert!(validate_fast_forward(&run, &repo).is_none());

            // When HEAD matches baseHead, validation passes.
            fs::write(repo.join(".git/refs/heads/main"), format!("{head_a}\n")).unwrap();
            assert!(validate_fast_forward(&run, &repo).is_some());
            let _ = fs::remove_dir_all(&dir);
        }
    }
}
