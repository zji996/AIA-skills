//! Atomic materialized index (`<state>/index.json`) and zero-lock, zero-subprocess
//! `delegate snapshot --json` command.
//!
//! Disk files remain the single source of truth: every cached run entry carries a cheap
//! stat-based `sig` (mtime + length of key files in the run dir + `work.jsonl`). Readers
//! validate `sig` lock-free; if an older CLI or external edit modified a run directory directly,
//! readers seamlessly fall back to reading that run from disk without losing freshness.

use crate::common::*;
use crate::runs;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u64 = 1;

pub fn index_path(state: &Path) -> PathBuf {
    state.join("index.json")
}

fn stat_token(path: &Path) -> String {
    match fs::metadata(path) {
        Ok(m) => format!("{}.{}:{}", m.mtime(), m.mtime_nsec(), m.len()),
        Err(_) => "-".to_string(),
    }
}

pub fn work_file_sig(state: &Path) -> String {
    stat_token(&state.join("work.jsonl"))
}

/// Cheap stat-only fingerprint of a run directory (zero file reads, zero git calls).
/// Deliberately excludes `work.jsonl` so unrelated work-table writes never invalidate
/// cached run entries; notes overlays are applied separately only when notes exist.
pub fn run_sig(run: &Path, _work_sig: &str) -> String {
    let mut parts = Vec::with_capacity(16);
    for name in [
        "meta.json",
        "summary.json",
        "exit_code",
        ".applied",
        ".applied-cleaned",
        ".delivered",
        ".truncated",
        ".sync-base",
        ".generate-pending",
        ".waiting",
        "review.json",
        "stall-warning.json",
        "missing-objects.json",
        "result.md",
        "pending-reply.json",
        "sign.json",
    ] {
        parts.push(stat_token(&run.join(name)));
    }
    parts.join("|")
}

fn run_key(run: &Path) -> String {
    fs::canonicalize(run)
        .unwrap_or_else(|_| run.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Strictly lock-free read of `<state>/index.json`. Returns an empty skeleton if missing/corrupt.
pub fn load_index(state: &Path) -> Value {
    let v = json(index_path(state));
    if v.is_object()
        && v["runs"].is_object()
        && v["schemaVersion"].as_u64() == Some(SCHEMA_VERSION)
    {
        v
    } else {
        json!({
            "schemaVersion": SCHEMA_VERSION,
            "updatedAt": "",
            "seq": 0,
            "runs": {}
        })
    }
}

/// Returns true if `<state>/index.json` is missing, corrupt, has a mismatched `schemaVersion`,
/// or contains pre-6.33 entries lacking the `meta` block. Only `_gc-worker` acts on this.
pub fn needs_rebuild(state: &Path) -> bool {
    let path = index_path(state);
    if !path.is_file() {
        return true;
    }
    let Ok(raw) = fs::read_to_string(&path) else {
        return true;
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return true;
    };
    if !v.is_object()
        || v["schemaVersion"].as_u64() != Some(SCHEMA_VERSION)
        || !v["runs"].is_object()
    {
        return true;
    }
    v["runs"]
        .as_object()
        .is_some_and(|m| m.values().any(|entry| !entry["meta"].is_object()))
}

/// Fast lock-free lookup of a run's `startedNs` from `index.json` (zero `meta.json` opens on hit).
pub fn cached_started_ns(run: &Path, index: &Value) -> Option<u64> {
    let raw = run.to_string_lossy();
    let entry = index["runs"]
        .get(raw.as_ref())
        .or_else(|| {
            let key = run_key(run);
            if key == raw {
                None
            } else {
                index["runs"].get(&key)
            }
        })?;
    entry["startedNs"].as_u64()
}

/// Fast lock-free, zero-side-effect `(status, meta)` lookup backed by `index.json`.
/// Uses cached entry for finished runs whose stat `sig` still matches; recomputes in-memory
/// for active runs, missing entries, or runs edited externally / by an older CLI.
pub fn status_and_meta_cached(run: &Path, index: &Value, work_sig: &str) -> (Value, Value) {
    let raw = run.to_string_lossy();
    let entry = index["runs"]
        .get(raw.as_ref())
        .or_else(|| {
            let key = run_key(run);
            if key == raw {
                None
            } else {
                index["runs"].get(&key)
            }
        });
    if let Some(entry) = entry {
        let cached_state = s(&entry["status"], "state");
        if !cached_state.is_empty()
            && !runs::active(cached_state)
            && entry["status"].is_object()
            && entry["meta"].is_object()
            && s(entry, "sig") == run_sig(run, work_sig)
        {
            return (entry["status"].clone(), entry["meta"].clone());
        }
    }
    (runs::status(run), runs::index_meta(run))
}

/// Fast lock-free, zero-side-effect status lookup backed by `index.json`.
pub fn status_cached(run: &Path, index: &Value, work_sig: &str) -> Value {
    let raw = run.to_string_lossy();
    let entry = index["runs"]
        .get(raw.as_ref())
        .or_else(|| {
            let key = run_key(run);
            if key == raw {
                None
            } else {
                index["runs"].get(&key)
            }
        });
    if let Some(entry) = entry {
        let cached_state = s(&entry["status"], "state");
        if !cached_state.is_empty()
            && !runs::active(cached_state)
            && entry["status"].is_object()
            && s(entry, "sig") == run_sig(run, work_sig)
        {
            return entry["status"].clone();
        }
    }
    runs::status(run)
}

fn repo_for_run_status(run: &Path, status: &Value) -> String {
    let meta = json(run.join("meta.json"));
    let src = s(&meta["worktree"], "source");
    let raw = if !src.is_empty() {
        src
    } else if !s(&meta, "top").is_empty() {
        s(&meta, "top")
    } else if !s(&meta, "workdir").is_empty() {
        s(&meta, "workdir")
    } else {
        s(status, "dir")
    };
    if raw.is_empty() {
        return String::new();
    }
    let p = Path::new(raw);
    fs::canonicalize(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Upsert a single run into `<state>/index.json` (computes status outside lock, then commits
/// under `<state>/.state.lock` in <1ms). Must only be called from write paths.
pub fn upsert_run(run: &Path) {
    if !run.join("meta.json").is_file() {
        return;
    }
    let state = state_dir();
    let meta = json(run.join("meta.json"));
    let started_ns = meta["startedNs"].as_u64().unwrap_or(0);
    let status = runs::status(run);
    let idx_meta = runs::index_meta(run);
    let key = run_key(run);
    let repo = repo_for_run_status(run, &status);
    let runs_root_str = run
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Ok(_guard) = crate::locks::acquire_state(&state) else {
        return;
    };
    let wsig = work_file_sig(&state);
    let sig = run_sig(run, &wsig);
    upsert_entry_locked(
        &state,
        &key,
        &repo,
        &runs_root_str,
        started_ns,
        &sig,
        status,
        idx_meta,
    );
}

/// Upsert a pre-built run status while caller ALREADY holds `<state>/.state.lock`.
/// Zero subprocesses, <1ms.
pub fn upsert_run_locked(state: &Path, run: &Path, repo: &str, status: Value) {
    let key = run_key(run);
    let started_ns = json(run.join("meta.json"))["startedNs"].as_u64().unwrap_or(0);
    let idx_meta = runs::index_meta(run);
    let runs_root_str = run
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let wsig = work_file_sig(state);
    let sig = run_sig(run, &wsig);
    upsert_entry_locked(
        state,
        &key,
        repo,
        &runs_root_str,
        started_ns,
        &sig,
        status,
        idx_meta,
    );
}

#[allow(clippy::too_many_arguments)]
fn upsert_entry_locked(
    state: &Path,
    key: &str,
    repo: &str,
    runs_root: &str,
    started_ns: u64,
    sig: &str,
    status: Value,
    idx_meta: Value,
) {
    let _ = fs::create_dir_all(state);
    let mut doc = load_index(state);
    if !doc["runs"].is_object() {
        doc["runs"] = json!({});
    }
    doc["schemaVersion"] = json!(SCHEMA_VERSION);
    doc["updatedAt"] = json!(iso());
    doc["seq"] = json!(crate::events::latest_seq());
    if let Some(map) = doc["runs"].as_object_mut() {
        // Prune obviously deleted run directories opportunistically (cheap stat).
        map.retain(|k, _| k == key || Path::new(k).join("meta.json").is_file());
        map.insert(
            key.to_string(),
            json!({
                "sig": sig,
                "repo": repo,
                "runsRoot": runs_root,
                "startedNs": started_ns,
                "status": status,
                "meta": idx_meta,
            }),
        );
    }
    let _ = write_json(index_path(state), &doc);
}

/// Remove a cleaned run from `<state>/index.json` under `<state>/.state.lock`.
pub fn remove_run(run: &Path) {
    let state = state_dir();
    let key = run_key(run);
    let raw_key = run.to_string_lossy().into_owned();
    let Ok(_guard) = crate::locks::acquire_state(&state) else {
        return;
    };
    let mut doc = load_index(&state);
    doc["schemaVersion"] = json!(SCHEMA_VERSION);
    doc["updatedAt"] = json!(iso());
    doc["seq"] = json!(crate::events::latest_seq());
    if let Some(map) = doc["runs"].as_object_mut() {
        map.remove(&key);
        map.remove(&raw_key);
    }
    let _ = write_json(index_path(&state), &doc);
}

/// Rebuild `<state>/index.json` from all known repositories and runs.
pub fn rebuild_all() {
    let state = state_dir();
    let wsig = work_file_sig(&state);
    let repos = discover_snapshot_repos(&state, &[], None);
    let mut entries = serde_json::Map::new();
    for (repo_path, runs_dir) in repos {
        let repo_str = repo_path.to_string_lossy().into_owned();
        let runs_root_str = runs_dir.to_string_lossy().into_owned();
        for run in runs::all_runs_in(&runs_dir) {
            let key = run_key(&run);
            let started_ns = json(run.join("meta.json"))["startedNs"].as_u64().unwrap_or(0);
            let st = runs::status(&run);
            let idx_meta = runs::index_meta(&run);
            let sig = run_sig(&run, &wsig);
            entries.insert(
                key,
                json!({
                    "sig": sig,
                    "repo": repo_str,
                    "runsRoot": runs_root_str,
                    "startedNs": started_ns,
                    "status": st,
                    "meta": idx_meta,
                }),
            );
        }
    }
    let Ok(_guard) = crate::locks::acquire_state(&state) else {
        return;
    };
    let doc = json!({
        "schemaVersion": SCHEMA_VERSION,
        "updatedAt": iso(),
        "seq": crate::events::latest_seq(),
        "runs": Value::Object(entries),
    });
    let _ = write_json(index_path(&state), &doc);
}

fn repo_from_runs_dir(runs_dir: &Path) -> Option<PathBuf> {
    let comps: Vec<&str> = runs_dir
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    if comps.len() >= 4 {
        let tail = &comps[comps.len() - 3..];
        if tail == [".local", "run", "delegate"] || tail == [".local", "run", "pi"] {
            return runs_dir
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .map(Path::to_path_buf);
        }
    }
    None
}

/// Discover `(repo_root, runs_dir)` pairs lock-free and side-effect-free.
pub fn discover_snapshot_repos(
    state: &Path,
    explicit_repos: &[PathBuf],
    _root: Option<&Path>,
) -> Vec<(PathBuf, PathBuf)> {
    let mut out: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut seen_runs: HashSet<PathBuf> = HashSet::new();

    let mut push_pair = |repo: PathBuf, runs_dir: PathBuf| {
        let canon_runs = fs::canonicalize(&runs_dir).unwrap_or(runs_dir);
        if seen_runs.insert(canon_runs.clone()) {
            let canon_repo = fs::canonicalize(&repo).unwrap_or(repo);
            out.push((canon_repo, canon_runs));
        }
    };

    if !explicit_repos.is_empty() {
        for repo in explicit_repos {
            let runs_dir = if !setting("RUNS", "").is_empty() && explicit_repos.len() == 1 {
                PathBuf::from(setting("RUNS", ""))
            } else {
                repo.join(".local/run/delegate")
            };
            push_pair(repo.clone(), runs_dir);
        }
        return out;
    }

    // 1. Current repo / DELEGATE_RUNS
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let cur_runs = runs_root();
    let cur_repo = git_top_fast(&cwd)
        .or_else(|| repo_from_runs_dir(&cur_runs))
        .unwrap_or_else(|| cwd.clone());
    if cur_runs.is_dir() {
        push_pair(cur_repo, cur_runs);
    }

    // 2. Machine repo registry (`state/repos.json`)
    let repos_doc = json(state.join("repos.json"));
    if let Some(map) = repos_doc["repos"].as_object() {
        for entry in map.values() {
            let runs_str = s(entry, "runs");
            if runs_str.is_empty() {
                continue;
            }
            let rdir = PathBuf::from(runs_str);
            if !rdir.is_dir() {
                continue;
            }
            let repo_str = s(entry, "repo");
            let repo_p = if !repo_str.is_empty() {
                PathBuf::from(repo_str)
            } else {
                repo_from_runs_dir(&rdir).unwrap_or_else(|| rdir.clone())
            };
            push_pair(repo_p, rdir);
        }
    }

    // 3. Live slot files under state_dir
    if let Ok(rd) = fs::read_dir(state) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("slot") {
                continue;
            }
            let raw = read(&p);
            let run_dir = PathBuf::from(raw.trim());
            if !run_dir.join("meta.json").is_file() {
                continue;
            }
            if let Some(parent) = run_dir.parent() {
                let meta = json(run_dir.join("meta.json"));
                let repo_p = repo_from_runs_dir(parent)
                    .or_else(|| {
                        let src = s(&meta["worktree"], "source");
                        let raw_r = if !src.is_empty() {
                            src
                        } else if !s(&meta, "top").is_empty() {
                            s(&meta, "top")
                        } else {
                            s(&meta, "workdir")
                        };
                        (!raw_r.is_empty()).then(|| PathBuf::from(raw_r))
                    })
                    .unwrap_or_else(|| parent.to_path_buf());
                push_pair(repo_p, parent.to_path_buf());
            }
        }
    }

    out
}

/// `delegate snapshot --json [--repo <PATH>...] [--root <DIR>] [--events-limit <N>]`
/// Zero locks, zero disk writes, zero child processes.
pub fn snapshot_command(args: &[String]) -> Res<i32> {
    let mut explicit_repos: Vec<PathBuf> = Vec::new();
    let mut root: Option<PathBuf> = None;
    let mut events_limit: usize = 200;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => {}
            "--repo" => {
                i += 1;
                let v = args.get(i).ok_or("argument --repo: expected a path")?;
                explicit_repos.push(PathBuf::from(v));
            }
            "--root" => {
                i += 1;
                let v = args.get(i).ok_or("argument --root: expected a path")?;
                root = Some(PathBuf::from(v));
            }
            "--events-limit" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or("argument --events-limit: expected a number")?;
                events_limit = v
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --events-limit: {v}"))?;
            }
            other => return Err(format!("unrecognized arguments: {other}")),
        }
        i += 1;
    }

    let state = state_dir();
    let index = load_index(&state);
    let wsig = work_file_sig(&state);
    let repos = discover_snapshot_repos(&state, &explicit_repos, root.as_deref());

    let mut repo_groups = Vec::new();
    let mut all_seen_runs: BTreeMap<String, Value> = BTreeMap::new();
    for (repo_path, runs_dir) in repos {
        let run_dirs = runs::all_runs_in_with_index(&runs_dir, &index);
        let mut run_statuses = Vec::with_capacity(run_dirs.len());
        for run in &run_dirs {
            let st = status_cached(run, &index, &wsig);
            let rid = s(&st, "run").to_string();
            if !rid.is_empty() {
                all_seen_runs.insert(rid, st.clone());
            }
            run_statuses.push(st);
        }
        let cleaned_notes: Vec<Value> = crate::notes::cleaned_pending_chains(&run_dirs)
            .into_iter()
            .map(|c| crate::output::cleaned_note_json(&c))
            .collect();
        repo_groups.push(json!({
            "path": repo_path.to_string_lossy(),
            "runsDir": runs_dir.to_string_lossy(),
            "runs": run_statuses,
            "cleanedNotes": cleaned_notes,
        }));
    }

    let mut events_all = crate::events::read_since_in(&state, 0);
    if events_limit > 0 && events_all.len() > events_limit {
        let drain_n = events_all.len() - events_limit;
        events_all.drain(0..drain_n);
    }
    let seq = crate::events::latest_seq().max(
        events_all
            .last()
            .and_then(|e| e["seq"].as_u64())
            .unwrap_or(0),
    );
    let work_items = crate::work::load_readonly_json(&state);
    let lane = crate::lane::status_json_readonly().unwrap_or_else(|_| json!({}));
    let dag_queue = crate::dag_queue::load_items(&state);
    let merges = crate::merge::load_merge_reports(None);
    let candidates = crate::radar::load_all_candidates(None);

    let payload = json!({
        "schemaVersion": SCHEMA_VERSION,
        "generatedAt": iso(),
        "seq": seq,
        "repos": repo_groups,
        "work": work_items,
        "events": events_all,
        "lane": lane,
        "dagQueue": dag_queue,
        "merges": merges,
        "candidates": candidates,
    });
    println!("{payload}");
    Ok(0)
}
