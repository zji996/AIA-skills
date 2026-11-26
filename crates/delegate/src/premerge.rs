//! Event-driven background premerge worker (`delegate _merge-worker`) and fail-closed
//! fast-forward `apply` (delegate 6.35.0).
//!
//! Architecture:
//! - One short-lived `_merge-worker` per repository, single-instance via `LOCK_NB` on
//!   `<state>/premerge/<repo-key>.lock` with a lock-free wake marker (`.wake`) so concurrent
//!   deliveries never lose a wakeup.
//! - Persistent integration worktree under `<cache>/premerge/<repo-key>/worktree` (outside the
//!   source repo), reusing `target/` and `node_modules/` across resets (`git reset --hard <HEAD>`).
//! - Worker NEVER writes to the source working tree; all source working tree writes happen
//!   exclusively inside `apply.lock`.
//! - Fast-forward `apply` validates `source HEAD == baseHead` and zero dirty overlap on
//!   `changedPaths`, writes `mergedTree`'s paths directly as uncommitted changes (leaving the git
//!   index untouched), and skips re-running verification. Any unmet condition falls back cleanly
//!   to 3-way merge with a clear one-line reason.

use crate::accept;
use crate::board;
use crate::changes;
use crate::common::*;
use crate::config;
use crate::events;
use crate::index;
use crate::lane;
use crate::launch;
use crate::locks;
use crate::refs;
use crate::review;
use crate::runs;
use crate::worktree;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

fn canonical_repo(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn repo_key(source: &Path) -> String {
    let canon = canonical_repo(source);
    let digest = sha1_smol::Sha1::from(canon.as_os_str().as_encoded_bytes())
        .digest()
        .to_string();
    let name = canon
        .file_name()
        .and_then(|n| n.to_str())
        .map(slug)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "repo".into());
    format!("{name}-{}", &digest[..12])
}

pub fn state_premerge_dir() -> PathBuf {
    state_dir().join("premerge")
}

pub fn worker_lock_path(source: &Path) -> PathBuf {
    state_premerge_dir().join(format!("{}.lock", repo_key(source)))
}

pub fn worker_wake_path(source: &Path) -> PathBuf {
    state_premerge_dir().join(format!("{}.wake", repo_key(source)))
}

pub fn worker_pid_path(source: &Path) -> PathBuf {
    state_premerge_dir().join(format!("{}.pid", repo_key(source)))
}

pub fn integration_worktree_path(source: &Path) -> PathBuf {
    cache_dir()
        .join("premerge")
        .join(repo_key(source))
        .join("worktree")
}

pub fn integration_setup_marker(source: &Path) -> PathBuf {
    cache_dir()
        .join("premerge")
        .join(repo_key(source))
        .join(".setup-done")
}

pub fn premerge_path(run: &Path) -> PathBuf {
    run.join("premerge.json")
}

pub fn premerge_log_path(run: &Path) -> PathBuf {
    run.join("premerge.log")
}

pub fn disabled() -> bool {
    matches!(
        std::env::var("DELEGATE_PREMERGE").ok().as_deref(),
        Some("0" | "false" | "off")
    )
}

/// Spawn a detached background `delegate _merge-worker <source>` (non-blocking, zero locks).
pub fn spawn_worker(source: &Path) {
    if disabled() || !source.is_dir() {
        return;
    }
    spawn_worker_force(source);
}

/// Spawn `_merge-worker` even when `DELEGATE_PREMERGE=0` (used for mainline-blocked auto-rerun;
/// inside the worker, normal premerge still respects `disabled()`).
pub fn spawn_worker_force(source: &Path) {
    if !source.is_dir() {
        return;
    }
    let canon = canonical_repo(source);
    let mut c = Command::new(script());
    c.arg("_merge-worker")
        .arg(&canon)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("DELEGATE_LANE_HELD");
    group(&mut c);
    let _ = c.spawn();
}

/// Cancel an in-flight `_merge-worker` that is currently premerging THIS same run so
/// `delegate apply` never double-runs `regenerate` / acceptance alongside `_merge-worker`.
/// Returns `true` if a live same-run worker was signaled.
pub fn cancel_for_run(run: &Path, source: &Path) -> bool {
    let pm_path = premerge_path(run);
    let pm = json(&pm_path);
    if s(&pm, "status") != "running" {
        return false;
    }
    let wpid = pm["workerPid"].as_i64().unwrap_or(0) as i32;
    if wpid <= 0 || wpid == std::process::id() as i32 {
        return false;
    }
    let recorded_pid = read(worker_pid_path(source))
        .trim()
        .parse::<i32>()
        .unwrap_or(0);
    if recorded_pid != wpid || !pid_alive(wpid) {
        return false;
    }
    unsafe {
        libc::kill(wpid, libc::SIGTERM);
    }
    let deadline = Instant::now() + std::time::Duration::from_millis(250);
    while Instant::now() < deadline && pid_alive(wpid) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let cur = json(&pm_path);
    if s(&cur, "status") == "running" && cur["workerPid"].as_i64() == Some(wpid as i64) {
        let _ = fs::remove_file(&pm_path);
        index::upsert_run(run);
    }
    true
}

fn should_yield_to_apply(run: &Path, source_canon: &Path) -> bool {
    lane::stopped()
        || !run.join("meta.json").is_file()
        || run.join(".applied").exists()
        || crate::runs::apply_in_flight(run)
        || crate::source_watch::apply_lock_busy(source_canon)
}

fn clear_running_marker_if_ours(run: &Path) {
    let p = premerge_path(run);
    let doc = json(&p);
    if s(&doc, "status") == "running"
        && doc["workerPid"].as_u64() == Some(std::process::id() as u64)
    {
        if run.join(".applied").exists() || !run.join("meta.json").is_file() {
            let _ = fs::remove_file(&p);
        } else {
            let cancelled = json!({
                "status": "cancelled",
                "cancelledBy": "apply",
                "baseHead": doc["baseHead"],
                "acceptOk": false,
                "conflicts": [],
                "updatedAt": iso(),
            });
            let _ = write_json(&p, &cancelled);
        }
        index::upsert_run(run);
    }
}

pub fn spawn_worker_for_meta(meta: &Value) {
    let src = s(&meta["worktree"], "source");
    if !src.is_empty() {
        let p = Path::new(src);
        if p.is_dir() {
            spawn_worker(p);
        }
    }
}

fn load_review_doc(run: &Path, sum: &Value) -> Value {
    let f = json(run.join("review.json"));
    if f.is_object() {
        f
    } else if sum["review"].is_object() {
        sum["review"].clone()
    } else {
        Value::Null
    }
}

/// Fast zero-git eligibility check (safe for `finish_run`, `status`, and `sign` triggers).
/// Matches: write worktree run in `delivered` state, original accept passed, review PASS or off
/// (or NEEDS_CONTROLLER only from controllerReviewPaths with a valid sign.json), no violations.
pub fn is_eligible_fast(run: &Path, meta: &Value, sum: &Value) -> bool {
    if !meta.is_object() || b(meta, "isMerge") {
        return false;
    }
    if s(meta, "mode") != "write" || !meta["worktree"].is_object() {
        return false;
    }
    let source = s(&meta["worktree"], "source");
    if source.is_empty() || !Path::new(source).is_dir() {
        return false;
    }
    if run.join(".applied").exists() || run.join(".applied-cleaned").exists() {
        return false;
    }
    let st = if !s(sum, "state").is_empty() {
        s(sum, "state").to_string()
    } else {
        runs::state(run)
    };
    if st != "delivered" || b(sum, "question") || b(sum, "policyRefused") {
        return false;
    }
    if !sum["accept"].is_object() || !b(&sum["accept"], "ok") {
        return false;
    }
    if !sum["protectViolation"].is_null()
        && !sum["protectViolation"]
            .as_array()
            .is_some_and(Vec::is_empty)
    {
        return false;
    }
    if !sum["sourceWriteViolation"].is_null() && sum["sourceWriteViolation"] != json!(false) {
        return false;
    }
    if sum["conflictMarkers"]
        .as_array()
        .is_some_and(|a| !a.is_empty())
    {
        return false;
    }

    let review_doc = load_review_doc(run, sum);
    let sign = review::sign_status(run, meta, &review_doc, false);
    if let Some(ref sg) = sign {
        if b(sg, "required") && !b(sg, "valid") {
            return false;
        }
    }

    if review_doc.is_object() {
        let rstatus = s(&review_doc, "status");
        if matches!(rstatus, "reviewing" | "reworking" | "cancelled") {
            return false;
        }
        match s(&review_doc, "verdict") {
            "PASS" => true,
            "NEEDS_CONTROLLER" => {
                let upgraded_by = s(&review_doc, "upgradedBy");
                let orig = s(&review_doc, "originalVerdict");
                let signed_valid = sign.as_ref().is_some_and(|sg| b(sg, "valid"));
                signed_valid && upgraded_by == "controllerReviewPaths" && orig != "REWORK"
            }
            _ => false,
        }
    } else {
        // If review was explicitly enabled in meta but no review record exists, don't premerge.
        !(meta["review"].is_object() && b(&meta["review"], "enabled"))
    }
}

/// Write initial pending `premerge.json` right before `finish_run` emits `delivered` & updates
/// `index.json` so L0 immediately transitions to `预合并中` (not `快进 —`) for eligible runs.
pub fn mark_pending_before_finish(run: &Path, meta: &Value, sum: &Value) -> bool {
    if disabled() || !is_eligible_fast(run, meta, sum) {
        return false;
    }
    let source = Path::new(s(&meta["worktree"], "source"));
    let head = read_git_head(source).unwrap_or_default();
    let now = iso();
    let doc = json!({
        "status": "pending",
        "baseHead": head,
        "acceptOk": false,
        "conflicts": [],
        "startedAt": now,
        "updatedAt": now,
    });
    let _ = write_json(premerge_path(run), &doc);
    true
}

/// Trigger premerge after `delegate sign <run>` succeeds.
pub fn on_sign_success(run: &Path, meta: &Value) {
    if disabled() {
        return;
    }
    let sum = json(run.join("summary.json"));
    if !is_eligible_fast(run, meta, &sum) {
        return;
    }
    let source = Path::new(s(&meta["worktree"], "source"));
    let head = read_git_head(source).unwrap_or_default();
    let now = iso();
    let doc = json!({
        "status": "pending",
        "baseHead": head,
        "acceptOk": false,
        "conflicts": [],
        "startedAt": now,
        "updatedAt": now,
    });
    let _ = write_json(premerge_path(run), &doc);
    index::upsert_run(run);
    board::refresh_for_run(run);
    spawn_worker(source);
}

fn read_head_cached(
    source: &Path,
    head_cache: Option<&mut HashMap<PathBuf, Option<String>>>,
) -> Option<String> {
    if let Some(cache) = head_cache {
        let key = source.to_path_buf();
        if let Some(v) = cache.get(&key) {
            return v.clone();
        }
        let h = read_git_head(source);
        cache.insert(key, h.clone());
        h
    } else {
        read_git_head(source)
    }
}

/// Evaluate compact live premerge status for `status`, `card`, `snapshot`, and TUI.
/// Strictly zero-lock, zero-write, zero-subprocess (pure filesystem reads).
pub fn status_for_run(
    run: &Path,
    meta: &Value,
    _sum: &Value,
    head_cache: Option<&mut HashMap<PathBuf, Option<String>>>,
) -> Option<Value> {
    if run.join(".applied").exists() || run.join(".applied-cleaned").exists() {
        return None;
    }
    let doc = json(premerge_path(run));
    if !doc.is_object() {
        return None;
    }
    let source_str = s(&meta["worktree"], "source");
    let current_head = if !source_str.is_empty() {
        read_head_cached(Path::new(source_str), head_cache)
    } else {
        None
    };
    let mut out = evaluate_premerge_doc(&doc, current_head.as_deref());
    if !source_str.is_empty() {
        out["source"] = json!(source_str);
    }
    Some(out)
}

fn evaluate_premerge_doc(doc: &Value, current_head: Option<&str>) -> Value {
    let raw_status = s(doc, "status");
    let base_head = s(doc, "baseHead");
    let base_short = &base_head[..7.min(base_head.len())];
    let merged_tree = s(doc, "mergedTree");
    let conflicts = strings(&doc["conflicts"]);

    let head_moved = current_head
        .is_some_and(|h| !h.is_empty() && !base_head.is_empty() && h != base_head);

    let effective_status = match raw_status {
        "pending" => {
            let age = iso_epoch(s(doc, "updatedAt"))
                .or_else(|| iso_epoch(s(doc, "startedAt")))
                .map(|t| (epoch() as i64 - t).max(0))
                .unwrap_or(999);
            if age <= 15 {
                "running"
            } else if head_moved {
                "stale"
            } else {
                "crashed"
            }
        }
        "running" => {
            let pid = doc["workerPid"].as_i64().unwrap_or(0) as i32;
            let age = iso_epoch(s(doc, "updatedAt"))
                .or_else(|| iso_epoch(s(doc, "startedAt")))
                .map(|t| (epoch() as i64 - t).max(0))
                .unwrap_or(999);
            if (pid > 0 && crate::cleanup::alive(pid)) || (pid == 0 && age <= 10) {
                "running"
            } else if head_moved {
                "stale"
            } else {
                "crashed"
            }
        }
        "ready" => {
            if head_moved {
                "stale"
            } else {
                "ready"
            }
        }
        "conflict" => {
            if head_moved {
                "stale"
            } else {
                "conflict"
            }
        }
        "failed" => {
            if head_moved {
                "stale"
            } else {
                "failed"
            }
        }
        "" => {
            // Backward compatibility with minimal premerge.json (e.g. 6.32 stub tests).
            if b(doc, "acceptOk") && !merged_tree.is_empty() && conflicts.is_empty() {
                if head_moved {
                    "stale"
                } else {
                    "ready"
                }
            } else if !conflicts.is_empty() {
                if head_moved {
                    "stale"
                } else {
                    "conflict"
                }
            } else {
                "unknown"
            }
        }
        other => other,
    };

    let mut out = json!({
        "status": effective_status,
        "baseHead": base_head,
        "baseShort": base_short,
    });
    if let Some(ch) = current_head {
        out["currentHead"] = json!(ch);
    }
    if !merged_tree.is_empty() {
        out["mergedTree"] = json!(merged_tree);
    }
    if !s(doc, "ref").is_empty() {
        out["ref"] = doc["ref"].clone();
    }
    if let Some(arr) = doc["changedPaths"].as_array() {
        out["changedCount"] = json!(arr.len());
    }
    if let Some(arr) = doc["ranLayers"].as_array().filter(|a| !a.is_empty()) {
        out["ranLayers"] = json!(arr);
    }
    if doc["reusedAccept"].is_boolean() {
        out["reusedAccept"] = doc["reusedAccept"].clone();
    }
    if doc["elapsedSeconds"].is_number() {
        out["elapsedSeconds"] = doc["elapsedSeconds"].clone();
    }
    if !conflicts.is_empty() {
        let sample: Vec<String> = conflicts.iter().take(5).cloned().collect();
        out["conflicts"] = json!(sample);
        out["conflictCount"] = json!(conflicts.len());
    }
    if !s(doc, "failedLayer").is_empty() {
        out["failedLayer"] = doc["failedLayer"].clone();
    }
    if !s(doc, "log").is_empty() {
        out["log"] = doc["log"].clone();
    }
    if !s(doc, "error").is_empty() {
        out["error"] = json!(clip(s(doc, "error"), 200));
    }
    if let Some(pid) = doc["workerPid"].as_i64() {
        out["workerPid"] = json!(pid);
    }
    if !s(doc, "startedAt").is_empty() {
        out["startedAt"] = doc["startedAt"].clone();
    }
    if !s(doc, "updatedAt").is_empty() {
        out["updatedAt"] = doc["updatedAt"].clone();
    }
    out
}

/// Overlay live HEAD / workerPid state onto a cached `status` JSON object (pure fs, zero locks,
/// zero file opens per run on index hit). Ensures `index.json` cache hits immediately detect
/// `git commit` advancing HEAD!
pub fn overlay_live_status(
    status: &mut Value,
    _run: &Path,
    head_cache: &mut HashMap<PathBuf, Option<String>>,
) {
    if b(status, "applied") {
        if let Some(obj) = status.as_object_mut() {
            obj.remove("premerge");
        }
        return;
    }
    if !status["premerge"].is_object() {
        return;
    }
    let source_str = {
        let pm_src = s(&status["premerge"], "source");
        if !pm_src.is_empty() {
            pm_src.to_string()
        } else {
            s(status, "top").to_string()
        }
    };
    if source_str.is_empty() {
        return;
    }
    let current_head = read_head_cached(Path::new(&source_str), Some(head_cache));
    let pm = &mut status["premerge"];
    let base_head = s(pm, "baseHead").to_string();
    let head_moved = current_head
        .as_deref()
        .is_some_and(|h| !h.is_empty() && !base_head.is_empty() && h != base_head);
    let cur_st = s(pm, "status").to_string();
    let new_st = match cur_st.as_str() {
        "ready" | "conflict" | "failed" if head_moved => Some("stale"),
        "pending" => {
            let age = iso_epoch(s(pm, "updatedAt"))
                .or_else(|| iso_epoch(s(pm, "startedAt")))
                .map(|t| (epoch() as i64 - t).max(0))
                .unwrap_or(999);
            if age > 15 {
                Some(if head_moved { "stale" } else { "crashed" })
            } else {
                Some("running")
            }
        }
        "running" => {
            let pid = pm["workerPid"].as_i64().unwrap_or(0) as i32;
            let age = iso_epoch(s(pm, "updatedAt"))
                .or_else(|| iso_epoch(s(pm, "startedAt")))
                .map(|t| (epoch() as i64 - t).max(0))
                .unwrap_or(999);
            if (pid > 0 && crate::cleanup::alive(pid)) || (pid == 0 && age <= 10) {
                None
            } else {
                Some(if head_moved { "stale" } else { "crashed" })
            }
        }
        _ => None,
    };
    if let Some(st) = new_st {
        pm["status"] = json!(st);
    }
    if let Some(ch) = current_head {
        pm["currentHead"] = json!(ch);
    }
}

/// Zero-allocation check for a single run inside `status` loops: adds its source repo to
/// `repos_to_wake` if it is an unapplied `delivered` write run whose premerge is stale,
/// crashed, or missing (when premerge is enabled), OR a `blocked_mainline` run whose source
/// HEAD has advanced past its last checked commit.
pub fn collect_stale_repo(run: &Path, status: &Value, repos_to_wake: &mut BTreeSet<PathBuf>) {
    if b(status, "applied") {
        return;
    }
    let st = s(status, "state");
    if st == "blocked_mainline" {
        let mb = &status["mainlineBlock"];
        if !mb.is_object() || n(mb, "rerunCount") > 0 {
            return;
        }
        let src = s(&json(run.join("meta.json"))["worktree"], "source").to_string();
        if src.is_empty() {
            return;
        }
        let src_path = PathBuf::from(&src);
        if !src_path.is_dir() {
            return;
        }
        let cur_head = read_git_head(&src_path).unwrap_or_default();
        if !cur_head.is_empty()
            && cur_head != s(mb, "blockedBaseHead")
            && cur_head != s(mb, "checkedHead")
        {
            repos_to_wake.insert(canonical_repo(&src_path));
        }
        return;
    }
    if disabled() || st != "delivered" {
        return;
    }
    if s(status, "mode") != "write" || s(status, "worktree").is_empty() {
        return;
    }
    let needs_wake = match status.get("premerge") {
        Some(pm) if pm.is_object() => matches!(s(pm, "status"), "stale" | "crashed"),
        _ => {
            let meta = json(run.join("meta.json"));
            let sum = json(run.join("summary.json"));
            is_eligible_fast(run, &meta, &sum)
        }
    };
    if !needs_wake {
        return;
    }
    let src = {
        let pm_src = s(&status["premerge"], "source");
        if !pm_src.is_empty() {
            pm_src.to_string()
        } else {
            s(&json(run.join("meta.json"))["worktree"], "source").to_string()
        }
    };
    if src.is_empty() {
        return;
    }
    let src_path = PathBuf::from(src);
    if src_path.is_dir() {
        repos_to_wake.insert(canonical_repo(&src_path));
    }
}

/// Spawn `_merge-worker` once per repo in `repos_to_wake` (zero locks, zero writes).
pub fn wake_repos(repos_to_wake: &BTreeSet<PathBuf>) {
    for repo in repos_to_wake {
        spawn_worker_force(repo);
    }
}

/// Canonical L0/L1 human label for premerge / fast-forward status.
pub fn format_l0(pm: &Value) -> String {
    if !pm.is_object() {
        return "快进 —".into();
    }
    match s(pm, "status") {
        "ready" => {
            let short = s(pm, "baseShort");
            let base = s(pm, "baseHead");
            let tag = if !short.is_empty() {
                short
            } else {
                &base[..7.min(base.len())]
            };
            if tag.is_empty() {
                "快进 ✓".into()
            } else {
                format!("快进 ✓（基于 {tag}）")
            }
        }
        "stale" => "快进 ✗ 主干已前进".into(),
        "running" | "pending" => "预合并中".into(),
        "conflict" => {
            let files = strings(&pm["conflicts"]);
            let total = pm["conflictCount"]
                .as_u64()
                .map(|v| v as usize)
                .unwrap_or(files.len());
            if files.is_empty() {
                "预合并冲突".into()
            } else if total == 1 {
                format!("预合并冲突：{}", files[0])
            } else if total == 2 && files.len() >= 2 {
                format!("预合并冲突：{}、{}", files[0], files[1])
            } else {
                format!("预合并冲突：{} 等 {} 个", files[0], total)
            }
        }
        "failed" => {
            let layer = s(pm, "failedLayer");
            if layer.is_empty() {
                "预合并失败".into()
            } else {
                format!("预合并失败（{layer}）")
            }
        }
        _ => "快进 —".into(),
    }
}

// ============================================================================
// Fast-forward validation & application (called strictly inside `apply.lock`)
// ============================================================================

#[derive(Debug, Clone)]
pub struct FastForwardPlan {
    pub doc: Value,
    pub base_head: String,
    pub merged_tree: String,
    pub changed_paths: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum FastForwardMiss {
    MissingPremerge,
    PremergeRunning,
    PremergeConflict {
        conflicts: Vec<String>,
    },
    PremergeFailed {
        detail: String,
    },
    HeadAdvanced {
        base_head: String,
        current_head: String,
        ahead_count: Option<u64>,
    },
    DirtyOverlap {
        paths: Vec<String>,
    },
    MissingMergedTree,
    RunChangedSincePremerge,
    GeneratePending,
    LargeFilesPresent,
    IncompleteSnapshot,
}

impl FastForwardMiss {
    pub fn human_line(&self) -> String {
        match self {
            FastForwardMiss::MissingPremerge => "未命中预合并，走三方合并".into(),
            FastForwardMiss::PremergeRunning => "后台预合并尚未完成，走三方合并".into(),
            FastForwardMiss::PremergeConflict { conflicts } => {
                if conflicts.is_empty() {
                    "预合并存在冲突，走三方合并".into()
                } else {
                    let sample: Vec<&str> = conflicts.iter().take(3).map(String::as_str).collect();
                    let more = if conflicts.len() > 3 {
                        format!(" 等 {} 个", conflicts.len())
                    } else {
                        String::new()
                    };
                    format!("预合并存在冲突（{}{}），走三方合并", sample.join("、"), more)
                }
            }
            FastForwardMiss::PremergeFailed { detail } => {
                if detail.is_empty() {
                    "预合并验收未通过，走三方合并".into()
                } else {
                    format!("预合并验收未通过（{detail}），走三方合并")
                }
            }
            FastForwardMiss::HeadAdvanced {
                base_head,
                current_head,
                ahead_count,
            } => {
                if let Some(n) = ahead_count.filter(|&n| n > 0) {
                    format!("主干已前进 {n} 个提交，走三方合并")
                } else {
                    let bshort = &base_head[..7.min(base_head.len())];
                    let cshort = &current_head[..7.min(current_head.len())];
                    format!("主干 HEAD 已变化（{bshort} → {cshort}），走三方合并")
                }
            }
            FastForwardMiss::DirtyOverlap { paths } => {
                let sample: Vec<&str> = paths.iter().take(3).map(String::as_str).collect();
                let more = if paths.len() > 3 {
                    format!(" 等 {} 个", paths.len())
                } else {
                    String::new()
                };
                format!(
                    "源工作树改动路径有未提交修改（{}{}），走三方合并",
                    sample.join("、"),
                    more
                )
            }
            FastForwardMiss::MissingMergedTree => "预合并树对象不可用，走三方合并".into(),
            FastForwardMiss::RunChangedSincePremerge => {
                "任务工作树在预合并后有新改动，走三方合并".into()
            }
            FastForwardMiss::GeneratePending => "存在待重新生成标记，走三方合并".into(),
            FastForwardMiss::LargeFilesPresent => "包含大文件改动，走三方合并".into(),
            FastForwardMiss::IncompleteSnapshot => "预合并快照不完整，走三方合并".into(),
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            FastForwardMiss::MissingPremerge => "missing_premerge",
            FastForwardMiss::PremergeRunning => "premerge_running",
            FastForwardMiss::PremergeConflict { .. } => "premerge_conflict",
            FastForwardMiss::PremergeFailed { .. } => "premerge_failed",
            FastForwardMiss::HeadAdvanced { .. } => "head_advanced",
            FastForwardMiss::DirtyOverlap { .. } => "dirty_overlap",
            FastForwardMiss::MissingMergedTree => "missing_merged_tree",
            FastForwardMiss::RunChangedSincePremerge => "run_changed",
            FastForwardMiss::GeneratePending => "generate_pending",
            FastForwardMiss::LargeFilesPresent => "large_files",
            FastForwardMiss::IncompleteSnapshot => "incomplete_snapshot",
        }
    }
}

#[derive(Debug, Clone)]
pub enum FastForwardCheck {
    Hit(FastForwardPlan),
    Miss(FastForwardMiss),
}

fn paths_overlap(a: &str, b: &str) -> bool {
    let a = a.trim_end_matches('/');
    let b = b.trim_end_matches('/');
    a == b
        || a.strip_prefix(b).is_some_and(|r| r.starts_with('/'))
        || b.strip_prefix(a).is_some_and(|r| r.starts_with('/'))
}

fn dirty_source_paths(source: &Path) -> Res<Vec<String>> {
    let raw = git(
        source,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
    )?;
    let mut dirty = Vec::new();
    for entry in raw.split(|b| *b == 0) {
        if entry.len() <= 3 {
            continue;
        }
        let path = String::from_utf8_lossy(&entry[3..]).into_owned();
        if path == ".local" || path.starts_with(".local/") {
            continue;
        }
        dirty.push(path);
    }
    Ok(dirty)
}

fn count_commits_ahead(source: &Path, base: &str, head: &str) -> Option<u64> {
    let range = format!("{base}..{head}");
    git_text(source, &["rev-list", "--count", &range])
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
}

/// Fail-closed validation of `premerge.json` against live source `HEAD` and working tree.
/// Must be called while holding `apply.lock`.
pub fn validate_fast_forward(run: &Path, source: &Path) -> FastForwardCheck {
    let doc = json(premerge_path(run));
    if !doc.is_object() {
        return FastForwardCheck::Miss(FastForwardMiss::MissingPremerge);
    }
    let status = s(&doc, "status");
    if matches!(status, "running" | "pending")
        || (status == "cancelled" && s(&doc, "cancelledBy") == "apply")
    {
        return FastForwardCheck::Miss(FastForwardMiss::PremergeRunning);
    }
    if status == "conflict" {
        return FastForwardCheck::Miss(FastForwardMiss::PremergeConflict {
            conflicts: strings(&doc["conflicts"]),
        });
    }
    if status == "failed" {
        let detail = {
            let layer = s(&doc, "failedLayer");
            if !layer.is_empty() {
                layer.to_string()
            } else {
                s(&doc, "error").to_string()
            }
        };
        return FastForwardCheck::Miss(FastForwardMiss::PremergeFailed { detail });
    }
    let base_head = s(&doc, "baseHead").to_string();
    let merged_tree = s(&doc, "mergedTree").to_string();
    let conflicts = strings(&doc["conflicts"]);
    if !conflicts.is_empty() {
        return FastForwardCheck::Miss(FastForwardMiss::PremergeConflict { conflicts });
    }
    if base_head.is_empty() || merged_tree.is_empty() || !b(&doc, "acceptOk") {
        return FastForwardCheck::Miss(FastForwardMiss::PremergeFailed {
            detail: s(&doc, "error").to_string(),
        });
    }
    if b(&doc, "hasLarge") {
        return FastForwardCheck::Miss(FastForwardMiss::LargeFilesPresent);
    }
    if doc.get("snapshotComplete") == Some(&json!(false)) {
        return FastForwardCheck::Miss(FastForwardMiss::IncompleteSnapshot);
    }
    if run.join(".generate-pending").exists() {
        return FastForwardCheck::Miss(FastForwardMiss::GeneratePending);
    }

    let Some(current_head) = read_git_head(source) else {
        return FastForwardCheck::Miss(FastForwardMiss::HeadAdvanced {
            base_head,
            current_head: String::new(),
            ahead_count: None,
        });
    };
    if current_head != base_head {
        let ahead_count = count_commits_ahead(source, &base_head, &current_head);
        return FastForwardCheck::Miss(FastForwardMiss::HeadAdvanced {
            base_head,
            current_head,
            ahead_count,
        });
    }

    // Verify `mergedTree` actually exists as a git tree object in source.
    let tree_type = git_text(source, &["cat-file", "-t", &merged_tree]).unwrap_or_default();
    if tree_type.trim() != "tree" {
        return FastForwardCheck::Miss(FastForwardMiss::MissingMergedTree);
    }

    // Verify the run's worktree `after` tree hasn't drifted since premerge ran.
    let recorded_after = s(&doc, "afterTree");
    if !recorded_after.is_empty() {
        let cur_after = s(&json(run.join("changes.json")), "after").to_string();
        if !cur_after.is_empty() && cur_after != recorded_after {
            return FastForwardCheck::Miss(FastForwardMiss::RunChangedSincePremerge);
        }
    }

    let changed_paths = strings(&doc["changedPaths"]);
    let Ok(dirty) = dirty_source_paths(source) else {
        return FastForwardCheck::Miss(FastForwardMiss::DirtyOverlap {
            paths: vec!["<git status 失败>".into()],
        });
    };
    if !changed_paths.is_empty() {
        let mut overlaps: Vec<String> = Vec::new();
        for p in &changed_paths {
            if dirty.iter().any(|d| paths_overlap(p, d)) {
                overlaps.push(p.clone());
            }
        }
        if !overlaps.is_empty() {
            return FastForwardCheck::Miss(FastForwardMiss::DirtyOverlap { paths: overlaps });
        }
    }
    FastForwardCheck::Hit(FastForwardPlan {
        doc,
        base_head,
        merged_tree,
        changed_paths,
    })
}

/// Legacy signature preserved for `gc::premerge::validate_fast_forward` unit tests.
pub fn validate_fast_forward_basic(run: &Path, source: &Path) -> Option<Value> {
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

struct TreeEntry {
    mode: String,
    oid: String,
}

fn read_tree_entries(source: &Path, tree: &str, paths: &[String]) -> Res<HashMap<String, TreeEntry>> {
    let mut entries = HashMap::new();
    if paths.is_empty() {
        return Ok(entries);
    }
    for chunk in paths.chunks(200) {
        let mut cmd = git_command();
        cmd.arg("-C")
            .arg(source)
            .args(["ls-tree", "-r", "-z", tree, "--"]);
        for p in chunk {
            cmd.arg(p);
        }
        let out = cmd.output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "git ls-tree failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        for rec in out.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let Some(tab) = rec.iter().position(|b| *b == b'\t') else {
                continue;
            };
            let header = String::from_utf8_lossy(&rec[..tab]);
            let path = String::from_utf8_lossy(&rec[tab + 1..]).into_owned();
            let mut parts = header.split_whitespace();
            let (Some(mode), Some(kind), Some(oid)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if kind == "blob" {
                entries.insert(
                    path,
                    TreeEntry {
                        mode: mode.to_string(),
                        oid: oid.to_string(),
                    },
                );
            }
        }
    }
    Ok(entries)
}

fn batch_read_blobs(source: &Path, oids: &[String]) -> Res<HashMap<String, Vec<u8>>> {
    let mut result = HashMap::new();
    if oids.is_empty() {
        return Ok(result);
    }
    let unique: Vec<&String> = {
        let mut seen = BTreeSet::new();
        oids.iter().filter(|o| seen.insert(*o)).collect()
    };
    let mut child = git_command()
        .arg("-C")
        .arg(source)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;

    if let Some(mut stdin) = child.stdin.take() {
        for oid in &unique {
            writeln!(stdin, "{oid}").map_err(|e| e.to_string())?;
        }
    }
    let stdout = child.stdout.take().ok_or("cat-file missing stdout")?;
    let mut reader = BufReader::new(stdout);
    let mut header = String::new();
    for _ in 0..unique.len() {
        header.clear();
        let n = reader.read_line(&mut header).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("unexpected EOF from git cat-file --batch".into());
        }
        let line = header.trim_end_matches(['\r', '\n']);
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 || parts[1] != "blob" {
            return Err(format!("missing or non-blob object in mergedTree: {line}"));
        }
        let oid = parts[0].to_string();
        let size: usize = parts[2]
            .parse()
            .map_err(|_| format!("invalid blob size: {}", parts[2]))?;
        let mut buf = vec![0u8; size];
        reader.read_exact(&mut buf).map_err(|e| e.to_string())?;
        let mut nl = [0u8; 1];
        reader.read_exact(&mut nl).map_err(|e| e.to_string())?;
        result.insert(oid, buf);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("git cat-file --batch exited non-zero".into());
    }
    Ok(result)
}

enum FfOp {
    Write {
        path: String,
        target: PathBuf,
        mode: String,
        content: Vec<u8>,
    },
    Delete {
        path: String,
        target: PathBuf,
    },
}

/// Execute fast-forward application of `plan.merged_tree`'s `changed_paths` directly to `source`
/// working tree (leaving git index untouched = uncommitted changes).
/// Returns `Ok(None)` if any pre-write safety check fails (caller falls back to 3-way merge).
pub(crate) fn apply_fast_forward(
    run: &Path,
    source: &Path,
    plan: &FastForwardPlan,
    dry: bool,
    conclusion: &mut Value,
) -> Res<Option<worktree::ApplyInnerResult>> {
    let t0 = Instant::now();
    let entries = match read_tree_entries(source, &plan.merged_tree, &plan.changed_paths) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    let oids: Vec<String> = entries.values().map(|e| e.oid.clone()).collect();
    let blobs = match batch_read_blobs(source, &oids) {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };

    // Pre-validate every single path before writing anything to source (fail-closed!).
    let mut ops: Vec<FfOp> = Vec::with_capacity(plan.changed_paths.len());
    for path in &plan.changed_paths {
        let target = source.join(path);
        if worktree::through_symlink(source, &target)
            || worktree::entry_mode(&target).as_deref() == Some("dir")
        {
            return Ok(None);
        }
        if let Some(ent) = entries.get(path) {
            if !["100644", "100755", "120000"].contains(&ent.mode.as_str()) {
                return Ok(None);
            }
            let Some(content) = blobs.get(&ent.oid) else {
                return Ok(None);
            };
            if !worktree::conflict_labels_in_bytes(Path::new(path), content).is_empty() {
                return Ok(None);
            }
            ops.push(FfOp::Write {
                path: path.clone(),
                target,
                mode: ent.mode.clone(),
                content: content.clone(),
            });
        } else {
            ops.push(FfOp::Delete {
                path: path.clone(),
                target,
            });
        }
    }

    let mut written = Vec::with_capacity(ops.len());
    let mut stdout_lines = Vec::with_capacity(ops.len());
    if dry {
        for op in &ops {
            match op {
                FfOp::Write { path, .. } => {
                    stdout_lines.push(format!(" {:<16} {path}", "fast-forward"));
                }
                FfOp::Delete { path, .. } => {
                    stdout_lines.push(format!(" {:<16} {path}", "deleted"));
                }
            }
        }
        eprintln!("delegate: dry run; nothing written");
    } else {
        {
            // Hold source-watch lock ONLY during the brief write-to-disk + fingerprint register window.
            let _write_guard = if !ops.is_empty() {
                Some(crate::source_watch::guard(source, true)?)
            } else {
                None
            };
            let before_actions = if _write_guard.is_some() {
                Some(crate::source_watch::fingerprint(source)?)
            } else {
                None
            };
            for op in ops {
                match op {
                    FfOp::Write {
                        path,
                        target,
                        mode,
                        content,
                    } => {
                        written.push(path.clone());
                        if let Err(e) = worktree::write_entry(&target, &mode, &content) {
                            if let Some(before) = &before_actions {
                                let _ = crate::source_watch::register_matching(
                                    source, before, &written, &[],
                                );
                            }
                            return Err(format!("apply fast-forward {}: {e}", target.display()));
                        }
                        stdout_lines.push(format!(" {:<16} {path}", "fast-forward"));
                    }
                    FfOp::Delete { path, target } => {
                        written.push(path.clone());
                        if target.exists() || target.is_symlink() {
                            if let Err(e) = fs::remove_file(&target) {
                                if let Some(before) = &before_actions {
                                    let _ = crate::source_watch::register_matching(
                                        source, before, &written, &[],
                                    );
                                }
                                return Err(format!("apply fast-forward {}: {e}", target.display()));
                            }
                            worktree::remove_empty_parents(&target, source);
                        }
                        stdout_lines.push(format!(" {:<16} {path}", "deleted"));
                    }
                }
            }
            if let Some(before) = &before_actions {
                crate::source_watch::register_matching(source, before, &written, &[])?;
            }
        }

        // Record `.applied` & `.sync-base` across the run and its parent reply chain.
        let at = iso();
        let after_tree = {
            let rec_after = s(&plan.doc, "afterTree");
            if !rec_after.is_empty() {
                rec_after.to_string()
            } else {
                s(&json(run.join("changes.json")), "after").to_string()
            }
        };
        let state = json!({
            "at": at,
            "head": plan.base_head,
            "tree": after_tree,
            "fastForward": true,
            "mergedTree": plan.merged_tree,
        });
        let mut chain = run.to_path_buf();
        loop {
            write_json(chain.join(".applied"), &state)?;
            write_json(chain.join(".sync-base"), &state)?;
            let mut chain_meta = json(chain.join("meta.json"));
            chain_meta["appliedAt"] = json!(at);
            if !plan.base_head.is_empty() {
                chain_meta["appliedHead"] = json!(plan.base_head);
            }
            let _ = write_json(chain.join("meta.json"), &chain_meta);
            if !after_tree.is_empty() {
                refs::pin_slots(
                    &chain,
                    &chain_meta,
                    &[("applied", &after_tree), ("sync-base", &after_tree)],
                );
            }
            let parent = s(&chain_meta, "parent").to_string();
            if parent.is_empty()
                || !chain
                    .parent()
                    .unwrap_or(Path::new("/"))
                    .join(&parent)
                    .is_dir()
            {
                break;
            }
            chain = chain.parent().unwrap().join(parent);
        }
    }

    let elapsed_ms = (t0.elapsed().as_secs_f64() * 1000.0).round() as u64;
    conclusion["fastForward"] = json!({
        "hit": true,
        "baseHead": plan.base_head,
        "mergedTree": plan.merged_tree,
        "ms": elapsed_ms,
    });
    Ok(Some(worktree::ApplyInnerResult {
        code: 0,
        written,
        regenerated: vec![],
        had_dirty_merge: false,
        stdout_lines,
    }))
}

// ============================================================================
// Persistent integration worktree & `_merge-worker` background execution
// ============================================================================

fn worktree_private_gitdir(wt: &Path) -> Option<PathBuf> {
    let dot_git = wt.join(".git");
    if !dot_git.is_file() {
        return None;
    }
    let text = fs::read_to_string(&dot_git).ok()?;
    let raw = text.lines().next()?.trim().strip_prefix("gitdir:")?.trim();
    let p = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        wt.join(raw)
    };
    let canon = fs::canonicalize(&p).unwrap_or(p);
    // Only return true linked worktree admin dirs (`.../.git/worktrees/<name>`).
    if canon
        .parent()
        .and_then(|par| par.file_name())
        .is_some_and(|n| n == "worktrees")
    {
        Some(canon)
    } else {
        None
    }
}

fn ensure_managed_entries(source: &Path, wt_path: &Path, work_cfg: &Value) -> Res<Vec<String>> {
    let copies = strings(&work_cfg["copy"]);
    let links = strings(&work_cfg["link"]);
    let managed: Vec<String> = copies.iter().chain(links.iter()).cloned().collect();
    for item in &managed {
        let origin = source.join(item);
        let target = wt_path.join(item);
        if target.is_dir()
            && !target.is_symlink()
            && fs::read_dir(&target).is_ok_and(|mut d| d.next().is_none())
        {
            let _ = fs::remove_dir(&target);
        }
        if !origin.exists() && !origin.is_symlink() {
            continue;
        }
        if target.exists() || target.is_symlink() {
            continue;
        }
        fs::create_dir_all(target.parent().unwrap_or(wt_path)).map_err(|e| e.to_string())?;
        if links.contains(item) {
            symlink(&origin, &target).map_err(|e| e.to_string())?;
        } else {
            worktree::copy_rec(&origin, &target)?;
        }
    }
    for item in &managed {
        let _ = git(wt_path, &["update-index", "--skip-worktree", "--", item]);
    }
    Ok(managed)
}

fn clean_integration_worktree(wt_path: &Path, head: &str, managed: &[String]) -> Res<()> {
    // Clear any stale index.lock inside THIS integration worktree's private admin dir
    // (safe because we hold the exclusive per-repo `_merge-worker` lock; NEVER touches source `.git/index.lock`).
    if let Some(admin_dir) = worktree_private_gitdir(wt_path) {
        let stale_lock = admin_dir.join("index.lock");
        if stale_lock.is_file() {
            let _ = fs::remove_file(&stale_lock);
        }
    }
    let _ = git(wt_path, &["update-index", "-q", "--refresh"]);
    git(wt_path, &["reset", "--hard", "-q", head])?;
    // `git clean -fd -q` (without `-x`) preserves `.gitignore`d caches (`target/`, `node_modules/`),
    // and explicit `-e` flags also preserve them even in repos without `.gitignore`.
    let mut clean_args: Vec<String> = vec![
        "clean".into(),
        "-fd".into(),
        "-q".into(),
        "-e".into(),
        "target".into(),
        "-e".into(),
        "**/target".into(),
        "-e".into(),
        "node_modules".into(),
        "-e".into(),
        "**/node_modules".into(),
    ];
    for m in managed {
        clean_args.push("-e".into());
        clean_args.push(m.clone());
    }
    let refs: Vec<&str> = clean_args.iter().map(String::as_str).collect();
    git(wt_path, &refs)?;
    Ok(())
}

/// Ensure `<cache>/premerge/<repo-key>/worktree` exists, is attached to `source`, and is cleanly
/// reset to `head` while preserving `target/` and `node_modules/`.
fn ensure_integration_worktree(
    source: &Path,
    head: &str,
    work_cfg: &Value,
    env_cfg: &Value,
) -> Res<(PathBuf, Vec<String>)> {
    let wt_path = integration_worktree_path(source);
    let copies = strings(&work_cfg["copy"]);
    let links = strings(&work_cfg["link"]);
    let managed: Vec<String> = copies.iter().chain(links.iter()).cloned().collect();

    let mut usable = wt_path.join(".git").is_file()
        && worktree_private_gitdir(&wt_path).is_some_and(|d| d.is_dir());

    if usable && clean_integration_worktree(&wt_path, head, &managed).is_err() {
        usable = false;
    }

    if !usable {
        if let Some(parent) = wt_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // Preserve existing `target/` and `node_modules/` across a forced recreation if present.
        let stash_dir = wt_path
            .parent()
            .unwrap_or(source)
            .join(format!(".preserve-{}", std::process::id()));
        let mut preserved = Vec::new();
        if wt_path.is_dir() {
            for cache_name in ["target", "node_modules"] {
                let p = wt_path.join(cache_name);
                if p.exists() && !p.is_symlink() {
                    let _ = fs::create_dir_all(&stash_dir);
                    if fs::rename(&p, stash_dir.join(cache_name)).is_ok() {
                        preserved.push(cache_name);
                    }
                }
            }
            let _ = git(
                source,
                &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
            );
            let _ = fs::remove_dir_all(&wt_path);
            let _ = git(source, &["worktree", "prune"]);
        }

        let add_res = git(
            source,
            &[
                "worktree",
                "add",
                "--detach",
                &wt_path.to_string_lossy(),
                head,
            ],
        );
        if let Err(err) = add_res {
            let _ = fs::remove_dir_all(&stash_dir);
            return Err(err);
        }
        for cache_name in preserved {
            let src_p = stash_dir.join(cache_name);
            let dst_p = wt_path.join(cache_name);
            if src_p.exists() && !dst_p.exists() {
                let _ = fs::rename(&src_p, &dst_p);
            }
        }
        let _ = fs::remove_dir_all(&stash_dir);
    }

    let managed = ensure_managed_entries(source, &wt_path, work_cfg)?;

    // Run one-time setup commands if configured and not yet run for this persistent worktree.
    let setup_marker = integration_setup_marker(source);
    if !setup_marker.is_file() {
        let mut setup = strings(&work_cfg["setup"]);
        setup.extend(strings(&work_cfg["writeSetup"]));
        if !setup.is_empty() {
            let setup_log = wt_path
                .parent()
                .unwrap_or(&wt_path)
                .join("setup.log");
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&setup_log)
                .map_err(|e| e.to_string())?;
            let _slot = lane::acquire_shell("premerge-setup", None, None, |_, _| {})?;
            let dummy_run = wt_path.parent().unwrap_or(&wt_path);
            for cmd in &setup {
                let _ = writeln!(file, "$ {cmd}");
                let _ = file.flush();
                let (code, _) = run_shell_with(
                    (cmd, "premerge-setup"),
                    &wt_path,
                    dummy_run,
                    env_cfg,
                    None,
                    &mut file,
                    None,
                    true,
                )?;
                let _ = writeln!(file, "[exit {code}]");
                if code != 0 {
                    return Err(format!(
                        "premerge integration worktree setup failed (exit {code}): {cmd}; see {}",
                        setup_log.display()
                    ));
                }
            }
        }
        let _ = write(&setup_marker, format!("{}\n", iso()));
    }

    Ok((wt_path, managed))
}

fn discover_runs_for_repo(source_canon: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut push_dir = |p: PathBuf| {
        if p.is_dir() {
            let c = fs::canonicalize(&p).unwrap_or(p);
            if seen_dirs.insert(c.clone()) {
                dirs.push(c);
            }
        }
    };
    push_dir(runs_root());
    push_dir(source_canon.join(".local/run/delegate"));
    let state = state_dir();
    for (repo_p, runs_d) in index::discover_snapshot_repos(&state, &[], None) {
        if canonical_repo(&repo_p) == source_canon {
            push_dir(runs_d);
        }
    }

    let mut all_in_repo: Vec<PathBuf> = Vec::new();
    let mut seen_runs: BTreeSet<PathBuf> = BTreeSet::new();
    for d in dirs {
        for r in runs::all_runs_in(&d) {
            let c = fs::canonicalize(&r).unwrap_or(r);
            if seen_runs.insert(c.clone()) {
                all_in_repo.push(c);
            }
        }
    }

    // Collapse reply chains to their latest head and build parent map for downstream lookups.
    let mut parent_ids: BTreeSet<String> = BTreeSet::new();
    let mut parent_map: HashMap<String, String> = HashMap::new();
    let mut run_metas: Vec<(PathBuf, String, Value)> = Vec::with_capacity(all_in_repo.len());
    for r in all_in_repo {
        let meta = json(r.join("meta.json"));
        let id = events::run_id_for(&r, &meta);
        let p = s(&meta, "parent").trim().to_string();
        if !p.is_empty() {
            parent_ids.insert(p.clone());
            if !id.is_empty() {
                parent_map.insert(id.clone(), p);
            }
        }
        run_metas.push((r, id, meta));
    }

    let work_items = crate::work::load_readonly_in(&state);
    let mut candidates: Vec<(usize, u64, String, PathBuf)> = Vec::new();
    for (r, id, meta) in run_metas {
        if !meta.is_object() || b(&meta, "isMerge") {
            continue;
        }
        if parent_ids.contains(&id) {
            continue;
        }
        let src = s(&meta["worktree"], "source");
        if src.is_empty() || canonical_repo(Path::new(src)) != source_canon {
            continue;
        }
        if r.join(".applied").exists() || r.join(".applied-cleaned").exists() {
            continue;
        }
        let blocked = crate::work::blocked_downstream_for_run_with_items(
            &work_items,
            &r,
            &meta,
            &state,
            Some(&parent_map),
        )
        .len();
        let ns = meta["startedNs"].as_u64().unwrap_or(0);
        candidates.push((blocked, ns, id, r));
    }
    sort_premerge_candidates(&mut candidates);
    candidates.into_iter().map(|(_, _, _, p)| p).collect()
}

/// Sort premerge candidates by transitive blocked downstream count (descending),
/// then `startedNs` (ascending FIFO), then `run_id` (ascending).
fn sort_premerge_candidates(candidates: &mut [(usize, u64, String, PathBuf)]) {
    candidates.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
}

fn run_needs_premerge(run: &Path, source_head: &str) -> bool {
    let doc = json(premerge_path(run));
    if !doc.is_object() {
        return true;
    }
    let st = s(&doc, "status");
    let base = s(&doc, "baseHead");
    if base != source_head {
        return true;
    }
    matches!(st, "pending" | "running" | "crashed" | "cancelled" | "")
}

/// Full eligibility verification inside `_merge-worker` (includes live `controllerReviewPaths`
/// and repo `defaults.protect` checks).
fn is_eligible_strict(
    run: &Path,
    meta: &Value,
    sum: &Value,
    repo_cfg: &Value,
    protect_rules: &[String],
    controller_patterns: &[String],
) -> bool {
    if !is_eligible_fast(run, meta, sum) {
        return false;
    }
    if !protect_rules.is_empty() {
        if let Some(changed_paths) = worktree::pending_changed_paths(run, meta) {
            if changed_paths
                .iter()
                .any(|p| worktree::matches_rule(p, protect_rules))
            {
                return false;
            }
        }
    }
    let mut merged_patterns = strings(&meta["controllerReviewPaths"]);
    for p in controller_patterns {
        if !merged_patterns.contains(p) {
            merged_patterns.push(p.clone());
        }
    }
    if !merged_patterns.is_empty() {
        let mut eval_meta = meta.clone();
        eval_meta["controllerReviewPaths"] = json!(merged_patterns);
        match review::evaluate_controller_paths(run, &eval_meta, true) {
            Ok(Some(hit)) => {
                let sign_doc = json(run.join("sign.json"));
                if s(&sign_doc, "hash") != hit.hash {
                    return false;
                }
            }
            Ok(None) => {}
            Err(_) => return false,
        }
    }
    let review_doc = load_review_doc(run, sum);
    if !review_doc.is_object() && review::is_enabled(&repo_cfg["review"]) {
        return false;
    }
    true
}

fn diff_tree_paths(wt_path: &Path, base_tree: &str, merged_tree: &str) -> Res<Vec<String>> {
    let raw = git(
        wt_path,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "--no-renames",
            "-z",
            base_tree,
            merged_tree,
        ],
    )?;
    let mut paths: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Returns `(can_reuse, reused_from_dirty_base)`:
/// - `(true, false)` when `mergedTree == accept.tree` (exact bit-for-bit tree match).
/// - `(true, true)` when source `HEAD` has not moved since the run started (`worktree_base == source_head`),
///   so `accept` already passed on `HEAD + dirty_source + run`; `apply` will still run full
///   `accept_validity` against the live source tree (`reusedFromDirtyBase: true`).
/// - `(false, false)` whenever `HEAD` advanced or acceptance evidence is incomplete.
#[allow(clippy::too_many_arguments)]
fn can_reuse_run_acceptance(
    sum: &Value,
    meta: &Value,
    source_head: &str,
    wt_path: &Path,
    merged_tree: &str,
    snap_complete: bool,
    run: &Path,
    touches_generated: bool,
) -> (bool, bool) {
    if !snap_complete || merged_tree.is_empty() || touches_generated {
        return (false, false);
    }
    if run.join(".generate-pending").exists() {
        return (false, false);
    }
    let acc = &sum["accept"];
    let acc_tree = s(acc, "tree");
    if !(acc.is_object()
        && b(acc, "ok")
        && b(acc, "snapshotComplete")
        && !acc_tree.is_empty()
        && acc_tree == s(acc, "treeAfter"))
    {
        return (false, false);
    }
    if acc_tree == merged_tree {
        return (true, false);
    }
    let wt_base = crate::changes::worktree_base_commit(run, meta)
        .filter(|x| !x.is_empty())
        .unwrap_or_else(|| s(&meta["worktree"], "base").to_string());
    if !wt_base.is_empty() {
        let same_head = wt_base == source_head
            || git_text(
                wt_path,
                &["rev-parse", "-q", "--verify", &format!("{wt_base}^")],
            )
            .ok()
            .as_deref()
            == Some(source_head);
        if same_head {
            return (true, true);
        }
    }
    (false, false)
}

fn premerge_workdir(source: &Path, meta: &Value, wt_path: &Path) -> PathBuf {
    let src_wd = s(&meta["worktree"], "sourceWorkdir");
    if src_wd.is_empty() {
        return wt_path.to_path_buf();
    }
    let canon_src = canonical_repo(source);
    let canon_wd = canonical_repo(Path::new(src_wd));
    if let Ok(rel) = canon_wd.strip_prefix(&canon_src) {
        if !rel.as_os_str().is_empty() {
            let sub = wt_path.join(rel);
            if sub.is_dir() {
                return sub;
            }
        }
    }
    wt_path.to_path_buf()
}

/// Detect corrupted incremental/linker cache failures in premerge generate/accept output
/// (e.g. `mold: error: undefined symbol: ...` or `error: linking with \`cc\` failed`).
pub(crate) fn is_linker_cache_error(output: &str) -> bool {
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("undefined symbol") {
            return true;
        }
        if let Some(pos) = lower.find("linking with") {
            if lower[pos + "linking with".len()..].contains("failed") {
                return true;
            }
        }
    }
    false
}

fn read_log_slice(path: &Path, start_offset: u64) -> String {
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len <= start_offset {
        return String::new();
    }
    const MAX_BYTES: u64 = 512 * 1024;
    let seek_to = if len - start_offset > MAX_BYTES {
        len - MAX_BYTES
    } else {
        start_offset
    };
    if f.seek(SeekFrom::Start(seek_to)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

fn is_valid_crate_ident(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn crate_name_from_artifact_stem(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '.');
    let without_ext = trimmed.split('.').next().unwrap_or(trimmed);
    let (prefix, hash) = without_ext.rsplit_once('-')?;
    if hash.len() < 8 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let name = prefix.strip_prefix("lib").unwrap_or(prefix);
    is_valid_crate_ident(name).then(|| name.to_string())
}

fn cargo_toml_package_name(cargo_toml: &Path) -> Option<String> {
    let text = fs::read_to_string(cargo_toml).ok()?;
    let mut in_package = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("name") {
            let rest = rest.trim_start();
            if let Some(val) = rest.strip_prefix('=') {
                let val = val.trim();
                let unquoted = val
                    .strip_prefix('"')
                    .and_then(|s| s.split('"').next())
                    .or_else(|| val.strip_prefix('\'').and_then(|s| s.split('\'').next()))?;
                if is_valid_crate_ident(unquoted) {
                    return Some(unquoted.to_string());
                }
            }
        }
    }
    None
}

pub(crate) fn extract_affected_crates(
    output: &str,
    command: &str,
    wt_path: &Path,
    changed_paths: &[String],
) -> Vec<String> {
    let mut crates = BTreeSet::<String>::new();

    for line in output.lines() {
        // 1) `error: could not compile \`xtask\` (bin "xtask") ...`
        if let Some(pos) = line.find("could not compile ") {
            let rest = &line[pos + "could not compile ".len()..];
            for quote in ['`', '\'', '"'] {
                if let Some(after) = rest.strip_prefix(quote) {
                    if let Some(name) = after.split(quote).next() {
                        if is_valid_crate_ident(name) {
                            crates.insert(name.to_string());
                        }
                    }
                }
            }
        }
        // 2) Paths referencing `deps/<artifact>`, `incremental/<crate>-<hash>`, `.fingerprint/<crate>-<hash>`.
        for marker in ["deps/", "incremental/", ".fingerprint/"] {
            let mut rest = line;
            while let Some(pos) = rest.find(marker) {
                let after = &rest[pos + marker.len()..];
                let seg = after
                    .split(|c: char| c.is_whitespace() || matches!(c, '/' | ':' | ')' | '(' | '"' | '\'' | '`' | ',' | ';'))
                    .next()
                    .unwrap_or("");
                if let Some(name) = crate_name_from_artifact_stem(seg) {
                    crates.insert(name);
                }
                rest = after;
            }
        }
        // 3) Rust symbol prefixes on `undefined symbol` lines (e.g. `drop_in_place<my_crate::Foo>`).
        if line.to_ascii_lowercase().contains("undefined symbol") {
            let bytes = line.as_bytes();
            let mut i = 0;
            while i + 2 < bytes.len() {
                if bytes[i] == b':' && bytes[i + 1] == b':' {
                    let mut start = i;
                    while start > 0
                        && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_')
                    {
                        start -= 1;
                    }
                    // Only take leading path segment (not `foo::bar::` -> skip `bar`).
                    let preceded_by_colon =
                        start >= 2 && bytes[start - 1] == b':' && bytes[start - 2] == b':';
                    if !preceded_by_colon && start < i {
                        let seg = &line[start..i];
                        if !matches!(
                            seg,
                            "core" | "std" | "alloc" | "proc_macro" | "test" | "drop_glue"
                        ) && is_valid_crate_ident(seg)
                        {
                            crates.insert(seg.to_string());
                        }
                    }
                    i += 2;
                    continue;
                }
                i += 1;
            }
        }
    }

    // 4) `-p <pkg>` / `--package <pkg>` / `--bin <bin>` in command.
    let cmd_tokens: Vec<&str> = command.split_whitespace().collect();
    for (idx, tok) in cmd_tokens.iter().enumerate() {
        let candidate = if matches!(*tok, "-p" | "--package" | "--bin" | "--test" | "--example") {
            cmd_tokens.get(idx + 1).copied()
        } else {
            tok.strip_prefix("--package=")
                .or_else(|| tok.strip_prefix("--bin="))
        };
        if let Some(raw) = candidate {
            let clean = raw.trim_matches(|c: char| matches!(c, '"' | '\'' | ';'));
            if is_valid_crate_ident(clean) {
                crates.insert(clean.to_string());
            }
        }
    }

    // 5) Nearest `Cargo.toml` owning any changed path inside `wt_path`.
    for rel in changed_paths {
        let mut dir = wt_path.join(rel);
        if dir.is_file() || !dir.is_dir() {
            if let Some(p) = dir.parent() {
                dir = p.to_path_buf();
            }
        }
        while dir.starts_with(wt_path) {
            let manifest = dir.join("Cargo.toml");
            if manifest.is_file() {
                if let Some(name) = cargo_toml_package_name(&manifest) {
                    crates.insert(name);
                }
                break;
            }
            if dir == wt_path {
                break;
            }
            let Some(p) = dir.parent() else { break };
            dir = p.to_path_buf();
        }
    }

    crates.into_iter().collect()
}

fn premerge_owned_target_dirs(wt_path: &Path, cwd: &Path, env: &Value) -> Vec<PathBuf> {
    let premerge_base = wt_path.parent().unwrap_or(wt_path);
    let premerge_root = fs::canonicalize(premerge_base).unwrap_or_else(|_| premerge_base.to_path_buf());
    let mut out = Vec::<PathBuf>::new();
    let mut seen = BTreeSet::<PathBuf>::new();

    let mut try_add = |cand: PathBuf| {
        if !cand.is_dir() {
            return;
        }
        let Ok(canon) = fs::canonicalize(&cand) else {
            return;
        };
        // Strict safety boundary: ONLY touch target dirs strictly inside this repo's premerge dir,
        // NEVER any worktree target or source repo target (even via symlinks or CARGO_TARGET_DIR).
        if canon.starts_with(&premerge_root) && canon != premerge_root && seen.insert(canon.clone())
        {
            out.push(canon);
        }
    };

    if let Some(env_target) = env.get("CARGO_TARGET_DIR").and_then(Value::as_str) {
        let trimmed = env_target.trim();
        if !trimmed.is_empty() {
            let p = PathBuf::from(trimmed);
            try_add(if p.is_absolute() { p } else { cwd.join(p) });
        }
    }
    if let Ok(proc_target) = std::env::var("CARGO_TARGET_DIR") {
        let trimmed = proc_target.trim();
        if !trimmed.is_empty() {
            let p = PathBuf::from(trimmed);
            try_add(if p.is_absolute() { p } else { cwd.join(p) });
        }
    }
    try_add(wt_path.join("target"));
    if cwd != wt_path && cwd.starts_with(wt_path) {
        try_add(cwd.join("target"));
    }
    out
}

fn has_cargo_cache_subdirs(dir: &Path) -> bool {
    dir.join("incremental").is_dir()
        || dir.join("deps").is_dir()
        || dir.join(".fingerprint").is_dir()
}

fn discover_profile_dirs(target_dir: &Path) -> Vec<PathBuf> {
    let mut profiles = Vec::new();
    let Ok(rd) = fs::read_dir(target_dir) else {
        return profiles;
    };
    for ent in rd.flatten() {
        let p = ent.path();
        if !p.is_dir() || p.is_symlink() {
            continue;
        }
        let name = ent.file_name().to_string_lossy().into_owned();
        if matches!(name.as_str(), "incremental" | "deps" | ".fingerprint" | "tmp") {
            continue;
        }
        if has_cargo_cache_subdirs(&p) {
            profiles.push(p);
            continue;
        }
        // Check `<target>/<triple>/<profile>` layout.
        if let Ok(sub_rd) = fs::read_dir(&p) {
            for sub in sub_rd.flatten() {
                let sp = sub.path();
                if sp.is_dir() && !sp.is_symlink() && has_cargo_cache_subdirs(&sp) {
                    profiles.push(sp);
                }
            }
        }
    }
    profiles
}

fn entry_matches_crates(entry_name: &str, crates: &[String], allow_lib_prefix: bool) -> bool {
    for c in crates {
        let under = c.replace('-', "_");
        let dash = c.replace('_', "-");
        for variant in [&under, &dash] {
            if entry_name == variant.as_str()
                || entry_name.starts_with(&format!("{variant}-"))
                || entry_name.starts_with(&format!("{variant}."))
            {
                return true;
            }
            if allow_lib_prefix
                && (entry_name.starts_with(&format!("lib{variant}-"))
                    || entry_name.starts_with(&format!("lib{variant}.")))
            {
                return true;
            }
        }
    }
    false
}

fn remove_path_any(path: &Path) -> bool {
    if path.is_symlink() || path.is_file() {
        fs::remove_file(path).is_ok()
    } else if path.is_dir() {
        fs::remove_dir_all(path).is_ok()
    } else {
        false
    }
}

pub(crate) struct CacheHealReport {
    pub crates: Vec<String>,
    pub targets: Vec<String>,
    pub removed_count: usize,
}

pub(crate) fn heal_premerge_target_cache(
    wt_path: &Path,
    cwd: &Path,
    env: &Value,
    output: &str,
    command: &str,
    changed_paths: &[String],
) -> CacheHealReport {
    let crates = extract_affected_crates(output, command, wt_path, changed_paths);
    let targets = premerge_owned_target_dirs(wt_path, cwd, env);
    let mut removed_count = 0usize;

    for target_dir in &targets {
        for profile_dir in discover_profile_dirs(target_dir) {
            let mut profile_removed = 0usize;
            if !crates.is_empty() {
                for (subdir, allow_lib) in [
                    ("incremental", false),
                    ("deps", true),
                    (".fingerprint", false),
                ] {
                    let sub_path = profile_dir.join(subdir);
                    let Ok(rd) = fs::read_dir(&sub_path) else {
                        continue;
                    };
                    for ent in rd.flatten() {
                        let name = ent.file_name().to_string_lossy().into_owned();
                        if entry_matches_crates(&name, &crates, allow_lib)
                            && remove_path_any(&ent.path())
                        {
                            profile_removed += 1;
                        }
                    }
                }
            }
            // Fallback when no crate-specific entries matched: remove the profile's
            // incremental / deps / .fingerprint directories inside premerge's own target.
            if profile_removed == 0 {
                for subdir in ["incremental", "deps", ".fingerprint"] {
                    let sub_path = profile_dir.join(subdir);
                    if sub_path.is_dir() && !sub_path.is_symlink() && remove_path_any(&sub_path) {
                        profile_removed += 1;
                    }
                }
            }
            removed_count += profile_removed;
        }
    }

    CacheHealReport {
        crates,
        targets: targets
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        removed_count,
    }
}

struct PremergeLayersOutcome {
    ok: bool,
    aborted: bool,
    layers: Vec<Value>,
    ran_layers: Vec<String>,
    failed_layer: Option<String>,
    failed_code: Option<i32>,
}

#[allow(clippy::too_many_arguments)]
fn run_premerge_layers(
    run: &Path,
    run_id: &str,
    run_name: &str,
    repo_str: &str,
    source_head: &str,
    meta: &Value,
    repo_cfg: &Value,
    wt_path: &Path,
    source: &Path,
    changed_paths: &[String],
    log_path: &Path,
) -> Res<PremergeLayersOutcome> {
    if should_yield_to_apply(run, source) {
        return Ok(PremergeLayersOutcome {
            ok: false,
            aborted: true,
            layers: Vec::new(),
            ran_layers: Vec::new(),
            failed_layer: None,
            failed_code: None,
        });
    }
    let mut eval_meta = meta.clone();
    if eval_meta["acceptRules"].is_null() && repo_cfg["acceptRules"].is_array() {
        eval_meta["acceptRules"] = repo_cfg["acceptRules"].clone();
    }
    if s(&eval_meta, "accept").trim().is_empty() && !s(repo_cfg, "accept").trim().is_empty() {
        eval_meta["accept"] = repo_cfg["accept"].clone();
    }

    let plan = accept::plan(&eval_meta, changed_paths, true, None, &BTreeMap::new());
    let mut specs = Vec::new();
    specs.extend(plan.fast);
    specs.extend(plan.slow);
    if specs.is_empty() {
        return Ok(PremergeLayersOutcome {
            ok: true,
            aborted: false,
            layers: Vec::new(),
            ran_layers: Vec::new(),
            failed_layer: None,
            failed_code: Some(0),
        });
    }

    let cwd = premerge_workdir(source, meta, wt_path);
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| format!("open {}: {e}", log_path.display()))?;

    let mut layers = Vec::new();
    let mut ran_layers = Vec::new();
    let mut overall_ok = true;
    let mut failed_layer = None;
    let mut failed_code = None;

    for (idx, spec) in specs.iter().enumerate() {
        if should_yield_to_apply(run, source) {
            return Ok(PremergeLayersOutcome {
                ok: false,
                aborted: true,
                layers,
                ran_layers,
                failed_layer: None,
                failed_code: None,
            });
        }
        let _slot = if spec.lane {
            let label = format!(
                "premerge {} ({})",
                run.file_name().unwrap_or_default().to_string_lossy(),
                spec.name
            );
            match lane::acquire_shell_for_commands(&label, &[spec.command.as_str()], None, Some(run), |_, _| {}) {
                Ok(slot) => Some(slot),
                Err(_) if should_yield_to_apply(run, source) => {
                    return Ok(PremergeLayersOutcome {
                        ok: false,
                        aborted: true,
                        layers,
                        ran_layers,
                        failed_layer: None,
                        failed_code: None,
                    });
                }
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        if should_yield_to_apply(run, source) {
            return Ok(PremergeLayersOutcome {
                ok: false,
                aborted: true,
                layers,
                ran_layers,
                failed_layer: None,
                failed_code: None,
            });
        }
        let _ = writeln!(
            log,
            "=== [premerge:{}] {} ({}) ===\n$ {}",
            spec.tier.as_str(),
            spec.name,
            spec.reason,
            spec.command
        );
        let _ = log.flush();
        let t0 = Instant::now();
        let cleanup_dir = wt_path.parent().unwrap_or(wt_path);
        let attempt_start = log.metadata().map(|m| m.len()).unwrap_or(0);
        // No timeout for premerge acceptance; silent commands emit the existing stall_warning.
        let (mut code, _timed) = run_shell_isolated(
            (&spec.command, "premerge"),
            &cwd,
            run,
            cleanup_dir,
            &eval_meta["env"],
            None,
            &mut log,
            None,
            true,
        )
        .unwrap_or((1, false));
        let _ = writeln!(log, "\n[exit {code}]");
        let _ = log.flush();

        if code != 0 && !should_yield_to_apply(run, source) {
            let attempt_out = read_log_slice(log_path, attempt_start);
            if is_linker_cache_error(&attempt_out) {
                let report = heal_premerge_target_cache(
                    wt_path,
                    &cwd,
                    &eval_meta["env"],
                    &attempt_out,
                    &spec.command,
                    changed_paths,
                );
                let _ = writeln!(
                    log,
                    "=== [premerge:cache-heal] {} linker error detected; healed {} entries (crates: {}) and retrying once ===\n$ {}",
                    spec.name,
                    report.removed_count,
                    if report.crates.is_empty() {
                        "all-profile".to_string()
                    } else {
                        report.crates.join(", ")
                    },
                    spec.command
                );
                let _ = log.flush();
                events::emit(
                    run_id,
                    repo_str,
                    "premerge_cache_healed",
                    json!({
                        "name": run_name,
                        "baseHead": source_head,
                        "step": "accept",
                        "layer": spec.name,
                        "crates": report.crates,
                        "removed": report.removed_count,
                        "targets": report.targets,
                        "log": log_path.to_string_lossy(),
                    }),
                );
                let (retry_code, _) = run_shell_isolated(
                    (&spec.command, "premerge"),
                    &cwd,
                    run,
                    cleanup_dir,
                    &eval_meta["env"],
                    None,
                    &mut log,
                    None,
                    true,
                )
                .unwrap_or((1, false));
                code = retry_code;
                let _ = writeln!(log, "\n[exit {code} (after cache heal)]");
                let _ = log.flush();
            }
        }
        let secs = (t0.elapsed().as_secs_f64() * 100.0).round() / 100.0;

        if should_yield_to_apply(run, source) {
            return Ok(PremergeLayersOutcome {
                ok: false,
                aborted: true,
                layers,
                ran_layers,
                failed_layer: None,
                failed_code: None,
            });
        }

        let ok = code == 0;
        ran_layers.push(spec.name.clone());
        layers.push(json!({
            "name": spec.name,
            "tier": spec.tier.as_str(),
            "command": spec.command,
            "status": if ok { "passed" } else { "failed" },
            "ok": ok,
            "exitCode": code,
            "seconds": secs,
            "reason": spec.reason,
        }));
        if !ok {
            overall_ok = false;
            failed_layer = Some(spec.name.clone());
            failed_code = Some(code);
            for rem in &specs[idx + 1..] {
                layers.push(json!({
                    "name": rem.name,
                    "tier": rem.tier.as_str(),
                    "command": rem.command,
                    "status": "not_run",
                    "ok": false,
                    "reason": format!("前序层 {} 失败，未执行", spec.name),
                }));
            }
            break;
        }
    }

    for sk in &plan.skipped {
        layers.push(json!({
            "name": sk.name,
            "tier": sk.tier.as_str(),
            "command": sk.command,
            "status": "skipped",
            "ok": true,
            "reason": sk.reason,
        }));
    }

    Ok(PremergeLayersOutcome {
        ok: overall_ok,
        aborted: false,
        layers,
        ran_layers,
        failed_layer,
        failed_code,
    })
}

fn source_file_blob(source: &Path, head: &str, path: &str) -> String {
    if head.is_empty() || path.is_empty() {
        return "absent".into();
    }
    git_text(source, &["rev-parse", "-q", "--verify", &format!("{head}:{path}")])
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "absent".into())
}

fn conflict_source_blobs(
    source: &Path,
    head: &str,
    conflicts: &[String],
) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for p in conflicts {
        map.insert(p.clone(), source_file_blob(source, head, p));
    }
    map
}

fn same_conflict_signature(
    prev_doc: &Value,
    conf_list: &[String],
    blobs: &BTreeMap<String, String>,
) -> bool {
    if s(prev_doc, "status") != "conflict" || conf_list.is_empty() || blobs.is_empty() {
        return false;
    }
    let prev_confs = strings(&prev_doc["conflicts"]);
    if prev_confs != conf_list {
        return false;
    }
    let Some(prev_blobs) = prev_doc["conflictSourceBlobs"].as_object() else {
        return false;
    };
    if prev_blobs.len() != blobs.len() {
        return false;
    }
    blobs
        .iter()
        .all(|(k, v)| prev_blobs.get(k).and_then(Value::as_str) == Some(v.as_str()))
}

/// Check if the most recent `premerge_conflict` event for `run_id` (not superseded by a later
/// `premerge_ready`) already reported the exact same `(conflicts, conflictSourceBlobs)`.
fn last_premerge_conflict_matches(
    run_id: &str,
    conf_list: &[String],
    blobs: &BTreeMap<String, String>,
) -> bool {
    if run_id.is_empty() || conf_list.is_empty() || blobs.is_empty() {
        return false;
    }
    let ev_path = events::events_path();
    let Ok(f) = File::open(ev_path) else {
        return false;
    };
    let mut last_matched = false;
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.contains(run_id) {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if s(&ev, "run") != run_id {
            continue;
        }
        match s(&ev, "kind") {
            "premerge_ready" => last_matched = false,
            "premerge_conflict" => {
                let pl = &ev["payload"];
                let ev_confs = strings(&pl["conflicts"]);
                let ev_blobs = pl["conflictSourceBlobs"].as_object();
                last_matched = ev_confs == conf_list
                    && ev_blobs.is_some_and(|eb| {
                        eb.len() == blobs.len()
                            && blobs
                                .iter()
                                .all(|(k, v)| eb.get(k).and_then(Value::as_str) == Some(v.as_str()))
                    });
            }
            _ => {}
        }
    }
    last_matched
}

/// If `run` is already in `conflict` status and EVERY conflicted file still has the exact same
/// git blob in `source_head` (and the run's own `after` tree did not change), refresh
/// `premerge.json`'s `baseHead` without re-running 3-way merge or emitting another event.
fn conflict_unchanged_at_head(run: &Path, source_canon: &Path, source_head: &str) -> bool {
    let mut doc = json(premerge_path(run));
    if s(&doc, "status") != "conflict" || s(&doc, "baseHead") == source_head {
        return false;
    }
    let confs = strings(&doc["conflicts"]);
    let Some(prev_blobs) = doc["conflictSourceBlobs"].as_object() else {
        return false;
    };
    if confs.is_empty() || prev_blobs.len() != confs.len() {
        return false;
    }
    let prev_after = s(&doc, "afterTree");
    if !prev_after.is_empty() {
        let changes_doc = json(run.join("changes.json"));
        let cur_after = s(&changes_doc, "after");
        if !cur_after.is_empty() && cur_after != prev_after {
            return false;
        }
    }
    let all_same = confs.iter().all(|p| {
        prev_blobs.get(p).and_then(Value::as_str)
            == Some(source_file_blob(source_canon, source_head, p).as_str())
    });
    if all_same {
        doc["baseHead"] = json!(source_head);
        doc["updatedAt"] = json!(iso());
        let _ = write_json(premerge_path(run), &doc);
        index::upsert_run(run);
        return true;
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn process_one_run(
    run: &Path,
    source_canon: &Path,
    repo_str: &str,
    source_head: &str,
    base_tree: &str,
    wt_path: &Path,
    managed: &[String],
    repo_cfg: &Value,
) -> Res<bool> {
    if should_yield_to_apply(run, source_canon) {
        return Ok(false);
    }
    let prev_premerge_doc = json(premerge_path(run));
    let meta = json(run.join("meta.json"));
    let sum = json(run.join("summary.json"));
    let run_id = events::run_id_for(run, &meta);
    let run_name = s(&meta, "name").to_string();
    let started_at = iso();
    let t0 = Instant::now();
    let log_path = premerge_log_path(run);
    let log_str = log_path.to_string_lossy().into_owned();

    // Mark running atomically so `status` / TUI immediately display `预合并中`.
    let running_doc = json!({
        "status": "running",
        "baseHead": source_head,
        "acceptOk": false,
        "conflicts": [],
        "workerPid": std::process::id(),
        "log": log_str,
        "startedAt": started_at,
        "updatedAt": started_at,
    });
    if write_json(premerge_path(run), &running_doc).is_err() {
        return Ok(false);
    }
    index::upsert_run(run);

    // Optional test hooks: allow concurrency tests to pause/kill the worker deterministically
    // while `premerge.json` is in `running` state (still yielding promptly to `apply` / SIGTERM).
    if cfg!(debug_assertions) {
        if let Ok(gate_str) = std::env::var("DELEGATE_TEST_PREMERGE_GATE") {
            if !gate_str.is_empty() {
                let gate_path = Path::new(&gate_str);
                let limit = Instant::now() + std::time::Duration::from_secs(30);
                while !gate_path.exists() && Instant::now() < limit {
                    if should_yield_to_apply(run, source_canon) {
                        clear_running_marker_if_ours(run);
                        return Ok(false);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
        }
        if let Ok(delay_str) = std::env::var("DELEGATE_TEST_PREMERGE_DELAY") {
            if let Ok(secs) = seconds(&delay_str) {
                if secs > 0.0 {
                    let until = Instant::now() + std::time::Duration::from_secs_f64(secs);
                    while Instant::now() < until {
                        if should_yield_to_apply(run, source_canon) {
                            clear_running_marker_if_ours(run);
                            return Ok(false);
                        }
                        let rem = until.saturating_duration_since(Instant::now());
                        std::thread::sleep(rem.min(std::time::Duration::from_millis(40)));
                    }
                }
            }
        }
    }

    if should_yield_to_apply(run, source_canon) {
        clear_running_marker_if_ours(run);
        return Ok(false);
    }

    // Reset persistent integration worktree to `source_head`.
    if let Err(err) = clean_integration_worktree(wt_path, source_head, managed) {
        let _ = write(&log_path, format!("reset integration worktree failed: {err}\n"));
        return Err(err);
    }
    let _ = File::create(&log_path);

    if should_yield_to_apply(run, source_canon) {
        clear_running_marker_if_ours(run);
        return Ok(false);
    }

    // 3-way merge run's net diff onto `source_head` inside the integration worktree.
    let merge_res = match worktree::merge_run_into_dir(run, wt_path, source_canon) {
        Ok(res) => res,
        Err(err) => {
            let _ = clean_integration_worktree(wt_path, source_head, managed);
            let _ = write(&log_path, format!("premerge error: {err}\n"));
            if !run.join("meta.json").is_file() || run.join(".applied").exists() {
                return Ok(false);
            }
            if read_git_head(source_canon).as_deref() != Some(source_head) {
                return Ok(true); // HEAD moved; retry on new HEAD
            }
            let elapsed = (t0.elapsed().as_secs_f64() * 100.0).round() / 100.0;
            let now = iso();
            let doc = json!({
                "status": "failed",
                "baseHead": source_head,
                "acceptOk": false,
                "conflicts": [],
                "elapsedSeconds": elapsed,
                "log": log_str,
                "error": clip(&err, 400),
                "startedAt": started_at,
                "finishedAt": now,
                "updatedAt": now,
            });
            let _ = write_json(premerge_path(run), &doc);
            events::emit(
                &run_id,
                repo_str,
                "premerge_failed",
                json!({
                    "name": run_name,
                    "baseHead": source_head,
                    "error": clip(&err, 300),
                    "log": log_str,
                    "next": format!("delegate reply {run_id} \"解决预合并失败\""),
                }),
            );
            index::upsert_run(run);
            board::refresh_for_run(run);
            return Ok(false);
        }
    };

    if !merge_res.conflicts.is_empty() || !merge_res.marked_paths.is_empty() {
        let _ = clean_integration_worktree(wt_path, source_head, managed);
        let mut all_conf: BTreeSet<String> = BTreeSet::new();
        for p in &merge_res.conflicts {
            all_conf.insert(p.clone());
        }
        for p in &merge_res.marked_paths {
            all_conf.insert(p.clone());
        }
        let conf_list: Vec<String> = all_conf.into_iter().collect();
        let _ = write(
            &log_path,
            format!(
                "premerge conflict against baseHead {source_head}\nconflicts:\n{}\n",
                conf_list
                    .iter()
                    .map(|p| format!("  - {p}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        );
        if !run.join("meta.json").is_file() || run.join(".applied").exists() {
            return Ok(false);
        }
        if read_git_head(source_canon).as_deref() != Some(source_head) {
            return Ok(true);
        }
        let elapsed = (t0.elapsed().as_secs_f64() * 100.0).round() / 100.0;
        let now = iso();
        let conf_blobs = conflict_source_blobs(source_canon, source_head, &conf_list);
        let is_dup_conflict = same_conflict_signature(&prev_premerge_doc, &conf_list, &conf_blobs)
            || last_premerge_conflict_matches(&run_id, &conf_list, &conf_blobs);
        let doc = json!({
            "status": "conflict",
            "baseHead": source_head,
            "afterTree": merge_res.after_tree,
            "acceptOk": false,
            "conflicts": conf_list,
            "conflictSourceBlobs": conf_blobs,
            "elapsedSeconds": elapsed,
            "log": log_str,
            "error": format!("三方合并冲突：{}", conf_list.join("、")),
            "startedAt": started_at,
            "finishedAt": now,
            "updatedAt": now,
        });
        let _ = write_json(premerge_path(run), &doc);
        if !is_dup_conflict {
            events::emit(
                &run_id,
                repo_str,
                "premerge_conflict",
                json!({
                    "name": run_name,
                    "baseHead": source_head,
                    "conflicts": conf_list,
                    "conflictSourceBlobs": conf_blobs,
                    "log": log_str,
                    "next": format!("delegate reply --sync=agent {run_id} \"解决预合并冲突\""),
                }),
            );
        }
        index::upsert_run(run);
        board::refresh_for_run(run);
        return Ok(false);
    }

    let cleanup_dir = wt_path.parent().unwrap_or(wt_path);

    // Regenerate `generated.paths` if `generated.inputs` were touched.
    if merge_res.touches_generated_inputs && !merge_res.generate_command.is_empty() {
        if should_yield_to_apply(run, source_canon) {
            let _ = clean_integration_worktree(wt_path, source_head, managed);
            clear_running_marker_if_ours(run);
            return Ok(false);
        }
        let _slot = match lane::acquire_shell_for_commands(
            &format!("premerge-generate {run_id}"),
            &[merge_res.generate_command.as_str()],
            None,
            Some(run),
            |_, _| {},
        ) {
            Ok(slot) => slot,
            Err(_) if should_yield_to_apply(run, source_canon) => {
                let _ = clean_integration_worktree(wt_path, source_head, managed);
                clear_running_marker_if_ours(run);
                return Ok(false);
            }
            Err(e) => return Err(e),
        };
        if should_yield_to_apply(run, source_canon) {
            let _ = clean_integration_worktree(wt_path, source_head, managed);
            clear_running_marker_if_ours(run);
            return Ok(false);
        }
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| e.to_string())?;
        let _ = writeln!(log, "=== [premerge:generate] ===\n$ {}", merge_res.generate_command);
        let _ = log.flush();
        let gen_attempt_start = log.metadata().map(|m| m.len()).unwrap_or(0);
        let (mut code, _) = run_shell_isolated(
            (&merge_res.generate_command, "premerge"),
            wt_path,
            run,
            cleanup_dir,
            &meta["env"],
            None,
            &mut log,
            None,
            true,
        )
        .unwrap_or((1, false));
        let _ = writeln!(log, "\n[exit {code}]");
        let _ = log.flush();
        if code != 0 && !should_yield_to_apply(run, source_canon) {
            let gen_out = read_log_slice(&log_path, gen_attempt_start);
            if is_linker_cache_error(&gen_out) {
                let run_changed = worktree::run_files(run);
                let report = heal_premerge_target_cache(
                    wt_path,
                    wt_path,
                    &meta["env"],
                    &gen_out,
                    &merge_res.generate_command,
                    &run_changed,
                );
                let _ = writeln!(
                    log,
                    "=== [premerge:cache-heal] generate linker error detected; healed {} entries (crates: {}) and retrying once ===\n$ {}",
                    report.removed_count,
                    if report.crates.is_empty() {
                        "all-profile".to_string()
                    } else {
                        report.crates.join(", ")
                    },
                    merge_res.generate_command
                );
                let _ = log.flush();
                events::emit(
                    &run_id,
                    repo_str,
                    "premerge_cache_healed",
                    json!({
                        "name": run_name,
                        "baseHead": source_head,
                        "step": "generate",
                        "layer": "generate",
                        "crates": report.crates,
                        "removed": report.removed_count,
                        "targets": report.targets,
                        "log": log_str,
                    }),
                );
                let (retry_code, _) = run_shell_isolated(
                    (&merge_res.generate_command, "premerge"),
                    wt_path,
                    run,
                    cleanup_dir,
                    &meta["env"],
                    None,
                    &mut log,
                    None,
                    true,
                )
                .unwrap_or((1, false));
                code = retry_code;
                let _ = writeln!(log, "\n[exit {code} (after cache heal)]");
                let _ = log.flush();
            }
        }
        if should_yield_to_apply(run, source_canon) {
            let _ = clean_integration_worktree(wt_path, source_head, managed);
            clear_running_marker_if_ours(run);
            return Ok(false);
        }
        if code != 0 {
            let _ = clean_integration_worktree(wt_path, source_head, managed);
            if should_yield_to_apply(run, source_canon) {
                clear_running_marker_if_ours(run);
                return Ok(false);
            }
            if read_git_head(source_canon).as_deref() != Some(source_head) {
                return Ok(true);
            }
            let elapsed = (t0.elapsed().as_secs_f64() * 100.0).round() / 100.0;
            let now = iso();
            let doc = json!({
                "status": "failed",
                "baseHead": source_head,
                "acceptOk": false,
                "failedLayer": "generate",
                "exitCode": code,
                "conflicts": [],
                "elapsedSeconds": elapsed,
                "log": log_str,
                "error": format!("预合并生成命令失败（exit {code}）"),
                "startedAt": started_at,
                "finishedAt": now,
                "updatedAt": now,
            });
            let _ = write_json(premerge_path(run), &doc);
            events::emit(
                &run_id,
                repo_str,
                "premerge_failed",
                json!({
                    "name": run_name,
                    "baseHead": source_head,
                    "failedLayer": "generate",
                    "exitCode": code,
                    "log": log_str,
                    "next": format!("delegate reply {run_id} \"修复预合并生成失败\""),
                }),
            );
            index::upsert_run(run);
            board::refresh_for_run(run);
            return Ok(false);
        }
    }

    if should_yield_to_apply(run, source_canon) {
        clear_running_marker_if_ours(run);
        return Ok(false);
    }

    // Snapshot merged integration worktree.
    let mut excluded = changes::managed_paths(&meta);
    for m in managed {
        if !excluded.contains(m) {
            excluded.push(m.clone());
        }
    }
    let Some(merged_snap) =
        changes::repository_snapshot_excluding(wt_path, cleanup_dir, &excluded)
    else {
        return Err("failed to snapshot premerge integration worktree".into());
    };
    let merged_tree = s(&merged_snap, "tree").to_string();
    let snap_complete = b(&merged_snap, "complete");
    let changed_paths = diff_tree_paths(wt_path, base_tree, &merged_tree)?;

    // Run matched `acceptRules` layers (or `accept` if none), reusing the run's verified
    // acceptance when the integrated tree is bit-for-bit identical or HEAD has not moved.
    let (reuse_ok, reused_from_dirty_base) = can_reuse_run_acceptance(
        &sum,
        &meta,
        source_head,
        wt_path,
        &merged_tree,
        snap_complete,
        run,
        merge_res.touches_generated_inputs,
    );
    let (accept_ok, reused_accept, layers, ran_layers, failed_layer, failed_code) = if reuse_ok {
            let orig_ran = {
                let r = strings(&sum["accept"]["ranLayers"]);
                if !r.is_empty() {
                    r
                } else {
                    vec!["accept".to_string()]
                }
            };
            let orig_layers = sum["accept"]["layers"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let _ = write(
                &log_path,
                format!(
                    "[reused] mergedTree {merged_tree} matches run's verified accept.tree; reused layers: {}\n",
                    orig_ran.join(", ")
                ),
            );
            (true, true, orig_layers, orig_ran, None, Some(0))
        } else {
            let res = run_premerge_layers(
                run,
                &run_id,
                &run_name,
                repo_str,
                source_head,
                &meta,
                repo_cfg,
                wt_path,
                source_canon,
                &changed_paths,
                &log_path,
            )?;
            if res.aborted {
                clear_running_marker_if_ours(run);
                return Ok(false);
            }
            (res.ok, false, res.layers, res.ran_layers, res.failed_layer, res.failed_code)
        };

    // Check if acceptance mutated the integration worktree.
    let mut accept_ok = accept_ok;
    let mut failed_layer = failed_layer;
    let mut extra_error = String::new();
    if accept_ok && !reused_accept {
        if let Some(after_snap) =
            changes::repository_snapshot_excluding(wt_path, cleanup_dir, &excluded)
        {
            if s(&after_snap, "tree") != merged_tree {
                accept_ok = false;
                failed_layer = Some("tree-mutated".into());
                extra_error = "预合并验收修改了集成树".into();
            }
        }
    }

    // Final race checks: if run was applied/cleaned, apply holds apply.lock, or source HEAD moved while we worked, bail cleanly!
    if should_yield_to_apply(run, source_canon) {
        clear_running_marker_if_ours(run);
        return Ok(false);
    }
    if read_git_head(source_canon).as_deref() != Some(source_head) {
        return Ok(true); // HEAD moved -> loop will re-premerge on new HEAD
    }

    let elapsed = (t0.elapsed().as_secs_f64() * 100.0).round() / 100.0;
    let now = iso();

    if !accept_ok {
        let log_body = read(&log_path);
        let tail: String = log_body
            .trim()
            .chars()
            .rev()
            .take(800)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let err_msg = if !extra_error.is_empty() {
            extra_error
        } else if let Some(ref fl) = failed_layer {
            format!("预合并验收层 {fl} 失败（exit {}）", failed_code.unwrap_or(1))
        } else {
            format!("预合并验收失败（exit {}）", failed_code.unwrap_or(1))
        };
        let doc = json!({
            "status": "failed",
            "baseHead": source_head,
            "mergedTree": merged_tree,
            "changedPaths": changed_paths,
            "acceptOk": false,
            "reusedAccept": reused_accept,
            "layers": layers,
            "ranLayers": ran_layers,
            "failedLayer": failed_layer.clone().unwrap_or_default(),
            "exitCode": failed_code.unwrap_or(1),
            "conflicts": [],
            "elapsedSeconds": elapsed,
            "log": log_str,
            "error": err_msg,
            "startedAt": started_at,
            "finishedAt": now,
            "updatedAt": now,
        });
        let _ = write_json(premerge_path(run), &doc);
        events::emit(
            &run_id,
            repo_str,
            "premerge_failed",
            json!({
                "name": run_name,
                "baseHead": source_head,
                "failedLayer": failed_layer.unwrap_or_default(),
                "exitCode": failed_code.unwrap_or(1),
                "log": log_str,
                "tail": clip(&tail, 400),
                "error": err_msg,
                "next": format!("delegate reply {run_id} \"修复预合并验收失败\""),
            }),
        );
        index::upsert_run(run);
        board::refresh_for_run(run);
        return Ok(false);
    }

    // Success! Pin `refs/delegate/premerge/<run>` and atomically write `premerge.json`.
    let ref_name = refs::pin_premerge(run, &meta, &merged_tree).unwrap_or_default();
    let doc = json!({
        "status": "ready",
        "baseHead": source_head,
        "mergedTree": merged_tree,
        "ref": ref_name,
        "changedPaths": changed_paths,
        "acceptOk": true,
        "reusedAccept": reused_accept,
        "reusedFromDirtyBase": reused_from_dirty_base,
        "layers": layers,
        "ranLayers": ran_layers,
        "conflicts": [],
        "elapsedSeconds": elapsed,
        "log": log_str,
        "afterTree": merge_res.after_tree,
        "hasLarge": merge_res.has_large,
        "snapshotComplete": snap_complete,
        "startedAt": started_at,
        "finishedAt": now,
        "updatedAt": now,
    });
    if write_json(premerge_path(run), &doc).is_err() {
        return Ok(false);
    }
    events::emit(
        &run_id,
        repo_str,
        "premerge_ready",
        json!({
            "name": run_name,
            "baseHead": source_head,
            "baseShort": &source_head[..source_head.len().min(7)],
            "elapsedSeconds": elapsed,
            "reusedAccept": reused_accept,
            "ranLayers": ran_layers,
        }),
    );
    index::upsert_run(run);
    board::refresh_for_run(run);
    Ok(false)
}

/// `delegate _merge-worker [<repo-path>]` — single-instance per-repo background premerge worker.
pub fn worker_command(args: &[String]) -> Res<i32> {
    let source = if let Some(arg) = args.first() {
        PathBuf::from(arg)
    } else {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        git_top(&cwd).ok_or("_merge-worker requires a git repository")?
    };
    if !source.is_dir() {
        return Ok(0);
    }
    let source_canon = canonical_repo(&source);
    let repo_str = source_canon.to_string_lossy().into_owned();
    let pre_dir = state_premerge_dir();
    let _ = fs::create_dir_all(&pre_dir);
    let lock_path = worker_lock_path(&source_canon);
    let wake_path = worker_wake_path(&source_canon);
    let pid_path = worker_pid_path(&source_canon);

    lane::install_signals();

    // Lock-free Dekker/flag wakeup handoff:
    // If another `_merge-worker` holds the lock, record `.wake` and retry `LOCK_NB` once.
    // The holder checks `.wake` AFTER dropping the lock, so no concurrent delivery can be lost.
    loop {
        if lane::stopped() || !source_canon.is_dir() {
            return Ok(0);
        }
        let guard = match locks::try_acquire(locks::LockKind::Worker, &lock_path, true) {
            Ok(g) => g,
            Err(_) => {
                let _ = write(&wake_path, format!("{}\n", now_ns()));
                match locks::try_acquire(locks::LockKind::Worker, &lock_path, true) {
                    Ok(g) => g,
                    Err(_) => return Ok(0),
                }
            }
        };

        let _ = fs::remove_file(&wake_path);
        let _ = write(&pid_path, format!("{}\n", std::process::id()));

        let run_res = run_worker_passes(&source_canon, &repo_str);

        let _ = fs::remove_file(&pid_path);
        drop(guard);

        if let Err(err) = run_res {
            eprintln!("delegate _merge-worker ({repo_str}): {err}");
        }

        // Check if another caller left a wake marker before we dropped the lock.
        if !wake_path.exists() || lane::stopped() || !source_canon.is_dir() {
            break;
        }
    }

    Ok(0)
}

fn is_mainline_blocked_candidate(run: &Path, source_head: &str) -> bool {
    if run.join(".applied").exists() || run.join(".applied-cleaned").exists() {
        return false;
    }
    let sum = json(run.join("summary.json"));
    if s(&sum, "state") != "blocked_mainline" {
        return false;
    }
    let mb = &sum["mainlineBlock"];
    if !mb.is_object() || n(mb, "rerunCount") > 0 {
        return false;
    }
    let blocked_base = s(mb, "blockedBaseHead");
    let checked_head = s(mb, "checkedHead");
    !source_head.is_empty() && source_head != blocked_base && source_head != checked_head
}

/// Auto-sync a `blocked_mainline` run cleanly to `source_head` when any recorded blocking file
/// changed on mainline, then re-run ONLY the failed (and downstream `not_run`) acceptance layers
/// with zero LLM calls. Transitions to `delivered` on pass or `rejected` on failure / sync conflict.
fn process_mainline_blocked_run(
    run: &Path,
    source_canon: &Path,
    repo_str: &str,
    source_head: &str,
    repo_cfg: &Value,
) -> Res<bool> {
    if should_yield_to_apply(run, source_canon) {
        return Ok(false);
    }
    let mut meta = json(run.join("meta.json"));
    let mut sum = json(run.join("summary.json"));
    if !meta.is_object() || s(&sum, "state") != "blocked_mainline" {
        return Ok(false);
    }
    let mb = sum["mainlineBlock"].clone();
    if !mb.is_object() || n(&mb, "rerunCount") > 0 {
        return Ok(false);
    }
    if s(&mb, "blockedBaseHead") == source_head || s(&mb, "checkedHead") == source_head {
        return Ok(false);
    }

    let blocking_files = strings(&mb["blockingFiles"]);
    let blocking_blobs = mb["blockingBlobs"].as_object();
    if blocking_files.is_empty() {
        return Ok(false);
    }

    let mut changed_blocking = Vec::new();
    for path in &blocking_files {
        let old_blob = blocking_blobs
            .and_then(|m| m.get(path))
            .and_then(Value::as_str)
            .unwrap_or("");
        let new_blob = source_file_blob(source_canon, source_head, path);
        if new_blob != old_blob {
            changed_blocking.push(path.clone());
        }
    }

    if changed_blocking.is_empty() {
        sum["mainlineBlock"]["checkedHead"] = json!(source_head);
        let _ = write_json(run.join("summary.json"), &sum);
        index::upsert_run(run);
        return Ok(false);
    }

    let wt_path = PathBuf::from(s(&meta["worktree"], "path"));
    if !wt_path.is_dir() {
        return Ok(false);
    }

    let run_id = events::run_id_for(run, &meta);
    let run_name = s(&meta, "name").to_string();

    // Sync worktree cleanly to committed source HEAD (zero LLM calls, never pulls dirty edits).
    let sync_report = match worktree::sync_to_head(run, &meta) {
        Ok(rep) => rep,
        Err(err) => {
            let now = iso();
            sum["state"] = json!("rejected");
            sum["error"] = json!(format!("主干阻塞解除但自动同步失败：{err}"));
            sum["mainlineBlock"]["blocked"] = json!(false);
            sum["mainlineBlock"]["rerunCount"] = json!(1);
            sum["mainlineBlock"]["rerunHead"] = json!(source_head);
            sum["mainlineBlock"]["rerunAt"] = json!(now);
            sum["mainlineBlock"]["rerunOk"] = json!(false);
            sum["mainlineBlock"]["syncError"] = json!(err);
            crate::card::generate_and_save(run, &meta, &mut sum);
            write_json(run.join("summary.json"), &sum)?;
            events::emit(
                &run_id,
                repo_str,
                "accept_rerun",
                json!({
                    "name": run_name,
                    "ok": false,
                    "baseHead": source_head,
                    "blockingFiles": blocking_files,
                    "changedBlockingFiles": changed_blocking,
                    "syncError": err,
                }),
            );
            events::emit_run_finished(run, &meta, &sum);
            crate::history::record_finish(run);
            crate::work::on_run_finished(run, &meta, &sum);
            board::refresh_for_run(run);
            index::upsert_run(run);
            return Ok(false);
        }
    };

    if !sync_report.conflicts.is_empty() {
        let now = iso();
        let conf_str = sync_report.conflicts.join("、");
        sum["state"] = json!("rejected");
        sum["error"] = json!(format!("主干阻塞解除但自动同步冲突：{conf_str}"));
        sum["mainlineBlock"]["blocked"] = json!(false);
        sum["mainlineBlock"]["rerunCount"] = json!(1);
        sum["mainlineBlock"]["rerunHead"] = json!(source_head);
        sum["mainlineBlock"]["rerunAt"] = json!(now);
        sum["mainlineBlock"]["rerunOk"] = json!(false);
        sum["mainlineBlock"]["syncConflicts"] = json!(sync_report.conflicts);
        crate::card::generate_and_save(run, &meta, &mut sum);
        write_json(run.join("summary.json"), &sum)?;
        events::emit(
            &run_id,
            repo_str,
            "accept_rerun",
            json!({
                "name": run_name,
                "ok": false,
                "baseHead": source_head,
                "blockingFiles": blocking_files,
                "changedBlockingFiles": changed_blocking,
                "syncConflicts": sync_report.conflicts,
            }),
        );
        events::emit_run_finished(run, &meta, &sum);
        crate::history::record_finish(run);
        crate::work::on_run_finished(run, &meta, &sum);
        board::refresh_for_run(run);
        index::upsert_run(run);
        return Ok(false);
    }

    // Update meta netBase & re-record changes against synced worktree.
    meta["netBase"] = json!(source_head);
    meta["netBaseRebased"] = json!(true);
    meta["netBaseAdvanced"] = json!(true);
    if !sync_report.base_tree.is_empty() {
        meta["netBaseTree"] = json!(sync_report.base_tree);
    }
    let _ = write_json(run.join("meta.json"), &meta);
    let _ = changes::record(&mut meta, run);

    let mut eval_meta = meta.clone();
    if eval_meta["acceptRules"].is_null() && repo_cfg["acceptRules"].is_array() {
        eval_meta["acceptRules"] = repo_cfg["acceptRules"].clone();
    }
    if s(&eval_meta, "accept").trim().is_empty() && !s(repo_cfg, "accept").trim().is_empty() {
        eval_meta["accept"] = repo_cfg["accept"].clone();
    }

    let failed_layers = strings(&mb["failedLayers"]);
    let not_run_layers = strings(&mb["notRunLayers"]);
    let plan = accept::plan_failed_layers_only(
        &eval_meta,
        &sum["accept"],
        &failed_layers,
        &not_run_layers,
    );

    let holder = std::sync::atomic::AtomicI32::new(0);
    let (excluded, before_snap) = accept::snapshot_for_accept(&eval_meta, run);
    let fast_outcome = if !plan.fast.is_empty() {
        Some(accept::run_phase(
            &eval_meta,
            run,
            &plan.fast,
            false,
            &holder,
            None,
            None,
        ))
    } else {
        None
    };
    let fast_ok = fast_outcome.as_ref().is_none_or(|f| f.ok);
    let slow_outcome = if fast_ok && !plan.slow.is_empty() {
        Some(accept::run_phase(
            &eval_meta,
            run,
            &plan.slow,
            fast_outcome.is_some(),
            &holder,
            None,
            None,
        ))
    } else {
        None
    };
    let (_, after_snap) = accept::snapshot_for_accept(&eval_meta, run);
    let new_accept = accept::build_accept_summary(
        &eval_meta,
        run,
        &plan,
        fast_outcome.as_ref(),
        slow_outcome.as_ref(),
        false,
        excluded,
        before_snap.as_ref(),
        after_snap.as_ref(),
    );

    let ok = b(&new_accept, "ok");
    let now = iso();
    let ran_layers = strings(&new_accept["ranLayers"]);
    let accept_label = accept::human_accept_label(&new_accept, b(&new_accept, "infrastructureFailure"));

    sum["accept"] = new_accept.clone();
    sum["mainlineBlock"]["blocked"] = json!(!ok);
    sum["mainlineBlock"]["rerunCount"] = json!(1);
    sum["mainlineBlock"]["rerunHead"] = json!(source_head);
    sum["mainlineBlock"]["rerunAt"] = json!(now);
    sum["mainlineBlock"]["rerunOk"] = json!(ok);
    sum["mainlineBlock"]["ranLayers"] = json!(ran_layers);

    if ok {
        sum["state"] = json!("delivered");
        let _ = write(run.join("exit_code"), "0\n");
        worktree::capture_final_entries(&meta, run);
    } else {
        sum["state"] = json!("rejected");
    }
    crate::card::generate_and_save(run, &meta, &mut sum);
    write_json(run.join("summary.json"), &sum)?;

    events::emit(
        &run_id,
        repo_str,
        "accept_rerun",
        json!({
            "name": run_name,
            "ok": ok,
            "baseHead": source_head,
            "blockingFiles": blocking_files,
            "changedBlockingFiles": changed_blocking,
            "ranLayers": ran_layers,
            "exitCode": new_accept["exitCode"],
            "acceptLabel": accept_label,
        }),
    );
    events::emit_run_finished(run, &meta, &sum);
    crate::history::record_finish(run);
    crate::work::on_run_finished(run, &meta, &sum);
    board::refresh_for_run(run);
    index::upsert_run(run);
    if ok {
        crate::dag_queue::advance();
    }
    Ok(false)
}

fn run_worker_passes(source_canon: &Path, repo_str: &str) -> Res<()> {
    let mut consecutive_same_head = 0u32;
    let mut last_head = String::new();

    loop {
        if lane::stopped() || !source_canon.is_dir() {
            break;
        }
        let Ok(source_head) = git_text(source_canon, &["rev-parse", "--verify", "HEAD^{commit}"])
        else {
            break;
        };
        let Ok(base_tree) = git_text(
            source_canon,
            &["rev-parse", "--verify", &format!("{source_head}^{{tree}}")],
        ) else {
            break;
        };

        let cfg = config::load(Some(source_canon))?;
        let mut protect_rules = strings(&cfg.value["defaults"]["protect"]);
        if !protect_rules.is_empty() {
            let _ = launch::normalize_protect(&mut protect_rules);
        }
        let controller_patterns = strings(&cfg.value["controllerReviewPaths"]);
        let work_cfg = worktree::work_config(&cfg.value, &source_canon.join(".delegate.json"))?;
        let env_cfg = if cfg.value["env"].is_object() {
            cfg.value["env"].clone()
        } else {
            json!({})
        };

        let candidates = discover_runs_for_repo(source_canon);

        // Pass 0 (runs even when DELEGATE_PREMERGE=0): unblock `blocked_mainline` runs whose
        // blocking files changed on mainline HEAD via zero-LLM auto-sync & failed-layer rerun.
        let blocked_pending: Vec<PathBuf> = candidates
            .iter()
            .filter(|r| is_mainline_blocked_candidate(r, &source_head))
            .cloned()
            .collect();
        let mut unblocked_any = false;
        let mut head_changed = false;
        for run in &blocked_pending {
            if lane::stopped()
                || !source_canon.is_dir()
                || crate::source_watch::apply_lock_busy(source_canon)
            {
                break;
            }
            if read_git_head(source_canon).as_deref() != Some(source_head.as_str()) {
                head_changed = true;
                break;
            }
            match process_mainline_blocked_run(
                run,
                source_canon,
                repo_str,
                &source_head,
                &cfg.value,
            ) {
                Ok(true) => {
                    head_changed = true;
                    break;
                }
                Ok(false) => {
                    unblocked_any = true;
                }
                Err(err) => {
                    eprintln!(
                        "delegate _merge-worker mainline-rerun ({}): {err}",
                        run.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
            }
        }
        if head_changed || read_git_head(source_canon).as_deref() != Some(source_head.as_str()) {
            continue;
        }
        if disabled() {
            break;
        }

        // Refresh candidates if any mainline-blocked run just transitioned to `delivered`.
        let premerge_candidates = if unblocked_any {
            discover_runs_for_repo(source_canon)
        } else {
            candidates
        };

        let pending: Vec<PathBuf> = premerge_candidates
            .into_iter()
            .filter(|r| {
                let meta = json(r.join("meta.json"));
                let sum = json(r.join("summary.json"));
                if !is_eligible_strict(
                    r,
                    &meta,
                    &sum,
                    &cfg.value,
                    &protect_rules,
                    &controller_patterns,
                ) || !run_needs_premerge(r, &source_head)
                {
                    return false;
                }
                // Deduplicate unchanged conflicts: if every conflicted file still has the exact
                // same blob in `source_head`, bump `baseHead` silently without re-merging/re-emitting.
                !conflict_unchanged_at_head(r, source_canon, &source_head)
            })
            .collect();

        if pending.is_empty() {
            break;
        }

        if source_head == last_head {
            consecutive_same_head += 1;
            if consecutive_same_head > 3 {
                // Guard against pathological infinite loop on the exact same HEAD.
                break;
            }
        } else {
            last_head = source_head.clone();
            consecutive_same_head = 0;
        }

        let (wt_path, managed) =
            ensure_integration_worktree(source_canon, &source_head, &work_cfg, &env_cfg)?;

        for run in &pending {
            if lane::stopped()
                || !source_canon.is_dir()
                || crate::source_watch::apply_lock_busy(source_canon)
            {
                break;
            }
            if read_git_head(source_canon).as_deref() != Some(source_head.as_str()) {
                head_changed = true;
                break;
            }
            match process_one_run(
                run,
                source_canon,
                repo_str,
                &source_head,
                &base_tree,
                &wt_path,
                &managed,
                &cfg.value,
            ) {
                Ok(true) => {
                    head_changed = true;
                    break;
                }
                Ok(false) => {}
                Err(err) => {
                    eprintln!(
                        "delegate _merge-worker ({}): {err}",
                        run.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
            }
        }

        // If HEAD changed during the pass, immediately loop to re-premerge on the new HEAD.
        if head_changed || read_git_head(source_canon).as_deref() != Some(source_head.as_str()) {
            continue;
        }
        // Check once more if new runs arrived during the pass.
        let more = discover_runs_for_repo(source_canon).into_iter().any(|r| {
            let meta = json(r.join("meta.json"));
            let sum = json(r.join("summary.json"));
            is_eligible_strict(
                &r,
                &meta,
                &sum,
                &cfg.value,
                &protect_rules,
                &controller_patterns,
            ) && run_needs_premerge(&r, &source_head)
        });
        if !more {
            break;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_l0_covers_all_required_states() {
        assert_eq!(format_l0(&Value::Null), "快进 —");
        assert_eq!(
            format_l0(&json!({"status": "ready", "baseHead": "abcdef1234567890", "baseShort": "abcdef1"})),
            "快进 ✓（基于 abcdef1）"
        );
        assert_eq!(format_l0(&json!({"status": "stale"})), "快进 ✗ 主干已前进");
        assert_eq!(format_l0(&json!({"status": "running"})), "预合并中");
        assert_eq!(
            format_l0(&json!({"status": "conflict", "conflicts": ["src/main.rs"], "conflictCount": 1})),
            "预合并冲突：src/main.rs"
        );
        assert_eq!(
            format_l0(&json!({"status": "conflict", "conflicts": ["a.rs", "b.rs", "c.rs"], "conflictCount": 3})),
            "预合并冲突：a.rs 等 3 个"
        );
        assert_eq!(
            format_l0(&json!({"status": "failed", "failedLayer": "rust-fast"})),
            "预合并失败（rust-fast）"
        );
    }

    #[test]
    fn paths_overlap_catches_exact_and_parent_child_collisions() {
        assert!(paths_overlap("src/a.rs", "src/a.rs"));
        assert!(paths_overlap("src/a.rs", "src"));
        assert!(paths_overlap("src", "src/a.rs"));
        assert!(!paths_overlap("src/a.rs", "src/ab.rs"));
        assert!(!paths_overlap("src/a.rs", "tests/a.rs"));
    }

    #[test]
    fn linker_cache_error_detection_matches_undefined_symbol_and_linking_failed() {
        assert!(is_linker_cache_error(
            "mold: error: undefined symbol: core::ptr::drop_in_place<my_crate::Widget>\n"
        ));
        assert!(is_linker_cache_error(
            "error: linking with `cc` failed: exit status: 1\n"
        ));
        assert!(is_linker_cache_error(
            "error: linking with `mold` failed\n"
        ));
        assert!(!is_linker_cache_error(
            "error[E0425]: cannot find value `foo` in this scope\ntest result: FAILED.\n"
        ));
    }

    #[test]
    fn heal_premerge_target_cleans_only_premerge_target_and_never_touches_worktree() {
        let root = std::env::temp_dir().join(format!(
            "delegate-premerge-heal-test-{}-{}",
            std::process::id(),
            now_ns()
        ));
        let premerge_dir = root.join("premerge").join("repo-key");
        let wt_path = premerge_dir.join("worktree");
        let other_wt_target = root.join("worktrees").join("run-1").join("target");

        // Premerge own target with affected `xtask` + unrelated `serde` artifacts.
        let pm_debug = wt_path.join("target").join("debug");
        fs::create_dir_all(pm_debug.join("incremental").join("xtask-abcdef1234")).unwrap();
        fs::create_dir_all(pm_debug.join("incremental").join("serde-11112222")).unwrap();
        fs::create_dir_all(pm_debug.join("deps")).unwrap();
        fs::write(pm_debug.join("deps").join("xtask-abcdef1234"), b"bad").unwrap();
        fs::write(pm_debug.join("deps").join("libserde-11112222.rlib"), b"good").unwrap();
        fs::create_dir_all(pm_debug.join(".fingerprint").join("xtask-abcdef1234")).unwrap();
        fs::create_dir_all(pm_debug.join(".fingerprint").join("serde-11112222")).unwrap();

        // Separate worktree target that must NEVER be touched even if passed via CARGO_TARGET_DIR.
        let owt_debug = other_wt_target.join("debug");
        fs::create_dir_all(owt_debug.join("incremental").join("xtask-abcdef1234")).unwrap();

        let err_out = "mold: error: undefined symbol: drop_glue<xtask::State>\nerror: could not compile `xtask` (bin \"xtask\")\n";
        let env = json!({"CARGO_TARGET_DIR": other_wt_target.to_string_lossy()});
        let report = heal_premerge_target_cache(
            &wt_path,
            &wt_path,
            &env,
            err_out,
            "cargo run -p xtask -- gen",
            &[],
        );

        assert!(report.crates.iter().any(|c| c == "xtask"), "{:?}", report.crates);
        assert!(report.removed_count >= 3, "removed={}", report.removed_count);
        assert!(!pm_debug.join("incremental").join("xtask-abcdef1234").exists());
        assert!(!pm_debug.join("deps").join("xtask-abcdef1234").exists());
        assert!(!pm_debug.join(".fingerprint").join("xtask-abcdef1234").exists());
        // Unrelated dependency preserved.
        assert!(pm_debug.join("incremental").join("serde-11112222").exists());
        assert!(pm_debug.join("deps").join("libserde-11112222.rlib").exists());
        assert!(pm_debug.join(".fingerprint").join("serde-11112222").exists());
        // External worktree target strictly untouched!
        assert!(owt_debug.join("incremental").join("xtask-abcdef1234").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sort_premerge_candidates_prioritizes_blocked_downstream_then_fifo() {
        let mut cands = vec![
            (0usize, 100u64, "run-a".to_string(), PathBuf::from("/tmp/a")),
            (1usize, 300u64, "run-b".to_string(), PathBuf::from("/tmp/b")),
            (3usize, 500u64, "run-c".to_string(), PathBuf::from("/tmp/c")),
            (1usize, 200u64, "run-d".to_string(), PathBuf::from("/tmp/d")),
        ];
        sort_premerge_candidates(&mut cands);
        let order: Vec<&str> = cands.iter().map(|c| c.2.as_str()).collect();
        assert_eq!(order, vec!["run-c", "run-d", "run-b", "run-a"]);
    }
}
