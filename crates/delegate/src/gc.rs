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

/// Atomically stage ONLY the worktree (not the run directory) into trash for `apply`
/// when the lightweight `.applied-cleaned` run record is kept. O(1) rename (<2ms).
pub fn stage_worktree_only_to_trash(run: &Path, meta: &Value) -> Res<Option<PathBuf>> {
    let wt_path_str = s(&meta["worktree"], "path").to_string();
    let source_str = s(&meta["worktree"], "source").to_string();
    if wt_path_str.is_empty() {
        return Ok(None);
    }
    let wt_path = Path::new(&wt_path_str);
    if !wt_path.is_absolute() || wt_path == Path::new(&source_str) {
        return Ok(None);
    }
    if crate::pool::is_slot(wt_path) {
        crate::pool::release(wt_path, run);
        return Ok(None);
    }
    let id = unique_trash_id(run);
    let jobs = jobs_dir();
    fs::create_dir_all(&jobs).map_err(|e| e.to_string())?;
    let mut trash_wt = String::new();
    let mut git_owner = String::new();
    if wt_path.exists() {
        let git_dirs = worktree_git_dirs_fast(wt_path);
        git_owner = git_dirs
            .as_ref()
            .map(|(_, common)| common.to_string_lossy().into_owned())
            .unwrap_or_else(|| source_str.clone());
        let dst = trash_root().join("wt").join(&id);
        let moved = rename_to_trash(wt_path, &dst, &id)?;
        trash_wt = moved.to_string_lossy().into_owned();
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
    let job_path = jobs.join(format!("{id}.json"));
    let job = json!({
        "id": id,
        "runId": s(meta, "run"),
        "trashRunDir": "",
        "worktreePath": wt_path_str,
        "trashWorktreeDir": trash_wt,
        "gitOwner": git_owner,
        "sourceRepo": source_str,
        "pooled": false,
        "remote": meta["remote"],
        "meta": meta,
        "createdAt": iso(),
    });
    write_json(&job_path, &job)?;
    Ok(Some(job_path))
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

fn drain_trash_jobs(jobs: &Path, max_passes: usize) -> bool {
    let mut drained_any = false;
    for _pass in 0..max_passes {
        let mut pending: Vec<PathBuf> = fs::read_dir(jobs)
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
            drained_any = true;
        }
    }
    drained_any
}

/// `delegate _gc-worker` — single-instance short-lived background GC worker.
/// Acquires `<state>/.gc-worker.lock` non-blocking (`LOCK_NB`); exits immediately if another
/// worker already holds it. Drains trash jobs, cleans orphan containers, reclaims applied/dropped
/// worktrees immediately, reclaims unmerged finished worktrees only on whole-disk watermark
/// pressure (LRU, archive before delete, reusing `disk` liveness signals), and runs deferred
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
    let mut drained_any = drain_trash_jobs(&jobs, 8);

    // Sweep orphan docker containers for already-deleted worktree directories.
    let _ = crate::cleanup::containers(&cache_dir().join("worktrees"), true);
    // Sweep orphan per-task agy temporary HOME directories whose processes have exited.
    let _ = crate::agy_accounts::cleanup_orphan_agy_homes();

    // Opportunistic maintenance moved out of read paths.
    crate::work::check_stale_and_expired();
    crate::refs::backfill_all();
    crate::history::ensure_backfilled();

    // Immediately archive and reclaim worktrees of applied or dropped runs (`.truncated` does not
    // block cleanup because `archive::preserve` retains full `result.md` / `result-original.md`).
    auto_reclaim_applied_and_dropped(&state);
    if drain_trash_jobs(&jobs, 4) {
        drained_any = true;
    }

    // Reclaim unmerged finished runs ONLY when whole-disk watermark triggers (P9: no fixed TTL or
    // static GiB cap), evicting least-recently-touched non-busy worktrees first after archiving.
    auto_reclaim_unmerged_on_watermark(&state);
    if drain_trash_jobs(&jobs, 4) {
        drained_any = true;
    }

    // P2-1: Cold-start / version-mismatch / corrupt index rebuild in the background worker.
    if crate::index::needs_rebuild(&state) {
        crate::index::rebuild_all();
    }
    // Keep `<state>/eta-model.json` fresh in the background so read-only `snapshot --json`
    // never has to full-scan `events.jsonl`.
    let _ = crate::speed::load_or_refresh_eta_model_in(&state, epoch() as i64, true);
    // Recompute quota rate drift cache & emit `quota_rate_shift` (at most 1/account/day)
    // in the background so read paths stay strictly lock-free and zero-subprocess.
    let _ = crate::quota_rate::refresh_and_emit_in(&state, epoch() as i64);

    // P1-2: Keep `<cache>/delegate/archive/.usage.json` warm and accurate.
    if !crate::archive::usage_cache_path().is_file() || drained_any {
        let _ = crate::archive::refresh_usage_cache();
    }

    Ok(0)
}

fn is_run_dropped(run: &Path, meta: &Value, work_items: &[crate::work::WorkItem]) -> bool {
    let run_id = crate::notes::run_id(run, meta);
    let dir_name = run.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let matches = |w: &crate::work::WorkItem| {
        w.matches_run(&run_id) || (!dir_name.is_empty() && w.matches_run(dir_name))
    };
    let in_dropped = work_items
        .iter()
        .any(|w| w.state == "done" && w.closed_kind == "dropped" && matches(w));
    if !in_dropped {
        return false;
    }
    let in_non_dropped = work_items
        .iter()
        .any(|w| !(w.state == "done" && w.closed_kind == "dropped") && matches(w));
    !in_non_dropped
}

fn run_last_touched_ns(run: &Path) -> u128 {
    use std::time::UNIX_EPOCH;
    for name in ["exit_code", "summary.json", "meta.json"] {
        if let Ok(m) = fs::metadata(run.join(name)) {
            if let Ok(t) = m.modified() {
                if let Ok(d) = t.duration_since(UNIX_EPOCH) {
                    return d.as_nanos();
                }
            }
        }
    }
    fs::metadata(run)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn auto_reclaim_applied_and_dropped(state: &Path) {
    let work_items = crate::work::load_readonly_in(state);
    let disk_cfg = crate::disk::load_config().unwrap_or_default();
    let lane_dir = state.join("lane");
    let proc_root = Path::new("/proc");
    let repos = crate::index::discover_snapshot_repos(state, &[], None);

    for (_, runs_dir) in repos {
        let all = crate::runs::all_runs_in(&runs_dir);
        let metas: Vec<Value> = all.iter().map(|r| json(r.join("meta.json"))).collect();
        let wt_paths: Vec<PathBuf> = metas
            .iter()
            .map(|m| {
                let p = s(&m["worktree"], "path");
                if p.is_empty() {
                    PathBuf::new()
                } else {
                    PathBuf::from(p)
                }
            })
            .collect();
        let busy = crate::disk::probe_worktrees_busy(&wt_paths, &disk_cfg, proc_root, &lane_dir);

        // Pass 1: Detect manually-landed changes for finished write runs first so `.applied`
        // propagates to inactive reply-chain ancestors before `shared` / cleanup decisions.
        for (idx, run) in all.iter().enumerate() {
            if !run.is_dir() || busy.contains_key(&idx) {
                continue;
            }
            if run.join(".applied").exists()
                || run.join(".generate-pending").exists()
                || crate::pending_reply::status(run).is_some()
            {
                continue;
            }
            if crate::runs::active(&crate::runs::state(run))
                || crate::runs::agent_alive(run)
                || crate::runs::apply_in_flight(run)
            {
                continue;
            }
            let meta = &metas[idx];
            if is_run_dropped(run, meta, &work_items) {
                continue;
            }
            if s(meta, "mode") == "write" && run.join("exit_code").is_file() {
                let _ = crate::worktree::detect_applied(run, meta);
            }
        }

        // Pass 2: Reclaim applied and dropped runs. When multiple runs share a worktree,
        // only remove the worktree once no other live run directory still points to it.
        for (idx, run) in all.iter().enumerate() {
            if !run.is_dir() {
                continue;
            }
            if run.join(".generate-pending").exists()
                || crate::pending_reply::status(run).is_some()
            {
                continue;
            }
            if crate::runs::active(&crate::runs::state(run))
                || crate::runs::agent_alive(run)
                || crate::runs::apply_in_flight(run)
            {
                continue;
            }
            if busy.contains_key(&idx) {
                continue;
            }

            let meta = &metas[idx];
            let run_id = crate::notes::run_id(run, meta);
            let dropped = is_run_dropped(run, meta, &work_items);
            let is_applied = run.join(".applied").exists();
            if !is_applied && !dropped {
                continue;
            }

            let has_active_child = all.iter().enumerate().any(|(oi, other)| {
                oi != idx
                    && other.is_dir()
                    && s(&metas[oi], "parent") == run_id
                    && (crate::runs::active(&crate::runs::state(other))
                        || crate::runs::apply_in_flight(other)
                        || (!other.join(".applied").exists()
                            && !is_run_dropped(other, &metas[oi], &work_items)))
            });
            if has_active_child {
                continue;
            }

            let wt_str = s(&meta["worktree"], "path");
            let source_str = s(&meta["worktree"], "source");
            let shared_blocking = !wt_str.is_empty()
                && all.iter().enumerate().any(|(oi, other)| {
                    oi != idx
                        && other.is_dir()
                        && s(&metas[oi]["worktree"], "path") == wt_str
                        && (crate::runs::active(&crate::runs::state(other))
                            || crate::runs::apply_in_flight(other)
                            || (!other.join(".applied").exists()
                                && !is_run_dropped(other, &metas[oi], &work_items)))
                });
            let any_other_still_present = !wt_str.is_empty()
                && all.iter().enumerate().any(|(oi, other)| {
                    oi != idx
                        && other.is_dir()
                        && s(&metas[oi]["worktree"], "path") == wt_str
                });
            let can_remove_wt = !shared_blocking && !any_other_still_present;

            if dropped && !is_applied {
                let _ = crate::runs::auto_clean_run(run, can_remove_wt, "dropped");
                continue;
            }

            if !crate::worktree::clean_on_apply(run) {
                continue;
            }
            let sum = json(run.join("summary.json"));
            if sum["conflictMarkers"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
            {
                continue;
            }

            let applied_doc = json(run.join(".applied"));
            let is_superseded_detected = !s(&applied_doc, "supersededBy").is_empty();
            if crate::worktree::is_applied_committed_in_head(run, meta)
                || (is_superseded_detected && !shared_blocking)
            {
                let _ = crate::runs::auto_clean_applied_run(run, can_remove_wt);
            } else if !run.join(".applied-cleaned").exists() {
                let wt_p = Path::new(wt_str);
                let has_reclaimable_wt = !wt_str.is_empty()
                    && wt_p.is_absolute()
                    && wt_p != Path::new(source_str)
                    && (wt_p.exists() || crate::pool::is_slot(wt_p));
                if has_reclaimable_wt && !shared_blocking {
                    let _ = crate::runs::remove_after_apply(run);
                    crate::index::upsert_run(run);
                    crate::board::refresh_for_run(run);
                }
            }
        }
    }
}

fn auto_reclaim_unmerged_on_watermark(state: &Path) {
    use std::collections::BTreeMap;

    let disk_cfg = crate::disk::load_config().unwrap_or_default();
    let wt_root = cache_dir().join("worktrees");
    let primary_probe = disk_cfg.primary_probe_path(&wt_root);
    let primary_need = crate::disk::probe_disk_space(crate::disk::DiskRole::Primary, &primary_probe)
        .map(|sp| disk_cfg.primary_bytes_to_free(&sp))
        .unwrap_or(0);
    let overflow_need = disk_cfg
        .overflow_root
        .as_deref()
        .and_then(|r| crate::disk::probe_disk_space(crate::disk::DiskRole::Overflow, r))
        .map(|sp| disk_cfg.overflow_bytes_to_free(&sp))
        .unwrap_or(0);

    if primary_need == 0 && overflow_need == 0 {
        return;
    }

    struct WtGroup {
        wt: PathBuf,
        disk: crate::disk::DiskRole,
        runs: Vec<PathBuf>,
        max_touched_ns: u128,
        blocked: bool,
    }

    let repos = crate::index::discover_snapshot_repos(state, &[], None);
    let mut groups: BTreeMap<PathBuf, WtGroup> = BTreeMap::new();

    for (_, runs_dir) in repos {
        let all = crate::runs::all_runs_in(&runs_dir);
        let metas: Vec<Value> = all.iter().map(|r| json(r.join("meta.json"))).collect();

        for (idx, run) in all.iter().enumerate() {
            if !run.is_dir() {
                continue;
            }
            let meta = &metas[idx];
            let wt_str = s(&meta["worktree"], "path");
            let source_str = s(&meta["worktree"], "source");
            if wt_str.is_empty() {
                continue;
            }
            let wt = PathBuf::from(wt_str);
            if !wt.is_absolute() || wt == Path::new(source_str) || !wt.is_dir() {
                continue;
            }

            let wt_key = fs::canonicalize(&wt).unwrap_or_else(|_| wt.clone());
            let disk = if crate::disk::path_is_under_overflow(&wt, &disk_cfg) {
                crate::disk::DiskRole::Overflow
            } else {
                crate::disk::DiskRole::Primary
            };
            let entry = groups.entry(wt_key).or_insert_with(|| WtGroup {
                wt: wt.clone(),
                disk,
                runs: Vec::new(),
                max_touched_ns: 0,
                blocked: false,
            });

            let run_id = crate::notes::run_id(run, meta);
            let has_active_child = all.iter().enumerate().any(|(oi, other)| {
                oi != idx
                    && other.is_dir()
                    && s(&metas[oi], "parent") == run_id
                    && (crate::runs::active(&crate::runs::state(other))
                        || crate::runs::apply_in_flight(other))
            });
            let eligible = run.join("exit_code").is_file()
                && !run.join(".applied").exists()
                && !run.join(".applied-cleaned").exists()
                && !run.join(".generate-pending").exists()
                && crate::pending_reply::status(run).is_none()
                && !crate::runs::active(&crate::runs::state(run))
                && !crate::runs::agent_alive(run)
                && !crate::runs::apply_in_flight(run)
                && !has_active_child;

            if !eligible {
                // Any active, pending, or applied run sharing this worktree protects the whole group.
                entry.blocked = true;
            } else {
                entry.max_touched_ns = entry.max_touched_ns.max(run_last_touched_ns(run));
                entry.runs.push(run.clone());
            }
        }
    }

    let candidates: Vec<WtGroup> = groups
        .into_values()
        .filter(|g| {
            if g.blocked || g.runs.is_empty() {
                return false;
            }
            let need_for_disk = match g.disk {
                crate::disk::DiskRole::Primary => primary_need,
                crate::disk::DiskRole::Overflow => overflow_need,
            };
            need_for_disk > 0
        })
        .collect();

    if candidates.is_empty() {
        return;
    }

    let wt_list: Vec<PathBuf> = candidates.iter().map(|c| c.wt.clone()).collect();
    let lane_dir = state.join("lane");
    let busy = crate::disk::probe_worktrees_busy(&wt_list, &disk_cfg, Path::new("/proc"), &lane_dir);

    let mut evictable: Vec<WtGroup> = candidates
        .into_iter()
        .enumerate()
        .filter(|(idx, _)| !busy.contains_key(idx))
        .map(|(_, g)| g)
        .collect();

    // Evict least-recently-touched worktree group first (LRU by latest round in the chain).
    evictable.sort_by(|a, b| {
        a.max_touched_ns
            .cmp(&b.max_touched_ns)
            .then_with(|| a.wt.cmp(&b.wt))
    });

    let mut primary_freed = 0u64;
    let mut overflow_freed = 0u64;

    for group in evictable {
        match group.disk {
            crate::disk::DiskRole::Primary if primary_freed >= primary_need => continue,
            crate::disk::DiskRole::Overflow if overflow_freed >= overflow_need => continue,
            _ => {}
        }
        if !group.wt.is_dir() {
            continue;
        }
        let is_pool = crate::pool::is_slot(&group.wt);
        let before_bytes = crate::disk::worktree_disk_bytes(&group.wt);
        let live_runs: Vec<PathBuf> = group
            .runs
            .into_iter()
            .filter(|r| r.is_dir())
            .collect();
        if live_runs.is_empty() {
            continue;
        }

        // Archive and clean every run sharing this worktree; only remove/release the worktree
        // on the final run so earlier rounds never leave newer siblings dangling.
        let last_idx = live_runs.len() - 1;
        let mut all_ok = true;
        for (i, run) in live_runs.iter().enumerate() {
            let remove_wt = i == last_idx;
            if crate::runs::auto_clean_run(run, remove_wt, "disk_watermark").is_err() {
                all_ok = false;
                break;
            }
        }
        if !all_ok {
            continue;
        }

        // Measure actual bytes reclaimed: pool slots are released (not deleted), so only count
        // the real delta on disk rather than crediting the entire retained slot/target directory.
        let freed = if is_pool {
            let after_bytes = if group.wt.exists() {
                crate::disk::worktree_disk_bytes(&group.wt)
            } else {
                0
            };
            before_bytes.saturating_sub(after_bytes)
        } else {
            before_bytes.max(1)
        };

        match group.disk {
            crate::disk::DiskRole::Primary => {
                primary_freed = primary_freed.saturating_add(freed);
            }
            crate::disk::DiskRole::Overflow => {
                overflow_freed = overflow_freed.saturating_add(freed);
            }
        }
        if primary_freed >= primary_need && overflow_freed >= overflow_need {
            break;
        }
    }
}

/// Premerge fast-forward interface compatibility facade (implemented in `crate::premerge`).
pub mod premerge {
    use serde_json::Value;
    use std::path::{Path, PathBuf};

    #[allow(dead_code)]
    pub fn premerge_path(run: &Path) -> PathBuf {
        crate::premerge::premerge_path(run)
    }

    /// Fail-closed validation of a `premerge.json` artifact against the current source `HEAD`.
    /// Returns `Some(doc)` ONLY if `baseHead == current_head`, `acceptOk == true`,
    /// `conflicts` is empty, and `mergedTree` is non-empty.
    #[allow(dead_code)]
    pub fn validate_fast_forward(run: &Path, source: &Path) -> Option<Value> {
        crate::premerge::validate_fast_forward_basic(run, source)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::common::*;
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
