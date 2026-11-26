//! Machine-wide append-only event stream (`<state_dir>/events.jsonl`), monotonic sequence
//! numbers, read cursors (`cursors.json`), rebuild from run metadata, `delegate events`,
//! and cursor-based `delegate wait --since SEQ`.
use crate::common::*;
use crate::runs;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Event kinds that require controller attention and trigger the Stop hook race lock
/// when unread past the controller's cursor.
pub const ALERT_KINDS: [&str; 12] = [
    "delivered",
    "question",
    "rejected",
    "failed",
    "stall_warning",
    "queue_paused",
    "needs_attention",
    "premerge_conflict",
    "premerge_failed",
    "work_ready",
    "work_defer_expired",
    "work_stale",
];

/// Whether an event record requires controller attention for cursor advancement & Stop hook.
pub fn is_alert_event(ev: &Value) -> bool {
    let kind = s(ev, "kind");
    ALERT_KINDS.contains(&kind)
        || (kind == "review_verdict" && s(&ev["payload"], "verdict") == "NEEDS_CONTROLLER")
}

pub fn events_path() -> PathBuf {
    state_dir().join("events.jsonl")
}

/// Resolve a canonical repository root path string for a run's `meta.json`.
pub fn repo_for_meta(meta: &Value) -> String {
    let src = crate::history::source_root_for_meta(meta);
    if !src.is_empty() {
        return fs::canonicalize(&src)
            .unwrap_or_else(|_| PathBuf::from(&src))
            .to_string_lossy()
            .into_owned();
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Some(top) = git_top(&cwd) {
        return fs::canonicalize(&top)
            .unwrap_or(top)
            .to_string_lossy()
            .into_owned();
    }
    String::new()
}

pub fn repo_for_run(run: &Path) -> String {
    let meta = json(run.join("meta.json"));
    if meta.is_object() {
        repo_for_meta(&meta)
    } else {
        String::new()
    }
}

pub fn run_id_for(run: &Path, meta: &Value) -> String {
    let id = s(meta, "run");
    if !id.is_empty() {
        id.to_string()
    } else {
        run.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }
}

fn last_seq_from_file(path: &Path) -> u64 {
    let Ok(mut f) = OpenOptions::new().read(true).open(path) else {
        return 0;
    };
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return 0;
    };
    if len == 0 {
        return 0;
    }
    let back = len.min(16 * 1024);
    let _ = f.seek(SeekFrom::End(-(back as i64)));
    let mut buf = String::new();
    let _ = io::Read::read_to_string(&mut f, &mut buf);
    for line in buf.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            if let Some(seq) = v["seq"].as_u64() {
                return seq;
            }
        }
    }
    0
}

fn current_max_seq_locked(events_file: &Path, seq_file: &Path) -> u64 {
    let from_seq = read(seq_file).trim().parse::<u64>().unwrap_or(0);
    let from_log = last_seq_from_file(events_file);
    from_seq.max(from_log)
}

/// Latest sequence number currently recorded in `<state>/events.jsonl` (0 if empty/missing).
pub fn latest_seq_in(state: &Path) -> u64 {
    let from_seq = read(state.join("events.seq"))
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    let from_log = last_seq_from_file(&state.join("events.jsonl"));
    from_seq.max(from_log)
}

/// Latest sequence number currently recorded in `events.jsonl` (0 if empty/missing).
pub fn latest_seq() -> u64 {
    latest_seq_in(&state_dir())
}

/// Build the standard `started` event payload from `meta.json`.
pub fn started_payload(meta: &Value, from_queue: bool) -> Value {
    let mut payload = json!({
        "name": s(meta, "name"),
        "mode": s(meta, "mode"),
        "tier": meta["tier"],
        "model": meta["model"],
        "parent": meta["parent"],
    });
    if !s(meta, "class").is_empty() {
        payload["class"] = meta["class"].clone();
    }
    if !s(meta, "thinking").is_empty() {
        payload["thinking"] = meta["thinking"].clone();
    }
    if !s(meta, "workId").is_empty() {
        payload["workId"] = meta["workId"].clone();
    }
    if from_queue {
        payload["fromQueue"] = json!(true);
    }
    payload
}

/// Append one event to `<state_dir>/events.jsonl` with a strictly monotonic `seq`.
/// Write failures only print a diagnostic to stderr and never fail or block a run.
pub fn emit(run: &str, repo: &str, kind: &str, payload: Value) -> Option<u64> {
    match emit_in(&state_dir(), run, repo, kind, payload) {
        Ok(seq) => Some(seq),
        Err(err) => {
            eprintln!("delegate: event stream write failed ({kind}): {err}");
            None
        }
    }
}

pub fn emit_in(state: &Path, run: &str, repo: &str, kind: &str, payload: Value) -> Res<u64> {
    fs::create_dir_all(state).map_err(|e| e.to_string())?;
    let _guard = crate::locks::acquire_state(state).map_err(|e| e.to_string())?;
    let events_file = state.join("events.jsonl");
    let seq_file = state.join("events.seq");
    let next_seq = current_max_seq_locked(&events_file, &seq_file).saturating_add(1);
    let record = json!({
        "seq": next_seq,
        "at": iso(),
        "run": run,
        "repo": repo,
        "kind": kind,
        "payload": if payload.is_object() { payload } else { json!({}) },
    });
    let line = format!("{record}\n");
    append_synced(&events_file, &line)?;
    let _ = atomic_write(&seq_file, format!("{next_seq}\n"));
    Ok(next_seq)
}

/// Emit the terminal run event for `finish_run` (`delivered` / `question` / `rejected` / `stopped` / `failed`).
pub fn emit_run_finished(run: &Path, meta: &Value, summary: &Value) {
    let run_id = run_id_for(run, meta);
    let repo = repo_for_meta(meta);
    let state = s(summary, "state");
    let name = s(meta, "name");
    let mode = s(meta, "mode");
    let question = b(summary, "question");

    let kind = if question {
        "question"
    } else if b(summary, "policyRefused") {
        "needs_attention"
    } else if state == "delivered" || state == "answered" {
        "delivered"
    } else if state == "stopped" {
        "stopped"
    } else if state == "blocked_mainline" {
        "accept_blocked_mainline"
    } else if state == "rejected" {
        "rejected"
    } else {
        "failed"
    };

    let mut payload = json!({
        "name": name,
        "state": state,
        "mode": mode,
    });
    if state == "blocked_mainline" && summary["mainlineBlock"].is_object() {
        let mb = &summary["mainlineBlock"];
        payload["blockingFiles"] = mb["blockingFiles"].clone();
        payload["failedLayers"] = mb["failedLayers"].clone();
        payload["baseHead"] = mb["blockedBaseHead"].clone();
        if !s(mb, "logFile").is_empty() {
            payload["logFile"] = mb["logFile"].clone();
        }
    }
    if b(summary, "policyRefused") {
        payload["policyRefused"] = json!(true);
        payload["reason"] = json!("policyRefused");
        payload["detail"] = json!("结论被模型服务拒绝");
    }
    let change_totals = if summary["pendingChanges"].is_object() {
        &summary["pendingChanges"]
    } else {
        &summary["changes"]
    };
    if change_totals["files"].as_u64().is_some() {
        payload["files"] = change_totals["files"].clone();
        payload["added"] = change_totals["added"].clone();
        payload["deleted"] = change_totals["deleted"].clone();
    }
    if summary["accept"].is_object() {
        payload["acceptOk"] = summary["accept"]["ok"].clone();
        payload["acceptLabel"] = json!(crate::accept::human_accept_label(
            &summary["accept"],
            b(summary, "infrastructureFailure"),
        ));
    }
    if summary["review"].is_object() {
        if !s(&summary["review"], "verdict").is_empty() {
            payload["reviewVerdict"] = summary["review"]["verdict"].clone();
        } else if b(&summary["review"], "cancelled") {
            payload["reviewVerdict"] = json!("cancelled");
        }
    }
    if summary["card"].is_object() {
        payload["card"] = summary["card"].clone();
    }
    if let Some(pm) = crate::premerge::status_for_run(run, meta, summary, None) {
        payload["premerge"] = pm;
    }
    if mode == "write" && kind == "delivered" {
        payload["next"] = json!(format!("delegate card {run_id}"));
    }
    if !s(summary, "error").is_empty() {
        payload["error"] = json!(clip(s(summary, "error"), 200));
    }
    if question {
        payload["question"] = json!(true);
    }
    emit(&run_id, &repo, kind, payload);
}

/// Read all events from `events.jsonl` with `seq > since`.
pub fn read_since(since: u64) -> Vec<Value> {
    read_since_in(&state_dir(), since)
}

pub fn read_since_in(state: &Path, since: u64) -> Vec<Value> {
    let path = state.join("events.jsonl");
    let Ok(f) = OpenOptions::new().read(true).open(path) else {
        return Vec::new();
    };
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if v["seq"].as_u64().is_some_and(|seq| seq > since) {
            out.push(v);
        }
    }
    out
}

/// Tail window returned by [`read_tail_in`].
#[derive(Debug, Clone, Default)]
pub struct EventsTail {
    pub events: Vec<Value>,
    /// True when `events.jsonl` contains valid events earlier than the returned window.
    pub has_earlier: bool,
    pub earliest_seq: Option<u64>,
    pub latest_seq: Option<u64>,
}

/// Read the last `limit` valid events from `<state>/events.jsonl` by reading growing chunks
/// from EOF instead of deserializing the entire log. When `limit == 0`, reads all events.
pub fn read_tail_in(state: &Path, limit: usize) -> EventsTail {
    if limit == 0 {
        let events = read_since_in(state, 0);
        let earliest_seq = events.first().and_then(|v| v["seq"].as_u64());
        let latest_seq = events.last().and_then(|v| v["seq"].as_u64());
        return EventsTail {
            events,
            has_earlier: false,
            earliest_seq,
            latest_seq,
        };
    }

    let path = state.join("events.jsonl");
    let Ok(mut f) = OpenOptions::new().read(true).open(&path) else {
        return EventsTail::default();
    };
    let Ok(meta) = f.metadata() else {
        return EventsTail::default();
    };
    let file_len = meta.len();
    if file_len == 0 {
        return EventsTail::default();
    }

    let mut window_bytes: u64 = 128 * 1024;
    loop {
        let start = file_len.saturating_sub(window_bytes);
        if f.seek(SeekFrom::Start(start)).is_err() {
            return EventsTail::default();
        }
        let mut buf = Vec::with_capacity((file_len - start) as usize);
        if f.read_to_end(&mut buf).is_err() {
            return EventsTail::default();
        }
        let slice = if start > 0 {
            match buf.iter().position(|&b| b == b'\n') {
                Some(nl) => &buf[nl + 1..],
                None => &[],
            }
        } else {
            &buf[..]
        };

        let text = String::from_utf8_lossy(slice);
        let mut parsed = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if v["seq"].as_u64().is_some_and(|seq| seq > 0) {
                parsed.push(v);
            }
        }

        if parsed.len() > limit || start == 0 {
            let has_earlier = start > 0 || parsed.len() > limit;
            if parsed.len() > limit {
                let drain_n = parsed.len() - limit;
                parsed.drain(0..drain_n);
            }
            let earliest_seq = parsed.first().and_then(|v| v["seq"].as_u64());
            let latest_seq = parsed.last().and_then(|v| v["seq"].as_u64());
            return EventsTail {
                events: parsed,
                has_earlier,
                earliest_seq,
                latest_seq,
            };
        }

        window_bytes = window_bytes.saturating_mul(4);
    }
}

fn load_cursors_doc(state: &Path) -> Value {
    let v = json(state.join("cursors.json"));
    if v.is_object() {
        v
    } else {
        json!({"default": {"seq": 0, "readRuns": {}}, "callers": {}})
    }
}

fn update_cursor_entry(entry: &mut Value, seq: u64) {
    if !entry.is_object() {
        *entry = json!({"seq": 0, "readRuns": {}});
    }
    let old = entry["seq"].as_u64().unwrap_or(0);
    if seq > old {
        entry["seq"] = json!(seq);
    }
    entry["updatedAt"] = json!(iso());
    // Trim readRuns entries already covered by seq.
    let cur = entry["seq"].as_u64().unwrap_or(0);
    if let Some(map) = entry["readRuns"].as_object_mut() {
        map.retain(|_, v| v.as_u64().unwrap_or(0) > cur);
    }
}

/// Record that the current caller (from `wait` / `run` / human `events`) has read up to `seq`.
pub fn record_cursor(seq: u64) {
    touch_caller_session();
    record_cursor_in(&state_dir(), caller().as_deref(), seq);
}

pub fn record_cursor_in(state: &Path, caller_id: Option<&str>, seq: u64) {
    if fs::create_dir_all(state).is_err() {
        return;
    }
    let Ok(_guard) = crate::locks::acquire_state(state) else {
        return;
    };
    let mut doc = load_cursors_doc(state);
    if let Some(id) = caller_id.filter(|x| !x.is_empty()) {
        if !doc["callers"].is_object() {
            doc["callers"] = json!({});
        }
        update_cursor_entry(&mut doc["callers"][id], seq);
    } else {
        update_cursor_entry(&mut doc["default"], seq);
    }
    let _ = write_json(state.join("cursors.json"), &doc);
}

/// Record that the current caller has read a specific run's output (via `result` or `wait`).
/// Marks `readRuns[run_id] = max_seq` and advances `seq` if all alert events up to `max_seq`
/// are now covered.
pub fn mark_run_read(run_id: &str) {
    mark_runs_read(&[run_id.to_string()]);
}

pub fn mark_runs_read(run_ids: &[String]) {
    if run_ids.is_empty() {
        return;
    }
    touch_caller_session();
    mark_runs_read_in(&state_dir(), caller().as_deref(), run_ids);
}

pub fn mark_runs_read_in(state: &Path, caller_id: Option<&str>, run_ids: &[String]) {
    if run_ids.is_empty() || fs::create_dir_all(state).is_err() {
        return;
    }
    let Ok(_guard) = crate::locks::acquire_state(state) else {
        return;
    };
    let mut doc = load_cursors_doc(state);
    let entry = if let Some(id) = caller_id.filter(|x| !x.is_empty()) {
        if !doc["callers"].is_object() {
            doc["callers"] = json!({});
        }
        &mut doc["callers"][id]
    } else {
        &mut doc["default"]
    };
    if !entry.is_object() {
        *entry = json!({"seq": 0, "readRuns": {}});
    }
    if !entry["readRuns"].is_object() {
        entry["readRuns"] = json!({});
    }
    let cur_seq = entry["seq"].as_u64().unwrap_or(0);
    let events = read_since_in(state, cur_seq);

    // Find max seq per requested run id.
    let target_set: HashSet<&str> = run_ids.iter().map(String::as_str).collect();
    let mut max_for_run: HashMap<String, u64> = HashMap::new();
    for ev in &events {
        let r = s(ev, "run");
        if target_set.contains(r) {
            let seq = ev["seq"].as_u64().unwrap_or(0);
            let e = max_for_run.entry(r.to_string()).or_insert(0);
            if seq > *e {
                *e = seq;
            }
        }
    }
    if let Some(map) = entry["readRuns"].as_object_mut() {
        for (rid, mseq) in &max_for_run {
            let old = map.get(rid).and_then(Value::as_u64).unwrap_or(0);
            if *mseq > old {
                map.insert(rid.clone(), json!(mseq));
            }
        }
    }

    // Advance `seq` contiguously across any events whose alert events are all covered by `readRuns`.
    let read_runs_snapshot: HashMap<String, u64> = entry["readRuns"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n)))
        .collect();
    let mut new_seq = cur_seq;
    for ev in &events {
        let seq = ev["seq"].as_u64().unwrap_or(0);
        if is_alert_event(ev) {
            let r = s(ev, "run");
            let covered = read_runs_snapshot
                .get(r)
                .is_some_and(|&read_up_to| seq <= read_up_to);
            if !covered {
                break;
            }
        }
        new_seq = seq;
    }
    update_cursor_entry(entry, new_seq);
    let _ = write_json(state.join("cursors.json"), &doc);
}

/// Discover all known run directories across the machine for `events --rebuild`.
fn discover_all_runs_for_rebuild(state: &Path, extra_runs_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut runs_dirs: Vec<PathBuf> = extra_runs_dirs.to_vec();
    let repos_doc = json(state.join("repos.json"));
    if let Some(map) = repos_doc["repos"].as_object() {
        for entry in map.values() {
            let r = s(entry, "runs");
            if !r.is_empty() {
                runs_dirs.push(PathBuf::from(r));
            }
        }
    }
    // Live slot files
    let mut individual_runs: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir(state) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|ext| ext == "slot") {
                let run_dir = PathBuf::from(read(&p).trim());
                if run_dir.join("meta.json").is_file() {
                    individual_runs.push(run_dir);
                }
            }
        }
    }

    let mut seen_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut found: Vec<PathBuf> = Vec::new();
    let mut push_run = |p: PathBuf| {
        if !p.join("meta.json").is_file() {
            return;
        }
        let key = fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if seen_dirs.insert(key) {
            found.push(p);
        }
    };

    for dir in runs_dirs {
        if !dir.is_dir() {
            continue;
        }
        if let Ok(entries) = fs::read_dir(&dir) {
            for e in entries.flatten() {
                push_run(e.path());
            }
        }
        let applied_dir = dir.join(".applied");
        if let Ok(entries) = fs::read_dir(&applied_dir) {
            for e in entries.flatten() {
                push_run(e.path());
            }
        }
    }
    for r in individual_runs {
        push_run(r);
    }
    found
}

/// Rebuild `<state_dir>/events.jsonl` deterministically from all run `meta.json` / `summary.json` files.
pub fn rebuild() -> Res<Value> {
    let mut extras = vec![runs_root()];
    if let Some(old) = legacy_runs_root() {
        extras.push(old);
    }
    rebuild_in(&state_dir(), &extras)
}

pub fn rebuild_in(state: &Path, extra_runs_dirs: &[PathBuf]) -> Res<Value> {
    fs::create_dir_all(state).map_err(|e| e.to_string())?;
    let runs = discover_all_runs_for_rebuild(state, extra_runs_dirs);

    // (timestamp_epoch, phase_order, run_id, at_iso, repo, kind, payload)
    let mut raw: Vec<(i64, u8, String, String, String, &'static str, Value)> = Vec::new();

    for run in &runs {
        let meta = json(run.join("meta.json"));
        if !meta.is_object() {
            continue;
        }
        let sum = json(run.join("summary.json"));
        let run_id = run_id_for(run, &meta);
        let repo = repo_for_meta(&meta);
        let name = s(&meta, "name").to_string();
        let mode = s(&meta, "mode").to_string();

        let started_at = {
            let at = s(&meta, "startedAt");
            if at.is_empty() {
                format_epoch(meta["startedEpoch"].as_i64().unwrap_or(0), false, "%Y-%m-%dT%H:%M:%SZ")
            } else {
                at.to_string()
            }
        };
        let started_ts = iso_epoch(&started_at).unwrap_or(0);

        let queued_at = s(&meta, "queuedAt");
        if !queued_at.is_empty() {
            let qts = iso_epoch(queued_at).unwrap_or(started_ts);
            let qrec = json(run.join("queue.json"));
            raw.push((
                qts,
                1,
                run_id.clone(),
                queued_at.to_string(),
                repo.clone(),
                "queued",
                json!({
                    "scope": "capacity",
                    "name": name,
                    "reason": s(&qrec, "reason"),
                }),
            ));
        }

        if !started_at.is_empty() {
            raw.push((
                started_ts,
                2,
                run_id.clone(),
                started_at.clone(),
                repo.clone(),
                "started",
                started_payload(&meta, false),
            ));
        }

        // Stall warning / stall record
        let sw = json(run.join("stall-warning.json"));
        let stall = if sw.is_object() {
            sw
        } else {
            json(run.join("stall.json"))
        };
        let mut rebuilt_stall = false;
        if stall.is_object() {
            rebuilt_stall = true;
            let at = if !s(&stall, "at").is_empty() {
                s(&stall, "at").to_string()
            } else if !s(&sum, "finishedAt").is_empty() {
                s(&sum, "finishedAt").to_string()
            } else {
                started_at.clone()
            };
            let ts = iso_epoch(&at).unwrap_or(started_ts);
            raw.push((
                ts,
                3,
                run_id.clone(),
                at,
                repo.clone(),
                "stall_warning",
                json!({
                    "name": name,
                    "stallKind": s(&stall, "kind"),
                    "cmd": stall["cmd"],
                    "seconds": stall["seconds"],
                    "elapsedSeconds": stall["elapsedSeconds"],
                    "lastLine": stall["lastLine"],
                    "detail": stall["detail"],
                }),
            ));
        }
        if !rebuilt_stall {
            for ev in crate::runs::events(run) {
                if s(&ev, "e") == "stall_warning" {
                    let at = if !s(&ev, "at").is_empty() {
                        s(&ev, "at").to_string()
                    } else {
                        started_at.clone()
                    };
                    let ts = iso_epoch(&at).unwrap_or(started_ts);
                    raw.push((
                        ts,
                        3,
                        run_id.clone(),
                        at,
                        repo.clone(),
                        "stall_warning",
                        json!({
                            "name": name,
                            "stallKind": s(&ev, "kind"),
                            "cmd": ev["cmd"],
                            "seconds": ev["seconds"],
                            "elapsedSeconds": ev["elapsedSeconds"],
                            "lastLine": ev["lastLine"],
                            "detail": ev["detail"],
                        }),
                    ));
                }
            }
        }

        if sum.is_object() {
            let fin_at = if !s(&sum, "finishedAt").is_empty() {
                s(&sum, "finishedAt").to_string()
            } else {
                started_at.clone()
            };
            let fin_ts = iso_epoch(&fin_at).unwrap_or(started_ts);

            if sum["accept"].is_object() {
                let acc = &sum["accept"];
                let mut acc_payload = json!({
                    "name": name,
                    "ok": b(acc, "ok"),
                    "exitCode": acc["exitCode"],
                    "command": clip(s(acc, "command"), 120),
                    "notExecuted": b(acc, "notExecuted"),
                });
                if b(acc, "tiered") {
                    acc_payload["tiered"] = json!(true);
                    acc_payload["failClosed"] = acc["failClosed"].clone();
                    acc_payload["ranLayers"] = acc["ranLayers"].clone();
                    acc_payload["skippedLayers"] = acc["skippedLayers"].clone();
                }
                if let Some(el) = acc["elapsedSeconds"].as_f64() {
                    acc_payload["elapsedSeconds"] = json!((el * 100.0).round() / 100.0);
                }
                if let Some(q) = acc["queuedSeconds"].as_f64() {
                    acc_payload["queuedSeconds"] = json!((q * 100.0).round() / 100.0);
                }
                if acc["layerSeconds"].is_object() {
                    acc_payload["layerSeconds"] = acc["layerSeconds"].clone();
                }
                raw.push((
                    fin_ts,
                    4,
                    run_id.clone(),
                    fin_at.clone(),
                    repo.clone(),
                    "accept",
                    acc_payload,
                ));
            }

            // Reconstruct 6.29 review events from review.json / summary.review.
            let rev = {
                let f = json(run.join("review.json"));
                if f.is_object() {
                    f
                } else {
                    sum["review"].clone()
                }
            };
            if rev.is_object() {
                if let Some(rounds) = rev["rounds"].as_array() {
                    for (idx, r) in rounds.iter().enumerate() {
                        let rnum = n(r, "round").max((idx + 1) as i64);
                        let r_start = if !s(r, "startedAt").is_empty() {
                            s(r, "startedAt").to_string()
                        } else {
                            fin_at.clone()
                        };
                        let r_end = if !s(r, "finishedAt").is_empty() {
                            s(r, "finishedAt").to_string()
                        } else {
                            r_start.clone()
                        };
                        let r_sts = iso_epoch(&r_start).unwrap_or(fin_ts);
                        let r_ets = iso_epoch(&r_end).unwrap_or(r_sts);
                        raw.push((
                            r_sts,
                            4,
                            run_id.clone(),
                            r_start,
                            repo.clone(),
                            "review_started",
                            json!({
                                "name": name,
                                "round": rnum,
                                "incremental": b(r, "incremental"),
                                "model": rev["model"],
                                "tier": rev["tier"],
                            }),
                        ));
                        let r_verdict = s(r, "verdict");
                        if !r_verdict.is_empty() {
                            let mut vp = json!({
                                "name": name,
                                "round": rnum,
                                "verdict": r_verdict,
                                "verdictFile": r["verdictFile"],
                            });
                            if !s(r, "upgradedBy").is_empty() {
                                vp["upgradedBy"] = r["upgradedBy"].clone();
                            }
                            if b(r, "formatError") {
                                vp["formatError"] = json!(true);
                            }
                            raw.push((
                                r_ets,
                                4,
                                run_id.clone(),
                                r_end.clone(),
                                repo.clone(),
                                "review_verdict",
                                vp,
                            ));
                            if r_verdict == "REWORK" {
                                raw.push((
                                    r_ets,
                                    4,
                                    run_id.clone(),
                                    r_end.clone(),
                                    repo.clone(),
                                    "rework_sent",
                                    json!({
                                        "name": name,
                                        "round": rnum,
                                        "reworkRounds": rnum,
                                        "verdictFile": r["verdictFile"],
                                    }),
                                ));
                                if rnum >= 3 {
                                    raw.push((
                                        r_ets,
                                        4,
                                        run_id.clone(),
                                        r_end.clone(),
                                        repo.clone(),
                                        "needs_attention",
                                        json!({
                                            "name": name,
                                            "round": rnum,
                                            "reworkRounds": rnum,
                                            "reason": "rework_rounds",
                                            "verdict": "REWORK",
                                            "detail": format!("审-改返工已达第 {rnum} 轮"),
                                        }),
                                    ));
                                }
                            } else if r_verdict == "NEEDS_CONTROLLER" {
                                raw.push((
                                    r_ets,
                                    4,
                                    run_id.clone(),
                                    r_end.clone(),
                                    repo.clone(),
                                    "needs_attention",
                                    json!({
                                        "name": name,
                                        "round": rnum,
                                        "reason": if s(r, "upgradedBy").is_empty() { "needs_controller" } else { s(r, "upgradedBy") },
                                        "verdict": "NEEDS_CONTROLLER",
                                        "detail": s(r, "parseError"),
                                    }),
                                ));
                            }
                        }
                    }
                } else if s(&rev, "verdict") == "NEEDS_CONTROLLER" {
                    let at = if !s(&rev, "finishedAt").is_empty() {
                        s(&rev, "finishedAt").to_string()
                    } else {
                        fin_at.clone()
                    };
                    let ts = iso_epoch(&at).unwrap_or(fin_ts);
                    raw.push((
                        ts,
                        4,
                        run_id.clone(),
                        at.clone(),
                        repo.clone(),
                        "review_verdict",
                        json!({
                            "name": name,
                            "round": n(&rev, "round"),
                            "verdict": "NEEDS_CONTROLLER",
                            "upgradedBy": rev["upgradedBy"],
                        }),
                    ));
                    raw.push((
                        ts,
                        4,
                        run_id.clone(),
                        at,
                        repo.clone(),
                        "needs_attention",
                        json!({
                            "name": name,
                            "round": n(&rev, "round"),
                            "reason": if s(&rev, "upgradedBy").is_empty() { "needs_controller" } else { s(&rev, "upgradedBy") },
                            "verdict": "NEEDS_CONTROLLER",
                        }),
                    ));
                }
            }

            let state_str = s(&sum, "state");
            let question = b(&sum, "question");
            let kind = if question {
                "question"
            } else if state_str == "delivered" || state_str == "answered" {
                "delivered"
            } else if state_str == "stopped" {
                "stopped"
            } else if state_str == "blocked_mainline" {
                "accept_blocked_mainline"
            } else if state_str == "rejected" {
                "rejected"
            } else {
                "failed"
            };
            let mut payload = json!({
                "name": name,
                "state": state_str,
                "mode": mode,
            });
            if !s(&sum, "error").is_empty() {
                payload["error"] = json!(clip(s(&sum, "error"), 200));
            }
            if question {
                payload["question"] = json!(true);
            }
            raw.push((
                fin_ts,
                5,
                run_id.clone(),
                fin_at.clone(),
                repo.clone(),
                kind,
                payload,
            ));
        }

        let applied = json(run.join(".applied"));
        let applied_at = if !s(&applied, "at").is_empty() {
            s(&applied, "at").to_string()
        } else {
            s(&meta, "appliedAt").to_string()
        };
        if !applied_at.is_empty() {
            let ats = iso_epoch(&applied_at).unwrap_or(started_ts);
            raw.push((
                ats,
                6,
                run_id.clone(),
                applied_at,
                repo.clone(),
                "applied",
                json!({
                    "name": name,
                    "head": s(&applied, "head"),
                    "appliedBy": if s(&applied, "appliedBy").is_empty() { "delegate" } else { s(&applied, "appliedBy") },
                }),
            ));
        }
    }

    raw.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });

    let _guard = crate::locks::acquire_state(state).map_err(|e| e.to_string())?;
    let events_file = state.join("events.jsonl");
    let seq_file = state.join("events.seq");
    let mut out_buf = String::new();
    let mut seq: u64 = 0;
    for (_, _, run_id, at, repo, kind, payload) in raw {
        seq += 1;
        let line = json!({
            "seq": seq,
            "at": at,
            "run": run_id,
            "repo": repo,
            "kind": kind,
            "payload": payload,
        });
        out_buf.push_str(&line.to_string());
        out_buf.push('\n');
    }
    atomic_write(&events_file, out_buf)?;
    atomic_write(&seq_file, format!("{seq}\n"))?;

    Ok(json!({
        "rebuilt": true,
        "runs": runs.len(),
        "events": seq,
        "cursor": seq,
    }))
}

fn short_repo_name(repo: &str) -> &str {
    if repo.is_empty() {
        return "-";
    }
    Path::new(repo)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(repo)
}

fn short_time(at: &str) -> &str {
    // ISO: 2026-11-23T09:05:01Z -> 09:05:01
    if at.len() >= 19 {
        &at[11..19]
    } else {
        at
    }
}

pub fn format_human(ev: &Value) -> String {
    let seq = ev["seq"].as_u64().unwrap_or(0);
    let time = short_time(s(ev, "at"));
    let repo = short_repo_name(s(ev, "repo"));
    let kind = s(ev, "kind");
    let run = s(ev, "run");
    let p = &ev["payload"];
    let name = s(p, "name");
    let display_run = if !name.is_empty() && !run.is_empty() {
        format!("{run} ({})", name.replace(['\n', '\r'], " "))
    } else if !run.is_empty() {
        run.to_string()
    } else if !s(p, "queueName").is_empty() {
        format!("queue:{}", s(p, "queueName"))
    } else if !s(p, "label").is_empty() {
        s(p, "label").to_string()
    } else {
        "-".to_string()
    };

    let detail = match kind {
        "started" => {
            let model = s(p, "model");
            let mode = s(p, "mode");
            if !model.is_empty() {
                format!("{mode} {model}")
            } else {
                mode.to_string()
            }
        }
        "queued" => {
            let scope = s(p, "scope");
            let reason = s(p, "reason");
            let label = s(p, "label");
            if scope == "dag" {
                let after = strings(&p["after"]).join(",");
                format!("DAG 队列 {} (after: {after})", s(p, "queueName"))
            } else if !reason.is_empty() {
                format!("{scope}: {reason}")
            } else if !label.is_empty() {
                format!("{scope}: {label}")
            } else {
                scope.to_string()
            }
        }
        "lane_acquired" => {
            let q = p["queuedSeconds"].as_f64().unwrap_or(0.0);
            format!("{} (等 {:.0}s)", s(p, "label"), q)
        }
        "lane_released" => {
            let h = p["heldSeconds"].as_f64().unwrap_or(0.0);
            let label = s(p, "label");
            if let Some(cpu) = crate::lane::format_cpu_summary(
                h,
                p["cpuUserSecs"].as_f64(),
                p["cpuSysSecs"].as_f64(),
            ) {
                format!("{label} ({cpu})")
            } else {
                format!("{label} (持 {h:.0}s)")
            }
        }
        "accept" => {
            let ok = if b(p, "ok") {
                "通过"
            } else if b(p, "notExecuted") {
                "未执行"
            } else {
                "失败"
            };
            format!("验收{ok}: {}", clip(s(p, "command"), 60))
        }
        "delivered" => {
            let st = s(p, "state");
            let mut segs = vec![if st == "answered" {
                "已答复".to_string()
            } else {
                "已交付".to_string()
            }];
            let acc_lbl = s(p, "acceptLabel");
            if !acc_lbl.is_empty() {
                segs.push(acc_lbl.to_string());
            } else if let Some(ok) = p["acceptOk"].as_bool() {
                segs.push(if ok { "验收通过" } else { "验收失败" }.into());
            }
            let rv = s(p, "reviewVerdict");
            if !rv.is_empty() {
                segs.push(format!("审查 {rv}"));
            }
            if let Some(files) = p["files"].as_u64() {
                if files > 0 {
                    if p["added"].as_u64().is_some() {
                        segs.push(format!(
                            "改 {files} 个文件 +{}/-{}",
                            n(p, "added"),
                            n(p, "deleted")
                        ));
                    } else {
                        segs.push(format!("改 {files} 个文件"));
                    }
                }
            }
            if p["card"].is_object() {
                let risk_str = crate::card::format_l0_risk(&p["card"]);
                if !risk_str.is_empty() {
                    segs.push(risk_str);
                }
                segs.push(crate::card::format_l0_fast_forward(
                    &p["card"],
                    p.get("premerge"),
                ));
            } else if p["premerge"].is_object() {
                segs.push(crate::premerge::format_l0(&p["premerge"]));
            } else if s(p, "mode") == "write" {
                segs.push("快进 —".into());
            }
            let next_cmd = s(p, "next");
            if !next_cmd.is_empty() {
                segs.push(format!("下一步：{next_cmd}"));
            }
            segs.join(" · ")
        }
        "question" => "需要主控决定".to_string(),
        "accept_blocked_mainline" => {
            let files = strings(&p["blockingFiles"]);
            let layers = strings(&p["failedLayers"]);
            let fpart = if files.is_empty() {
                "主干文件".to_string()
            } else {
                files.join("、")
            };
            if layers.is_empty() {
                format!("主干阻塞：{fpart}（主干修复后自动重验）")
            } else {
                format!("主干阻塞：{fpart}（层：{}；主干修复后自动重验）", layers.join("、"))
            }
        }
        "accept_rerun" => {
            let ok = b(p, "ok");
            let layers = strings(&p["ranLayers"]);
            let head = s(p, "baseHead");
            let short = &head[..head.len().min(7)];
            let lpart = if layers.is_empty() {
                String::new()
            } else {
                format!(" [{}]", layers.join("、"))
            };
            if ok {
                format!("主干阻塞解除，自动重验通过（基线 {short}{lpart}）")
            } else if !s(p, "syncError").is_empty() {
                format!("主干阻塞解除但自动同步失败：{}", clip(s(p, "syncError"), 80))
            } else if p["syncConflicts"].as_array().is_some_and(|a| !a.is_empty()) {
                format!(
                    "主干阻塞解除但自动同步冲突：{}",
                    strings(&p["syncConflicts"]).join("、")
                )
            } else {
                format!("自动重验仍失败（基线 {short}{lpart}），交主控处理")
            }
        }
        "rejected" | "failed" => {
            let err = s(p, "error");
            if err.is_empty() {
                s(p, "state").to_string()
            } else {
                clip(err, 80)
            }
        }
        "stopped" => "已停止".to_string(),
        "applied" => {
            let head = s(p, "head");
            if head.is_empty() {
                "已合入".to_string()
            } else {
                format!("已合入 {}", &head[..head.len().min(7)])
            }
        }
        "cleaned" => "已清理".to_string(),
        "stall_warning" => {
            let sk = s(p, "stallKind");
            let det = s(p, "detail");
            if !det.is_empty() {
                format!("⚠ {sk}: {det}")
            } else {
                format!("⚠ {sk}")
            }
        }
        "loop_review" => {
            let stop = b(p, "stop");
            format!("自检 stop={stop}: {}", clip(s(p, "reason"), 80))
        }
        "queue_dispatched" => {
            format!("自动续派 {} -> {}", s(p, "queueName"), s(p, "nextRun"))
        }
        "queue_paused" => {
            format!("续派暂停 {}: {}", s(p, "queueName"), clip(s(p, "reason"), 80))
        }
        "review_started" => {
            let round = n(p, "round").max(1);
            let inc = if b(p, "incremental") { "，增量 diff" } else { "" };
            let model = s(p, "model");
            if model.is_empty() {
                format!("开始审查（第 {round} 轮{inc}）")
            } else {
                format!("开始审查（第 {round} 轮{inc}，{model}）")
            }
        }
        "review_verdict" => {
            let verdict = s(p, "verdict");
            let round = n(p, "round");
            if verdict == crate::review::REVIEW_VERDICT_ENV_ERROR || b(p, "envError") {
                let err = s(p, "error");
                let prefix = if round > 0 {
                    format!("审查环境错误（第 {round} 轮）")
                } else {
                    "审查环境错误".to_string()
                };
                if err.is_empty() {
                    prefix
                } else {
                    format!("{prefix}：{}", clip(err, 80))
                }
            } else {
                let up = s(p, "upgradedBy");
                let mut msg = if round > 0 {
                    format!("审查判定 {verdict}（第 {round} 轮）")
                } else {
                    format!("审查判定 {verdict}")
                };
                if !up.is_empty() {
                    msg.push_str(&format!(" [升级:{up}]"));
                }
                msg
            }
        }
        "rework_sent" => {
            let round = n(p, "reworkRounds").max(n(p, "round")).max(1);
            format!("原样退回作者返工（第 {round} 轮）")
        }
        "needs_attention" => {
            let det = s(p, "detail");
            let reason = s(p, "reason");
            if !det.is_empty() {
                format!("⚠ 需要主控关注：{}", clip(det, 80))
            } else if !reason.is_empty() {
                format!("⚠ 需要主控关注（{reason}）")
            } else {
                "⚠ 需要主控关注".to_string()
            }
        }
        "merge_started" => {
            let cnt = n(p, "count");
            let base = s(p, "baseHead");
            let short = &base[..base.len().min(7)];
            if short.is_empty() {
                format!("开始串行合入（{cnt} 个 run）")
            } else {
                format!("开始串行合入（{cnt} 个 run，基线 {short}）")
            }
        }
        "merge_item" => {
            let src = s(p, "sourceRun");
            let st = s(p, "status");
            if st == "MERGED" {
                let commit = s(p, "commit");
                let short = &commit[..commit.len().min(7)];
                let tag = if b(p, "autoMerge") {
                    "可自主合入"
                } else {
                    "待主控点头"
                };
                format!("已合入 {src} → {short}（{tag}）")
            } else {
                let reason = s(p, "reason");
                format!("已跳过 {src}：{}", clip(reason, 80))
            }
        }
        "merge_done" => {
            let merged = n(p, "mergedCount");
            let skipped = n(p, "skippedCount");
            let auto = n(p, "autoMergeCount");
            let ok = if b(p, "mergeAcceptOk") {
                "mergeAccept✓"
            } else {
                "mergeAccept✗"
            };
            format!("合入完成：合入 {merged} · 跳过 {skipped} · 自主 {auto} · {ok}")
        }
        "premerge_ready" => {
            let short = {
                let bs = s(p, "baseShort");
                if !bs.is_empty() {
                    bs
                } else {
                    let bh = s(p, "baseHead");
                    &bh[..bh.len().min(7)]
                }
            };
            let secs = p["elapsedSeconds"].as_f64().unwrap_or(0.0);
            format!("快进 ✓（基于 {short}，{secs:.1}s）")
        }
        "premerge_conflict" => {
            let files = strings(&p["conflicts"]);
            let log = s(p, "log");
            let fpart = if files.is_empty() {
                "存在冲突".to_string()
            } else {
                files.join("、")
            };
            if log.is_empty() {
                format!("预合并冲突：{fpart}")
            } else {
                format!("预合并冲突：{fpart}（日志 {log}）")
            }
        }
        "premerge_cache_healed" => {
            let step = s(p, "step");
            let layer = s(p, "layer");
            let crates = strings(&p["crates"]);
            let removed = n(p, "removed");
            let scope = if !layer.is_empty() {
                layer
            } else if !step.is_empty() {
                step
            } else {
                "premerge"
            };
            if crates.is_empty() {
                format!("预合并缓存自愈（{scope}，清理 {removed} 项后重试）")
            } else {
                format!(
                    "预合并缓存自愈（{scope}，清理 {} 共 {removed} 项后重试）",
                    crates.join("、")
                )
            }
        }
        "adopted" => {
            let from = s(p, "fromCaller");
            let to = s(p, "toCaller");
            let rounds = strings(&p["rounds"]).len().max(1);
            let from_disp = if from.is_empty() { "(无)" } else { from };
            format!("已接手会话（{from_disp} → {to}，共 {rounds} 轮）")
        }
        "premerge_failed" => {
            let layer = s(p, "failedLayer");
            let log = s(p, "log");
            let err = s(p, "error");
            let head = if !layer.is_empty() {
                format!("预合并验收失败（{layer}）")
            } else if !err.is_empty() {
                format!("预合并失败：{}", clip(err, 80))
            } else {
                "预合并失败".to_string()
            };
            if log.is_empty() {
                head
            } else {
                format!("{head}（日志 {log}）")
            }
        }
        "radar_unchanged" => {
            let fp = s(p, "fingerprint");
            format!("侦察指纹未变化（{}，0 token）", &fp[..fp.len().min(15)])
        }
        "radar_done" => {
            let added = n(p, "added");
            let deduped = n(p, "deduped");
            let dropped = n(p, "droppedNoAnchor");
            let total = n(p, "total");
            format!("侦察完成：新增 {added} · 去重 {deduped} · 无锚丢弃 {dropped} · 共 {total} 条")
        }
        "work_ready" => {
            let wid = s(p, "workId");
            let brief = s(p, "brief");
            if brief.is_empty() {
                format!("工作项就绪 [{wid}]（卡片：delegate work show {wid}）")
            } else {
                format!("工作项就绪 [{wid}] · 说明 {brief}（卡片：delegate work show {wid}）")
            }
        }
        "work_dispatched" => {
            let wid = s(p, "workId");
            let next = s(p, "nextRun");
            if s(p, "reason") == "fill" {
                let cls = s(p, "class");
                let active = n(p, "activeWriteRuns");
                format!(
                    "工作项补位派发 [{wid}]（{cls} · 在跑 {active}）-> {next}（卡片：delegate work show {wid}）"
                )
            } else {
                format!("工作项自动派发 [{wid}] -> {next}（卡片：delegate work show {wid}）")
            }
        }
        "work_defer_expired" => {
            let wid = s(p, "workId");
            let reason = s(p, "reason");
            format!("暂缓到期 [{wid}]（{} · 卡片：delegate work show {wid}）", clip(reason, 60))
        }
        "work_stale" => {
            let wid = s(p, "workId");
            let reason = s(p, "reason");
            format!("工作项停滞 [{wid}]（{} · 卡片：delegate work show {wid}）", clip(reason, 60))
        }
        "work_migrated" => {
            let ids = strings(&p["workIds"]);
            let label = if !ids.is_empty() {
                ids.join(", ")
            } else {
                s(p, "workId").to_string()
            };
            format!("迁移去掉旧手动前置 [{label}]（已记入 followup）")
        }
        _ => String::new(),
    };

    if detail.is_empty() {
        format!("#{seq:<4} {time} [{repo}] {kind:<14} {display_run}")
    } else {
        format!("#{seq:<4} {time} [{repo}] {kind:<14} {display_run} · {detail}")
    }
}

/// `delegate events [--since SEQ] [--follow] [--rebuild] [--json]`
pub fn command(args: &[String]) -> Res<i32> {
    let mut since: u64 = 0;
    let mut since_given = false;
    let mut follow = false;
    let mut rebuild_flag = false;
    let mut json_mode = false;

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (key, inline) = if a.starts_with("--") {
            a.split_once('=')
                .map_or((a.as_str(), None), |(k, v)| (k, Some(v)))
        } else {
            (a.as_str(), None)
        };
        match key {
            "--since" => {
                let raw = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --since: expected a sequence number")?
                    }
                };
                since = raw
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --since sequence number: {raw}"))?;
                since_given = true;
            }
            "--follow" | "-f" => follow = true,
            "--rebuild" => rebuild_flag = true,
            "--json" => json_mode = true,
            other => {
                return Err(unrecognized_arg(
                    other,
                    &["--since", "--follow", "-f", "--rebuild", "--json"],
                    "events",
                ));
            }
        }
        i += 1;
    }

    if rebuild_flag {
        if follow {
            return Err("--rebuild and --follow cannot be combined".into());
        }
        crate::refs::backfill_all();
        crate::history::ensure_backfilled();
        let info = rebuild()?;
        crate::index::rebuild_all();
        if json_mode {
            println!("{info}");
        } else {
            println!(
                "已从 {} 个 run 重建 {} 条事件（当前游标：{}）",
                info["runs"], info["events"], info["cursor"]
            );
        }
        return Ok(0);
    }

    let mut cursor = since;
    let initial = read_since(cursor);
    let mut last_printed_seq: Option<u64> = None;
    let mut write_ok = true;
    {
        let mut out = io::stdout().lock();
        for ev in &initial {
            let line = if json_mode {
                ev.to_string()
            } else {
                format_human(ev)
            };
            if writeln!(out, "{line}").is_err() {
                write_ok = false;
                break;
            }
            if let Some(seq) = ev["seq"].as_u64() {
                cursor = cursor.max(seq);
                last_printed_seq = Some(last_printed_seq.map_or(seq, |prev| prev.max(seq)));
            }
        }
        if write_ok && out.flush().is_err() {
            write_ok = false;
        }
    }

    if !follow {
        let current = latest_seq().max(cursor);
        if !json_mode {
            if write_ok {
                if let Some(printed) = last_printed_seq {
                    record_cursor(printed);
                }
            }
            if initial.is_empty() && since_given {
                println!("无新事件（当前游标：{current}）");
            } else if !initial.is_empty() || since_given {
                let shown_cursor = last_printed_seq.unwrap_or(current);
                println!("游标：{shown_cursor}");
            }
        }
        return Ok(0);
    }

    // Follow mode: watch state directory with Linux `inotify` for instant event delivery.
    crate::lane::install_signals();
    let watcher = crate::inotify::StateWatcher::watch(&state_dir());
    let stdout = io::stdout();
    let _ = stdout.lock().flush();
    while !crate::lane::stopped() {
        let fresh = read_since(cursor);
        if !fresh.is_empty() {
            let mut out = stdout.lock();
            for ev in &fresh {
                if let Some(seq) = ev["seq"].as_u64() {
                    cursor = cursor.max(seq);
                }
                let line = if json_mode {
                    ev.to_string()
                } else {
                    format_human(ev)
                };
                if writeln!(out, "{line}").is_err() {
                    return Ok(0);
                }
            }
            let _ = out.flush();
            continue;
        }
        crate::inotify::wait_or_sleep(watcher.as_ref(), Duration::from_millis(500));
    }
    Ok(0)
}

/// Cursor-based wait: `delegate wait --since SEQ [runs...] [--max DURATION] [--json]`
pub fn wait_since(since: u64, run_filter: &[String], max: Option<f64>) -> Res<i32> {
    let mut resolved_runs: Vec<(String, PathBuf)> = Vec::new();
    for r in run_filter {
        let p = runs::resolve_head(r)?;
        let id = run_id_for(&p, &json(p.join("meta.json")));
        resolved_runs.push((id, p));
    }
    let resolved_ids: HashSet<String> = resolved_runs.iter().map(|(id, _)| id.clone()).collect();

    let matches_filter = |ev: &Value| -> bool {
        if resolved_ids.is_empty() {
            return true;
        }
        resolved_ids.contains(s(ev, "run"))
    };

    // Start inotify watch BEFORE initial read so events written concurrently are never missed.
    let watcher = crate::inotify::StateWatcher::watch(&state_dir());
    let fallback_poll = setting("POLL", "0.5")
        .parse::<f64>()
        .unwrap_or(0.5)
        .clamp(0.05, 1.0);
    let begin = Instant::now();
    let mut collected: Vec<Value>;

    loop {
        let all_after = read_since(since);
        collected = all_after
            .into_iter()
            .filter(&matches_filter)
            .collect();
        if !collected.is_empty() {
            // Brief coalesce window to pick up paired events written back-to-back (e.g. accept + delivered).
            crate::inotify::wait_or_sleep(watcher.as_ref(), Duration::from_millis(30));
            collected = read_since(since)
                .into_iter()
                .filter(&matches_filter)
                .collect();
            break;
        }
        if max.is_some_and(|m| begin.elapsed().as_secs_f64() >= m) {
            break;
        }
        if resolved_runs.is_empty() {
            let has_active = !crate::launch::machine_runs(&state_dir()).is_empty()
                || runs::all_runs().iter().any(|r| runs::active(&runs::state(r)))
                || crate::dag_queue::has_pending();
            if !has_active && max.is_none() {
                break;
            }
        } else {
            // Filtered mode: only check whether the target run(s) themselves are still active.
            let target_active = resolved_runs.iter().any(|(id, path)| {
                runs::active(&runs::state(path))
                    || crate::launch::machine_runs(&state_dir()).iter().any(|mr| {
                        mr == path || mr.file_name().is_some_and(|n| n == id.as_str())
                    })
            });
            if !target_active {
                break;
            }
        }
        let wait_s = match max {
            Some(m) => (m - begin.elapsed().as_secs_f64()).max(0.0).min(fallback_poll),
            None => fallback_poll,
        };
        if wait_s <= 0.0 {
            break;
        }
        crate::inotify::wait_or_sleep(watcher.as_ref(), Duration::from_secs_f64(wait_s));
    }

    let new_cursor = collected
        .iter()
        .filter_map(|e| e["seq"].as_u64())
        .max()
        .unwrap_or_else(|| latest_seq().max(since));

    // Record cursor when waiting unfiltered, or mark filtered runs read.
    if resolved_ids.is_empty() {
        record_cursor(new_cursor);
    } else {
        let ids: Vec<String> = collected
            .iter()
            .map(|e| s(e, "run").to_string())
            .filter(|r| !r.is_empty())
            .collect();
        mark_runs_read(&ids);
    }

    let timed_out = collected.is_empty() && max.is_some_and(|m| begin.elapsed().as_secs_f64() >= m);
    if crate::output::json_enabled() {
        for ev in &collected {
            println!("{ev}");
        }
        println!(
            "{}",
            json!({
                "cursor": new_cursor,
                "since": since,
                "count": collected.len(),
                "timedOut": timed_out,
            })
        );
    } else {
        for ev in &collected {
            println!("{}", format_human(ev));
        }
        if collected.is_empty() {
            println!("无新事件（游标：{new_cursor}）");
        } else {
            println!("游标：{new_cursor}");
            for hint in crate::work::hints_for_events(&collected) {
                println!("{hint}");
            }
        }
        let has_active = !crate::launch::machine_runs(&state_dir()).is_empty()
            || runs::all_runs().iter().any(|r| runs::active(&runs::state(r)));
        if collected.is_empty() && has_active {
            if let Some(eta_line) = crate::work::eta_hint_for_wait(&[], &[], None) {
                println!("{eta_line}");
            }
        }
        if has_active {
            println!("继续等：{} wait --since {new_cursor}", script().display());
        }
    }

    Ok(if timed_out { 75 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "delegate-events-{tag}-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn monotonic_seq_and_read_since() {
        let state = temp_dir("mono");
        let s1 = emit_in(&state, "r1", "/repo/a", "started", json!({"name": "a"})).unwrap();
        let s2 = emit_in(&state, "r1", "/repo/a", "delivered", json!({"name": "a"})).unwrap();
        let s3 = emit_in(&state, "r2", "/repo/b", "question", json!({"name": "b"})).unwrap();
        assert_eq!((s1, s2, s3), (1, 2, 3));

        let after_1 = read_since_in(&state, 1);
        assert_eq!(after_1.len(), 2);
        assert_eq!(after_1[0]["seq"], json!(2));
        assert_eq!(after_1[1]["seq"], json!(3));

        let after_3 = read_since_in(&state, 3);
        assert!(after_3.is_empty());

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn emit_failure_returns_err_without_panicking() {
        let state = temp_dir("fail");
        // Make events.jsonl a directory so append fails.
        fs::create_dir_all(state.join("events.jsonl")).unwrap();
        let res = emit_in(&state, "r1", "/repo", "started", json!({}));
        assert!(res.is_err());
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn mark_runs_read_advances_cursor_only_when_contiguous() {
        let state = temp_dir("cursor");
        let _ = emit_in(&state, "r1", "/repo", "started", json!({})).unwrap(); // 1
        let _ = emit_in(&state, "r1", "/repo", "delivered", json!({})).unwrap(); // 2
        let _ = emit_in(&state, "r2", "/repo", "delivered", json!({})).unwrap(); // 3

        // Reading r2 first must not skip r1 (seq 2).
        mark_runs_read_in(&state, Some("s1"), &["r2".to_string()]);
        let doc = load_cursors_doc(&state);
        assert_eq!(doc["callers"]["s1"]["seq"], json!(1));
        assert_eq!(doc["callers"]["s1"]["readRuns"]["r2"], json!(3));

        // Reading r1 now advances seq all the way to 3 and clears readRuns.
        mark_runs_read_in(&state, Some("s1"), &["r1".to_string()]);
        let doc = load_cursors_doc(&state);
        assert_eq!(doc["callers"]["s1"]["seq"], json!(3));
        assert!(doc["callers"]["s1"]["readRuns"]
            .as_object()
            .is_none_or(|m| m.is_empty()));

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn record_cursor_updates_named_caller_and_default_monotonically() {
        let state = temp_dir("record-cursor");
        record_cursor_in(&state, Some("sess-a"), 309);
        record_cursor_in(&state, Some("sess-a"), 994);
        record_cursor_in(&state, Some("sess-a"), 100); // must not regress
        record_cursor_in(&state, None, 42);
        record_cursor_in(&state, Some(""), 88); // empty caller maps to default

        let doc = load_cursors_doc(&state);
        assert_eq!(doc["callers"]["sess-a"]["seq"], json!(994));
        assert_eq!(doc["default"]["seq"], json!(88));

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn rebuild_reconstructs_events_from_run_metas() {
        let state = temp_dir("rebuild");
        let runs_dir = state.join("runs");
        let r1 = runs_dir.join("20261123-010000-alpha");
        fs::create_dir_all(&r1).unwrap();
        write_json(
            r1.join("meta.json"),
            &json!({
                "run": "20261123-010000-alpha",
                "name": "alpha",
                "mode": "write",
                "workdir": "/tmp/demo",
                "startedAt": "2026-11-23T01:00:00Z",
                "startedEpoch": 1795395600,
                "queuedAt": "2026-11-23T00:59:30Z",
            }),
        )
        .unwrap();
        write_json(
            r1.join("summary.json"),
            &json!({
                "state": "delivered",
                "finishedAt": "2026-11-23T01:05:00Z",
                "accept": {"ok": true, "exitCode": 0, "command": "cargo test"},
            }),
        )
        .unwrap();
        write_json(
            r1.join(".applied"),
            &json!({"at": "2026-11-23T01:06:00Z", "head": "abcdef1234567", "appliedBy": "delegate"}),
        )
        .unwrap();
        write_json(
            state.join("repos.json"),
            &json!({"repos": {runs_dir.to_string_lossy(): {"repo": "/tmp/demo", "runs": runs_dir}}}),
        )
        .unwrap();

        let info = rebuild_in(&state, &[]).unwrap();
        assert_eq!(info["rebuilt"], json!(true));
        let evs = read_since_in(&state, 0);
        let kinds: Vec<&str> = evs.iter().map(|e| s(e, "kind")).collect();
        assert_eq!(
            kinds,
            vec!["queued", "started", "accept", "delivered", "applied"]
        );

        let _ = fs::remove_dir_all(&state);
    }
}
