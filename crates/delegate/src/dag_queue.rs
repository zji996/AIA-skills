//! Deterministic DAG continuation queue (`delegate queue add|list|remove|cancel`).
//!
//! When an upstream run delivers cleanly (and passes acceptance, plus review PASS when 6.29
//! review metadata is present), ready downstream steps are dispatched automatically with
//! zero LLM calls. If an upstream fails, asks a question, or requires controller attention,
//! the step pauses and emits a `queue_paused` event to `<state_dir>/events.jsonl`.
use crate::common::*;
use crate::{events, launch, runs};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn queue_path(state: &Path) -> PathBuf {
    state.join("dag-queue.json")
}

pub(crate) fn load_items(state: &Path) -> Vec<Value> {
    let doc = json(queue_path(state));
    doc["items"].as_array().cloned().unwrap_or_default()
}

fn save_items(state: &Path, items: &[Value]) -> Res<()> {
    fs::create_dir_all(state).map_err(|e| e.to_string())?;
    write_json(queue_path(state), &json!({"items": items}))
}

pub fn has_pending() -> bool {
    load_items(&state_dir())
        .iter()
        .any(|item| s(item, "status") == "pending")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamVerdict {
    Waiting,
    Satisfied,
    Paused {
        upstream_run: String,
        reason: String,
        state: String,
    },
}

/// Evaluate whether a finished run satisfies a DAG continuation dependency.
/// Checks:
/// - No question (`summary.question != true`)
/// - Optional 6.29 review verdict (`review.json` or `summary.review`): `PASS` required,
///   `NEEDS_CONTROLLER` or non-PASS pauses
/// - Terminal state is `delivered` (when `accept` is configured, `accept.ok` must be true)
///   or `answered` (when no `accept` command is configured)
pub fn evaluate_run_dir(run_dir: &Path) -> UpstreamVerdict {
    if !run_dir.join("meta.json").is_file() {
        return UpstreamVerdict::Paused {
            upstream_run: run_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            reason: "upstream run directory missing".into(),
            state: "missing".into(),
        };
    }
    let meta = json(run_dir.join("meta.json"));
    let run_id = events::run_id_for(run_dir, &meta);
    let st = runs::state(run_dir);
    if runs::active(&st) {
        return UpstreamVerdict::Waiting;
    }
    let sum = json(run_dir.join("summary.json"));
    let review = {
        let f = json(run_dir.join("review.json"));
        if f.is_object() {
            f
        } else {
            sum["review"].clone()
        }
    };
    evaluate_run_outcome(&run_id, &st, &meta, &sum, &review)
}

pub fn evaluate_run_outcome(
    run_id: &str,
    state: &str,
    meta: &Value,
    sum: &Value,
    review: &Value,
) -> UpstreamVerdict {
    if runs::active(state) {
        return UpstreamVerdict::Waiting;
    }
    if b(sum, "question") {
        return UpstreamVerdict::Paused {
            upstream_run: run_id.to_string(),
            reason: "上游提问，等待主控决定".into(),
            state: "question".into(),
        };
    }
    if b(sum, "policyRefused") {
        return UpstreamVerdict::Paused {
            upstream_run: run_id.to_string(),
            reason: "上游结论被模型服务拒绝，需主控确认".into(),
            state: "policy_refused".into(),
        };
    }
    if review.is_object() {
        let verdict = s(review, "verdict");
        if verdict == "NEEDS_CONTROLLER" {
            return UpstreamVerdict::Paused {
                upstream_run: run_id.to_string(),
                reason: "审查结果 NEEDS_CONTROLLER，需要主控介入".into(),
                state: "NEEDS_CONTROLLER".into(),
            };
        }
        if !verdict.is_empty() && verdict != "PASS" {
            return UpstreamVerdict::Paused {
                upstream_run: run_id.to_string(),
                reason: format!("审查未通过（{verdict}）"),
                state: verdict.to_string(),
            };
        }
    }
    let has_accept = !s(meta, "accept").is_empty();
    match state {
        "delivered" => {
            if sum["accept"].is_object() && !b(&sum["accept"], "ok") {
                UpstreamVerdict::Paused {
                    upstream_run: run_id.to_string(),
                    reason: "上游验收未通过".into(),
                    state: state.to_string(),
                }
            } else {
                UpstreamVerdict::Satisfied
            }
        }
        "answered" => {
            if has_accept {
                UpstreamVerdict::Paused {
                    upstream_run: run_id.to_string(),
                    reason: "上游未通过验收".into(),
                    state: state.to_string(),
                }
            } else {
                UpstreamVerdict::Satisfied
            }
        }
        other => UpstreamVerdict::Paused {
            upstream_run: run_id.to_string(),
            reason: format!("上游结束状态为 {other}"),
            state: other.to_string(),
        },
    }
}

fn evaluate_dep(
    dep: &str,
    after_dirs: &Value,
    items_by_name: &HashMap<String, Value>,
) -> UpstreamVerdict {
    // 1. Another DAG queue item by name
    if let Some(up) = items_by_name.get(dep) {
        let ust = s(up, "status");
        return match ust {
            "pending" | "dispatching" => UpstreamVerdict::Waiting,
            "dispatched" => {
                let rdir = s(up, "runDir");
                if !rdir.is_empty() {
                    evaluate_run_dir(Path::new(rdir))
                } else {
                    UpstreamVerdict::Waiting
                }
            }
            "paused" => UpstreamVerdict::Paused {
                upstream_run: s(up, "upstreamRun").to_string(),
                reason: format!("上游队列项 {dep} 已暂停（{}）", s(up, "pauseReason")),
                state: "paused".into(),
            },
            "cancelled" => UpstreamVerdict::Paused {
                upstream_run: dep.to_string(),
                reason: format!("上游队列项 {dep} 已取消"),
                state: "cancelled".into(),
            },
            _ => UpstreamVerdict::Waiting,
        };
    }

    // 2. Direct run directory recorded at add time
    if let Some(dir_str) = after_dirs.get(dep).and_then(Value::as_str) {
        if !dir_str.is_empty() {
            return evaluate_run_dir(Path::new(dir_str));
        }
    }

    // 3. Fallback: try resolving in current runs_root
    if let Ok(run_dir) = runs::resolve_head(dep) {
        return evaluate_run_dir(&run_dir);
    }

    UpstreamVerdict::Paused {
        upstream_run: dep.to_string(),
        reason: format!("找不到上游任务 {dep}"),
        state: "missing".into(),
    }
}

fn evaluate_applied_dep(
    dep: &str,
    after_dirs: &Value,
    items_by_name: &HashMap<String, Value>,
    state: &Path,
    repo: &str,
    prefetched_applied: &HashMap<(String, String), bool>,
) -> UpstreamVerdict {
    let check_applied = |run_ref: &str| -> bool {
        if let Some(&cached) = prefetched_applied.get(&(repo.to_string(), run_ref.to_string())) {
            return cached;
        }
        crate::work::is_run_applied(run_ref, state, Some(repo))
    };
    if let Some(up) = items_by_name.get(dep) {
        let ust = s(up, "status");
        return match ust {
            "pending" | "dispatching" => UpstreamVerdict::Waiting,
            "dispatched" => {
                let rid = s(up, "runId");
                let rdir = s(up, "runDir");
                if !rid.is_empty() && check_applied(rid) {
                    return UpstreamVerdict::Satisfied;
                }
                if !rdir.is_empty() {
                    match evaluate_run_dir(Path::new(rdir)) {
                        UpstreamVerdict::Paused { upstream_run, reason, state } => {
                            UpstreamVerdict::Paused { upstream_run, reason, state }
                        }
                        _ => UpstreamVerdict::Waiting,
                    }
                } else {
                    UpstreamVerdict::Waiting
                }
            }
            "paused" => UpstreamVerdict::Paused {
                upstream_run: s(up, "upstreamRun").to_string(),
                reason: format!("上游队列项 {dep} 已暂停（{}）", s(up, "pauseReason")),
                state: "paused".into(),
            },
            "cancelled" => UpstreamVerdict::Paused {
                upstream_run: dep.to_string(),
                reason: format!("上游队列项 {dep} 已取消"),
                state: "cancelled".into(),
            },
            _ => UpstreamVerdict::Waiting,
        };
    }
    if check_applied(dep) {
        return UpstreamVerdict::Satisfied;
    }
    if let Some(dir_str) = after_dirs.get(dep).and_then(Value::as_str) {
        if !dir_str.is_empty() {
            let p = Path::new(dir_str);
            if p.join(".applied").exists() || p.join(".applied-cleaned").exists() {
                return UpstreamVerdict::Satisfied;
            }
            return match evaluate_run_dir(p) {
                UpstreamVerdict::Paused { upstream_run, reason, state } => {
                    UpstreamVerdict::Paused { upstream_run, reason, state }
                }
                _ => UpstreamVerdict::Waiting,
            };
        }
    }
    if let Ok(run_dir) = runs::resolve_head(dep) {
        if run_dir.join(".applied").exists() || run_dir.join(".applied-cleaned").exists() {
            return UpstreamVerdict::Satisfied;
        }
        return match evaluate_run_dir(&run_dir) {
            UpstreamVerdict::Paused { upstream_run, reason, state } => {
                UpstreamVerdict::Paused { upstream_run, reason, state }
            }
            _ => UpstreamVerdict::Waiting,
        };
    }
    UpstreamVerdict::Waiting
}

fn has_cycle(items: &[Value], new_name: &str, new_after: &[String]) -> bool {
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for item in items {
        if s(item, "status") == "cancelled" {
            continue;
        }
        let mut deps = strings(&item["after"]);
        deps.extend(strings(&item["afterApplied"]));
        adj.insert(s(item, "name").to_string(), deps);
    }
    adj.insert(new_name.to_string(), new_after.to_vec());

    // Check if new_name is reachable from any of new_after
    let mut stack: Vec<String> = new_after.to_vec();
    let mut visited: HashSet<String> = HashSet::new();
    while let Some(cur) = stack.pop() {
        if cur == new_name {
            return true;
        }
        if !visited.insert(cur.clone()) {
            continue;
        }
        if let Some(parents) = adj.get(&cur) {
            for p in parents {
                stack.push(p.clone());
            }
        }
    }
    false
}

/// Called from `common::finish_run` (and `queue add`) to evaluate pending DAG items.
/// Zero LLM calls: spawns `delegate start` directly when ready, or pauses and emits
/// `queue_paused` when an upstream failed / questioned / needs controller.
pub fn advance() {
    let state = state_dir();
    if !queue_path(&state).is_file() {
        return;
    }
    // Loop because pausing item B may immediately pause item C (--after B).
    for _ in 0..64 {
        let to_dispatch: Vec<Value>;
        let mut paused_events: Vec<(String, String, Value)> = Vec::new();

        // Precompute `is_run_applied` (which may inspect git/worktrees) strictly OUTSIDE `.state.lock`.
        let prefetched_applied: HashMap<(String, String), bool> = {
            let snap = load_items(&state);
            let mut snap_by_name: HashMap<String, &Value> = HashMap::new();
            for item in &snap {
                if s(item, "status") != "cancelled" {
                    snap_by_name.insert(s(item, "name").to_string(), item);
                }
            }
            let mut map = HashMap::new();
            for item in &snap {
                if s(item, "status") != "pending" {
                    continue;
                }
                let item_repo = s(item, "repo").to_string();
                for dep in strings(&item["afterApplied"]) {
                    if let Some(up) = snap_by_name.get(&dep) {
                        if s(up, "status") == "dispatched" {
                            let rid = s(up, "runId");
                            if !rid.is_empty() {
                                let key = (item_repo.clone(), rid.to_string());
                                map.entry(key).or_insert_with(|| {
                                    crate::work::is_run_applied(rid, &state, Some(&item_repo))
                                });
                            }
                        }
                    } else {
                        let key = (item_repo.clone(), dep.clone());
                        map.entry(key).or_insert_with(|| {
                            crate::work::is_run_applied(&dep, &state, Some(&item_repo))
                        });
                    }
                }
            }
            map
        };

        {
            let Ok(_guard) = crate::locks::acquire_state(&state) else {
                return;
            };
            let mut items = load_items(&state);
            let mut by_name: HashMap<String, Value> = HashMap::new();
            for item in &items {
                if s(item, "status") != "cancelled" {
                    by_name.insert(s(item, "name").to_string(), item.clone());
                }
            }

            let mut ready_indices: Vec<usize> = Vec::new();
            let mut changed = false;

            for (idx, item) in items.iter_mut().enumerate() {
                if s(item, "status") != "pending" {
                    continue;
                }
                let deps = strings(&item["after"]);
                let applied_deps = strings(&item["afterApplied"]);
                let item_repo = s(item, "repo").to_string();
                let mut all_satisfied = true;
                let mut paused: Option<(String, String, String)> = None;

                for dep in &deps {
                    match evaluate_dep(dep, &item["afterDirs"], &by_name) {
                        UpstreamVerdict::Satisfied => {}
                        UpstreamVerdict::Waiting => {
                            all_satisfied = false;
                        }
                        UpstreamVerdict::Paused {
                            upstream_run,
                            reason,
                            state: st,
                        } => {
                            all_satisfied = false;
                            paused = Some((upstream_run, reason, st));
                            break;
                        }
                    }
                }
                if paused.is_none() {
                    for dep in &applied_deps {
                        match evaluate_applied_dep(
                            dep,
                            &item["afterDirs"],
                            &by_name,
                            &state,
                            &item_repo,
                            &prefetched_applied,
                        ) {
                            UpstreamVerdict::Satisfied => {}
                            UpstreamVerdict::Waiting => {
                                all_satisfied = false;
                            }
                            UpstreamVerdict::Paused {
                                upstream_run,
                                reason,
                                state: st,
                            } => {
                                all_satisfied = false;
                                paused = Some((upstream_run, reason, st));
                                break;
                            }
                        }
                    }
                }

                if let Some((upstream_run, reason, st)) = paused {
                    item["status"] = json!("paused");
                    item["pausedAt"] = json!(iso());
                    item["pauseReason"] = json!(reason);
                    item["upstreamRun"] = json!(upstream_run);
                    changed = true;
                    paused_events.push((
                        upstream_run.clone(),
                        s(item, "repo").to_string(),
                        json!({
                            "queueName": s(item, "name"),
                            "after": item["after"],
                            "upstreamRun": upstream_run,
                            "reason": reason,
                            "state": st,
                        }),
                    ));
                } else if all_satisfied {
                    item["status"] = json!("dispatching");
                    ready_indices.push(idx);
                    changed = true;
                }
            }

            if changed {
                let _ = save_items(&state, &items);
            }
            to_dispatch = ready_indices
                .into_iter()
                .map(|idx| items[idx].clone())
                .collect();
        }

        for (run_id, repo, payload) in paused_events {
            events::emit(&run_id, &repo, "queue_paused", payload);
        }

        if to_dispatch.is_empty() {
            // If we paused something, loop once more to cascade pause to downstream pending items;
            // otherwise we are done.
            let still_has_cascade = {
                let items = load_items(&state);
                let paused_names: BTreeSet<String> = items
                    .iter()
                    .filter(|i| matches!(s(i, "status"), "paused" | "cancelled"))
                    .map(|i| s(i, "name").to_string())
                    .collect();
                items.iter().any(|i| {
                    s(i, "status") == "pending"
                        && strings(&i["after"])
                            .iter()
                            .any(|d| paused_names.contains(d))
                })
            };
            if !still_has_cascade {
                break;
            }
            continue;
        }

        for item in to_dispatch {
            dispatch_one(&state, &item);
        }
    }
}

fn dispatch_one(state: &Path, item: &Value) {
    let qname = s(item, "name").to_string();
    let repo = s(item, "repo").to_string();
    let cwd = s(item, "cwd");
    let runs_dir = s(item, "runsDir");
    let prompt_file = s(item, "promptFile");
    let start_args = strings(&item["startArgs"]);

    let mut cmd = Command::new(script());
    cmd.arg("start")
        .arg("--json")
        .arg("--name")
        .arg(&qname)
        .arg("--prompt-file")
        .arg(prompt_file)
        .args(&start_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !cwd.is_empty() && Path::new(cwd).is_dir() {
        cmd.current_dir(cwd);
    }
    for guard in GUARDS {
        cmd.env_remove(guard);
    }
    if !runs_dir.is_empty() {
        cmd.env("DELEGATE_RUNS", runs_dir);
    }

    let result: Res<(String, String)> = (|| {
        let out = cmd.output().map_err(|e| format!("spawn delegate start: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            return Err(format!(
                "delegate start exited {}: {}",
                out.status.code().unwrap_or(1),
                clip(stderr.trim(), 240)
            ));
        }
        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('{') {
                if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                    let rid = s(&v, "run").to_string();
                    let rdir = s(&v, "dir").to_string();
                    if !rid.is_empty() {
                        return Ok((rid, rdir));
                    }
                }
            }
        }
        Err("delegate start produced no run JSON".into())
    })();

    let Ok(_guard) = crate::locks::acquire_state(state) else {
        return;
    };
    let mut items = load_items(state);
    if let Some(entry) = items
        .iter_mut()
        .find(|e| s(e, "name") == qname && s(e, "status") == "dispatching")
    {
        match result {
            Ok((next_run, next_dir)) => {
                entry["status"] = json!("dispatched");
                entry["runId"] = json!(next_run);
                entry["runDir"] = json!(next_dir);
                entry["dispatchedAt"] = json!(iso());
                let after = entry["after"].clone();
                let _ = save_items(state, &items);
                drop(_guard);
                events::emit(
                    &next_run,
                    &repo,
                    "queue_dispatched",
                    json!({
                        "queueName": qname,
                        "after": after,
                        "nextRun": next_run,
                        "name": qname,
                    }),
                );
            }
            Err(err) => {
                entry["status"] = json!("paused");
                entry["pausedAt"] = json!(iso());
                entry["pauseReason"] = json!(err);
                let after = entry["after"].clone();
                let _ = save_items(state, &items);
                drop(_guard);
                events::emit(
                    "",
                    &repo,
                    "queue_paused",
                    json!({
                        "queueName": qname,
                        "after": after,
                        "reason": err,
                        "state": "dispatch_error",
                    }),
                );
            }
        }
    }
}

/// `delegate queue add|list|remove|cancel ...`
pub fn command(args: &[String]) -> Res<i32> {
    let Some((sub, rest)) = args.split_first() else {
        return list_cmd(&[]);
    };
    match sub.as_str() {
        "add" => add_cmd(rest),
        "list" | "ls" => list_cmd(rest),
        "remove" | "rm" | "cancel" => remove_cmd(rest),
        other => Err(format!(
            "unknown queue subcommand: {other} (expected: add, list, remove, cancel)"
        )),
    }
}

fn add_cmd(args: &[String]) -> Res<i32> {
    if let Some(err) = crate::agents::nesting_error() {
        return Err(err);
    }
    let mut name: Option<String> = None;
    let mut after: Vec<String> = Vec::new();
    let mut after_applied: Vec<String> = Vec::new();
    let mut prompt_file: Option<String> = None;
    let mut start_args: Vec<String> = Vec::new();
    let mut json_mode = false;

    let value_start_flags = [
        "--tier",
        "--workdir",
        "--image",
        "--share",
        "--protect",
        "--allow",
        "--accept",
        "--accept-also",
        "--accept-timeout",
        "--evidence",
        "--evidence-timeout",
        "--timeout",
        "--max-answer",
        "--retries",
        "--provider",
        "--model",
        "--review-model",
        "--thinking",
        "--class",
        "--overflow",
        "--in",
        "--work",
    ];
    let bool_start_flags = [
        "--read-only",
        "--shell",
        "--in-place",
        "--worktree",
        "--hide-accept",
        "--no-accept",
        "--no-evidence",
        "--allow-parallel-writes",
    ];

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
            "--after" => {
                let raw = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --after: expected one argument")?
                    }
                };
                for part in raw.split(',') {
                    let t = part.trim();
                    if !t.is_empty() {
                        after.push(t.to_string());
                    }
                }
            }
            "--after-applied" => {
                let raw = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --after-applied: expected one argument")?
                    }
                };
                for part in raw.split(',') {
                    let t = part.trim();
                    if !t.is_empty() {
                        after_applied.push(t.to_string());
                    }
                }
            }
            "--prompt-file" => {
                let raw = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --prompt-file: expected one argument")?
                    }
                };
                prompt_file = Some(raw);
            }
            "--json" => json_mode = true,
            "--protect-reason" => {
                if inline.is_some() || i + 2 >= args.len() {
                    return Err("--protect-reason expects PATH REASON".into());
                }
                start_args.push(args[i].clone());
                start_args.push(args[i + 1].clone());
                start_args.push(args[i + 2].clone());
                i += 2;
            }
            k if value_start_flags.contains(&k) => {
                if inline.is_some() {
                    start_args.push(a.clone());
                } else {
                    i += 1;
                    let v = args
                        .get(i)
                        .cloned()
                        .ok_or_else(|| format!("argument {k}: expected one argument"))?;
                    start_args.push(k.to_string());
                    start_args.push(v);
                }
            }
            k if bool_start_flags.contains(&k) => {
                start_args.push(k.to_string());
            }
            other if other.starts_with('-') => {
                return Err(format!("unrecognized arguments: {other}"));
            }
            other => {
                if name.is_none() {
                    name = Some(other.to_string());
                } else {
                    return Err(format!("unexpected positional argument: {other}"));
                }
            }
        }
        i += 1;
    }

    let name = name
        .filter(|n| !n.trim().is_empty())
        .ok_or("usage: delegate queue add <name> --after|--after-applied <run|name> --prompt-file <FILE> [start options...]")?;
    if after.is_empty() && after_applied.is_empty() {
        return Err("queue add requires at least one --after or --after-applied <run|name>".into());
    }
    let prompt_file_raw = prompt_file
        .ok_or("queue add requires --prompt-file <FILE>")?;

    // Validate forwarded start options right now.
    let mut probe_args = start_args.clone();
    probe_args.push("--prompt".into());
    probe_args.push("validation probe".into());
    let _ = launch::parse_launch(&probe_args, false, false)?;

    let state = state_dir();
    fs::create_dir_all(&state).map_err(|e| e.to_string())?;

    let resolved_prompt = if prompt_file_raw == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| e.to_string())?;
        if buf.trim().is_empty() {
            return Err("empty prompt on stdin".into());
        }
        let dir = state.join("queue-prompts");
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let slug: String = name
            .chars()
            .map(|c| if c.is_alphanumeric() || "._-".contains(c) { c } else { '-' })
            .take(40)
            .collect();
        let saved = dir.join(format!("{slug}-{}.md", now_ns()));
        write(&saved, buf)?;
        saved
    } else {
        let p = PathBuf::from(&prompt_file_raw);
        if !p.is_file() {
            return Err(format!("prompt file does not exist: {prompt_file_raw}"));
        }
        fs::canonicalize(&p).unwrap_or(p)
    };

    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let cwd_canon = fs::canonicalize(&cwd).unwrap_or(cwd);
    let repo = git_top(&cwd_canon)
        .map(|p| fs::canonicalize(&p).unwrap_or(p).to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd_canon.to_string_lossy().into_owned());
    let runs_dir = std::env::var_os("DELEGATE_RUNS")
        .or_else(|| std::env::var_os("PI_DELEGATE_RUNS"))
        .filter(|v| !v.is_empty())
        .map(|v| {
            let p = PathBuf::from(v);
            fs::canonicalize(&p)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_default();

    let created_item: Value;
    {
        let _guard = crate::locks::acquire_state(&state).map_err(|e| e.to_string())?;
        let mut items = load_items(&state);

        if items
            .iter()
            .any(|item| s(item, "name") == name && s(item, "status") != "cancelled")
        {
            return Err(format!(
                "queue item '{name}' already exists (remove or cancel it first)"
            ));
        }

        let active_queue_names: HashSet<String> = items
            .iter()
            .filter(|item| s(item, "status") != "cancelled")
            .map(|item| s(item, "name").to_string())
            .collect();

        let mut normalized_after: Vec<String> = Vec::new();
        let mut normalized_after_applied: Vec<String> = Vec::new();
        let mut after_dirs = serde_json::Map::new();
        for dep in &after {
            if active_queue_names.contains(dep) {
                if !normalized_after.contains(dep) {
                    normalized_after.push(dep.clone());
                }
            } else if let Ok(run_dir) = runs::resolve_head(dep) {
                let meta = json(run_dir.join("meta.json"));
                let rid = events::run_id_for(&run_dir, &meta);
                let canonical_dir = fs::canonicalize(&run_dir)
                    .unwrap_or(run_dir)
                    .to_string_lossy()
                    .into_owned();
                if !normalized_after.contains(&rid) {
                    normalized_after.push(rid.clone());
                }
                after_dirs.insert(rid, json!(canonical_dir));
            } else {
                return Err(format!(
                    "unknown upstream run or queue item in --after: '{dep}'"
                ));
            }
        }
        for dep in &after_applied {
            if active_queue_names.contains(dep) {
                if !normalized_after_applied.contains(dep) {
                    normalized_after_applied.push(dep.clone());
                }
            } else if let Ok(run_dir) = runs::resolve_head(dep) {
                let meta = json(run_dir.join("meta.json"));
                let rid = events::run_id_for(&run_dir, &meta);
                let canonical_dir = fs::canonicalize(&run_dir)
                    .unwrap_or(run_dir)
                    .to_string_lossy()
                    .into_owned();
                if !normalized_after_applied.contains(&rid) {
                    normalized_after_applied.push(rid.clone());
                }
                after_dirs.insert(rid, json!(canonical_dir));
            } else if crate::work::is_run_applied(dep, &state, Some(&repo)) {
                if !normalized_after_applied.contains(dep) {
                    normalized_after_applied.push(dep.clone());
                }
            } else {
                return Err(format!(
                    "unknown upstream run or queue item in --after-applied: '{dep}'"
                ));
            }
        }

        let mut all_deps = normalized_after.clone();
        all_deps.extend(normalized_after_applied.iter().cloned());
        if has_cycle(&items, &name, &all_deps) {
            return Err(format!(
                "dependency cycle detected when adding '{name}' after {:?}",
                all_deps
            ));
        }

        created_item = json!({
            "name": name,
            "after": normalized_after,
            "afterApplied": normalized_after_applied,
            "afterDirs": Value::Object(after_dirs),
            "promptFile": resolved_prompt.to_string_lossy(),
            "startArgs": start_args,
            "cwd": cwd_canon.to_string_lossy(),
            "repo": repo,
            "runsDir": runs_dir,
            "status": "pending",
            "createdAt": iso(),
        });
        items.push(created_item.clone());
        save_items(&state, &items)?;
    }

    events::emit(
        "",
        &repo,
        "queued",
        json!({
            "scope": "dag",
            "queueName": name,
            "after": created_item["after"],
            "name": name,
        }),
    );

    // If upstream is already finished, advance immediately!
    advance();

    // Reload latest status of this item to show to user.
    let final_item = load_items(&state)
        .into_iter()
        .rev()
        .find(|i| s(i, "name") == name)
        .unwrap_or(created_item);

    if json_mode || crate::output::json_enabled() {
        println!("{final_item}");
    } else {
        println!("{}", format_item_human(&final_item));
    }
    Ok(0)
}

fn format_item_human(item: &Value) -> String {
    let name = s(item, "name");
    let status = s(item, "status");
    let after = strings(&item["after"]).join(", ");
    let after_applied = strings(&item["afterApplied"]).join(", ");
    let mut dep_parts = Vec::new();
    if !after.is_empty() {
        dep_parts.push(format!("after: [{after}]"));
    }
    if !after_applied.is_empty() {
        dep_parts.push(format!("after-applied: [{after_applied}]"));
    }
    let deps_str = dep_parts.join(" · ");
    let repo = Path::new(s(item, "repo"))
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("-");
    let status_zh = match status {
        "pending" => "等待上游".to_string(),
        "dispatching" => "派发中".to_string(),
        "dispatched" => format!("已自动派发 -> {}", s(item, "runId")),
        "paused" => format!("已暂停（{}）", s(item, "pauseReason")),
        "cancelled" => "已取消".to_string(),
        other => other.to_string(),
    };
    format!("{name} · {status_zh} · {deps_str} · [{repo}] · {}", s(item, "promptFile"))
}

fn list_cmd(args: &[String]) -> Res<i32> {
    let mut show_all = false;
    let mut json_mode = crate::output::json_enabled();
    for a in args {
        match a.as_str() {
            "--all" => show_all = true,
            "--json" => json_mode = true,
            other => return Err(format!("unrecognized arguments: {other}")),
        }
    }
    let state = state_dir();
    let all_items = load_items(&state);
    let items: Vec<Value> = all_items
        .iter()
        .filter(|i| show_all || s(i, "status") != "cancelled")
        .cloned()
        .collect();
    if items.is_empty() {
        if !json_mode {
            println!("续派队列为空");
        }
        return Ok(0);
    }

    let now_ts = epoch() as i64;
    let mut visible_runs = crate::runs::all_runs();
    for mrun in crate::launch::machine_runs(&state) {
        if !visible_runs.contains(&mrun) {
            visible_runs.push(mrun);
        }
    }
    let eta_model = crate::speed::load_or_refresh_eta_model_in(&state, now_ts, true);
    let eta_ctx = crate::eta::LiveContext::capture(&state, now_ts, &visible_runs);
    let dag = crate::eta::estimate_queue_dag_in(&all_items, None, &eta_model, &eta_ctx);

    if json_mode {
        let by_name: std::collections::HashMap<&str, &crate::eta::WorkItemEta> =
            dag.items.iter().map(|it| (it.id.as_str(), it)).collect();
        for item in &items {
            let mut out = item.clone();
            if let Some(ie) = by_name.get(s(item, "name")) {
                out["eta"] = ie.to_json();
            }
            println!("{out}");
        }
    } else {
        if dag.write_chains > 0 || !dag.blockers.is_empty() {
            println!("{}", dag.headline);
        }
        for item in &items {
            println!("{}", format_item_human(item));
        }
    }
    Ok(0)
}

fn remove_cmd(args: &[String]) -> Res<i32> {
    let mut json_mode = crate::output::json_enabled();
    let mut names: Vec<String> = Vec::new();
    for a in args {
        if a == "--json" {
            json_mode = true;
        } else if a.starts_with('-') {
            return Err(format!("unrecognized arguments: {a}"));
        } else {
            names.push(a.clone());
        }
    }
    if names.is_empty() {
        return Err("queue remove requires at least one <name>".into());
    }
    let state = state_dir();
    let _guard = crate::locks::acquire_state(&state).map_err(|e| e.to_string())?;
    let mut items = load_items(&state);
    let mut removed = Vec::new();
    for target in &names {
        let mut matched = false;
        for item in items.iter_mut() {
            if s(item, "name") == target && s(item, "status") != "cancelled" {
                item["status"] = json!("cancelled");
                item["cancelledAt"] = json!(iso());
                removed.push(target.clone());
                matched = true;
            }
        }
        if !matched {
            return Err(format!("queue item not found: '{target}'"));
        }
    }
    save_items(&state, &items)?;
    if json_mode {
        println!("{}", json!({"removed": removed}));
    } else {
        println!("已取消续派项：{}", removed.join("、"));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_delivery_question_failure_and_629_review_verdicts() {
        let write_meta = json!({"accept": "cargo test"});
        let ro_meta = json!({});

        // Delivered + accept ok -> Satisfied
        assert_eq!(
            evaluate_run_outcome(
                "r1",
                "delivered",
                &write_meta,
                &json!({"state": "delivered", "accept": {"ok": true}}),
                &Value::Null
            ),
            UpstreamVerdict::Satisfied
        );

        // Delivered + 6.29 review PASS -> Satisfied
        assert_eq!(
            evaluate_run_outcome(
                "r1",
                "delivered",
                &write_meta,
                &json!({"state": "delivered", "accept": {"ok": true}}),
                &json!({"verdict": "PASS"})
            ),
            UpstreamVerdict::Satisfied
        );

        // Delivered + 6.29 review NEEDS_CONTROLLER -> Paused
        assert!(matches!(
            evaluate_run_outcome(
                "r1",
                "delivered",
                &write_meta,
                &json!({"state": "delivered", "accept": {"ok": true}}),
                &json!({"verdict": "NEEDS_CONTROLLER"})
            ),
            UpstreamVerdict::Paused { .. }
        ));

        // Question -> Paused
        assert!(matches!(
            evaluate_run_outcome(
                "r2",
                "answered",
                &ro_meta,
                &json!({"state": "answered", "question": true}),
                &Value::Null
            ),
            UpstreamVerdict::Paused { .. }
        ));

        // Policy-refused conclusion -> Paused (not treated as normal delivery)
        assert!(matches!(
            evaluate_run_outcome(
                "r2p",
                "delivered",
                &write_meta,
                &json!({"state": "delivered", "accept": {"ok": true}, "policyRefused": true}),
                &json!({"verdict": "PASS"})
            ),
            UpstreamVerdict::Paused { state, .. } if state == "policy_refused"
        ));

        // Rejected -> Paused
        assert!(matches!(
            evaluate_run_outcome(
                "r3",
                "rejected",
                &write_meta,
                &json!({"state": "rejected", "accept": {"ok": false}}),
                &Value::Null
            ),
            UpstreamVerdict::Paused { .. }
        ));

        // Read-only answered without question -> Satisfied
        assert_eq!(
            evaluate_run_outcome(
                "r4",
                "answered",
                &ro_meta,
                &json!({"state": "answered"}),
                &Value::Null
            ),
            UpstreamVerdict::Satisfied
        );
    }

    #[test]
    fn detects_direct_and_transitive_cycles() {
        let items = vec![
            json!({"name": "b", "after": ["a"], "status": "pending"}),
            json!({"name": "c", "after": ["b"], "status": "pending"}),
        ];
        assert!(has_cycle(&items, "a", &["a".into()]));
        assert!(has_cycle(&items, "a", &["c".into()]));
        assert!(!has_cycle(&items, "d", &["c".into()]));
    }
}
