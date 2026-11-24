use crate::agents;
use crate::changes;
use crate::common::*;
use crate::lane;
use crate::launch;
use crate::probe;
use crate::runs;
use crate::transient;
use crate::worktree;
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicI32, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::Duration;

type Recorded = Option<(Vec<Value>, Value)>;

static EXTRA_STOP_HOLDERS: OnceLock<Mutex<Vec<Arc<AtomicI32>>>> = OnceLock::new();

pub(crate) fn register_stop_holder(h: Arc<AtomicI32>) {
    let list = EXTRA_STOP_HOLDERS.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut guard) = list.lock() {
        guard.push(h);
    }
}

pub(crate) fn unregister_stop_holder(h: &Arc<AtomicI32>) {
    if let Some(list) = EXTRA_STOP_HOLDERS.get() {
        if let Ok(mut guard) = list.lock() {
            guard.retain(|x| !Arc::ptr_eq(x, h));
        }
    }
}

fn kill_extra_stop_holders(runpath: &Path) {
    if let Some(list) = EXTRA_STOP_HOLDERS.get() {
        if let Ok(guard) = list.lock() {
            for h in guard.iter() {
                let pid = h.load(Ordering::SeqCst);
                if pid > 0 {
                    crate::cleanup::record(runpath, pid);
                    kill_group(pid, 5.0);
                }
            }
        }
    }
}

fn evidence(meta: &Value, run: &Path, holder: Arc<AtomicI32>) -> Value {
    let path = run.join("evidence.log");
    let mut result = json!({"exit":null,"timedOut":false,"seconds":0,"tail":"","log":path});
    let execute = || -> Res<(i32, bool, f64)> {
        let _slot = lane::acquire_shell("evidence", Some(run), Some(run), |ahead, _| {
            log_event(run, json!({"e":"queue","role":"evidence","ahead":ahead}));
        })?;
        if lane::stopped() {
            return Err("stopped while queued for the heavy lane".into());
        }
        let mut log = File::create(&path).map_err(|e| e.to_string())?;
        let started = epoch();
        let outcome = run_shell(
            (s(meta, "evidenceCommand"), "evidence"),
            Path::new(s(meta, "workdir")),
            run,
            &meta["env"],
            Some(meta["evidenceTimeoutSeconds"].as_f64().unwrap_or(1800.0)),
            &mut log,
            Some(&holder),
        );
        let elapsed = epoch() - started;
        outcome.map(|(code, timed)| (code, timed, elapsed))
    };
    match execute() {
        Ok((code, timed, elapsed)) => {
            result["exit"] = json!(code);
            result["timedOut"] = json!(timed);
            // Milliseconds: serde_json does not round-trip every f64, so meta and --json could differ.
            result["seconds"] = json!((elapsed * 1000.0).round() / 1000.0);
            result["tail"] = json!(read(&path)
                .chars()
                .rev()
                .take(1500)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
                .trim());
        }
        Err(error) => {
            result["tail"] = json!(error);
        }
    }
    result
}

pub fn log_event(run: &Path, mut event: Value) {
    event["at"] = json!(iso());
    let _ = append(run.join("events.jsonl"), &format!("{event}\n"));
}
pub(crate) fn attempts_from(
    meta: &Value,
    run: &Path,
    first: i64,
    holder: Arc<AtomicI32>,
    grace: Arc<std::sync::Mutex<Option<f64>>>,
) -> (String, String, i64) {
    let (mut verdict, mut answer, mut attempt) = ("failed".to_string(), String::new(), first - 1);
    for current in first..first + n(meta, "retries") + 1 {
        attempt = current;
        if lane::stopped() {
            verdict = "stopped".into();
            break;
        }
        match agents::run_agent(meta, run, attempt, holder.clone(), grace.clone()) {
            Ok((v, a)) => {
                verdict = v;
                answer = a;
            }
            Err(e) => {
                verdict = "failed".into();
                let _ = append(run.join("stderr.log"), &format!("supervisor error: {e}\n"));
                break;
            }
        }
        if verdict != "malformed" || attempt >= first + n(meta, "retries") {
            break;
        }
        log_event(run, json!({"e":"rerun","reason":"malformed answer"}));
    }
    if verdict == "ok" {
        (answer, attempt) = compress_answer(meta, run, answer, attempt, holder, grace);
    }
    (verdict, answer, attempt)
}

fn compress_answer(
    meta: &Value,
    run: &Path,
    answer: String,
    attempt: i64,
    holder: Arc<AtomicI32>,
    grace: Arc<std::sync::Mutex<Option<f64>>>,
) -> (String, i64) {
    let Some(limit) = meta["maxAnswer"].as_u64() else {
        return (answer, attempt);
    };
    if answer.chars().count() as u64 <= limit.saturating_add(limit / 2)
        || run.join("compression.json").exists()
        || lane::stopped()
    {
        return (answer, attempt);
    }
    let original = run.join("result-original.md");
    let mut record = json!({"ok":false,"original":original,"maxAnswer":limit});
    let execute = || -> Res<(String, String)> {
        write(&original, &answer)?;
        let session = runs::events(run)
            .into_iter()
            .rev()
            .find(|e| s(e, "e") == "session" && !s(e, "id").is_empty())
            .ok_or("no saved session for answer compression")?;
        let mut continuation = meta.clone();
        continuation["fork"] = json!(agents::continuation_target(meta, s(&session, "id"))
            .ok_or("missing session file for answer compression")?);
        let prompt = run.join("compression-prompt.md");
        write(&prompt, format!("压缩到 {limit} 字以内，保留结论与证据。只答复文字，不执行命令或修改文件。 / Compress your previous answer to at most {limit} characters, preserving conclusions and evidence. Respond with text only.\n"))?;
        continuation["agentPromptFile"] = json!(prompt);
        log_event(run, json!({"e":"answer_compression","maxAnswer":limit}));
        agents::run_agent(&continuation, run, attempt + 1, holder, grace)
    };
    let mut used_attempt = attempt;
    let result = match execute() {
        Ok((verdict, compressed)) => {
            used_attempt += 1;
            // Same tolerance as the trigger: a shorter answer slightly over the limit beats the original.
            let length = compressed.chars().count() as u64;
            if verdict == "ok"
                && !compressed.trim().is_empty()
                && length <= limit.saturating_add(limit / 2)
                && length < answer.chars().count() as u64
            {
                record["ok"] = json!(true);
                Some(compressed)
            } else {
                record["error"] = json!(format!(
                    "compression returned {verdict}, {} characters",
                    compressed.chars().count()
                ));
                None
            }
        }
        Err(error) => {
            record["error"] = json!(error);
            None
        }
    };
    let _ = write_json(run.join("compression.json"), &record);
    (result.unwrap_or(answer), used_attempt)
}
pub(crate) struct PreAccept {
    pub state: String,
    pub sum: Value,
    pub rec: Recorded,
    pub changed: Vec<String>,
    pub can_accept: bool,
}

pub(crate) fn settle_pre_accept(
    meta: &mut Value,
    run: &Path,
    verdict: &str,
    setup_error: &Option<String>,
    holder: Arc<AtomicI32>,
    question: bool,
) -> PreAccept {
    let mut state = if verdict == "ok" {
        "answered".to_string()
    } else {
        verdict.into()
    };
    let mut sum = json!({"state":state});
    if question {
        // A question waits for the delegator; acceptance and escalation do not apply.
        sum["question"] = json!(true);
    }
    if verdict == "stalled" {
        let reason = json(run.join("stall.json"));
        if reason.is_object() {
            sum["stall"] = reason;
        }
    }
    let initial = if setup_error.is_none() {
        changes::record(meta, run)
    } else {
        None
    };
    let inputs = strings(&meta["protectGenerated"]["inputs"]);
    let needs_generation = inputs.is_empty()
        || initial.as_ref().is_some_and(|rec| {
            rec.0
                .iter()
                .any(|c| worktree::matches_rule(s(c, "path"), &inputs))
        });
    let generated_checked = meta["protectGenerated"].is_object()
        && setup_error.is_none()
        && !lane::stopped()
        && needs_generation;
    let (generated_violations, generation_error) = if generated_checked {
        check_generated(meta, run, holder.clone())
    } else {
        (vec![], None)
    };
    let rec = if generated_checked {
        changes::record(meta, run)
    } else {
        initial
    };
    let changed = rec
        .as_ref()
        .map(|x| {
            x.0.iter()
                .map(|c| s(c, "path").to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let source_changed = crate::source_watch::changed(meta);
    let attributed_source = if s(meta, "mode") == "write" || b(meta, "shell") {
        crate::source_watch::attributed(meta, run, &source_changed)
    } else {
        vec![]
    };
    let source_leaks = crate::source_watch::worktree_leaks(meta);
    let worktree_nm_links = crate::source_watch::worktree_source_node_modules_links(meta);
    let outside_write = !attributed_source.is_empty()
        || !source_leaks.is_empty()
        || !worktree_nm_links.is_empty();
    let protected = meta["protect"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let allowed = strings(&meta["allow"]);
    let generated_paths = meta["protectGenerated"]["paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let protect_paths = if meta["worktree"].is_object() && s(meta, "mode") == "write" {
        worktree::pending_changed_paths(run, meta).unwrap_or_else(|| changed.clone())
    } else {
        changed.clone()
    };
    let mut violations = protect_paths
        .iter()
        .filter(|path| {
            if worktree::matches_rule(path, &allowed) { return false; }
            if generated_checked
                && generation_error.is_none()
                && worktree::matches_rule(path, &generated_paths)
            {
                return false;
            }
            protected.iter().any(|rule| {
                if rule.ends_with('/') {
                    path.starts_with(rule)
                } else {
                    path == rule
                }
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    violations.extend(
        generated_violations
            .iter()
            .filter(|path| {
                !worktree::matches_rule(path, &allowed) && protected
                    .iter()
                    .any(|rule| worktree::matches_rule(path, &[rule.to_string()]))
            })
            .cloned(),
    );
    violations.sort();
    violations.dedup();
    if let Some((changes, totals)) = &rec {
        sum["changes"] = totals.clone();
        if let Some(pending) = worktree::pending_changes(run, meta) {
            sum["pendingChanges"] = pending;
        }
        if let Ok(Some(shape)) = changes::shape(meta, run, changes) {
            sum["shape"] = shape;
        }
    } else if !s(meta, "top").is_empty() && setup_error.is_none() {
        sum["warning"] = json!("could not snapshot the working tree; changes are unknown");
    }
    if s(meta, "mode") == "read-only" && !changed.is_empty() && verdict == "ok" {
        if meta["worktree"].is_object() {
            sum["readOnlyViolation"] = json!(changed);
            sum["warning"] = json!("the read-only run changed files in its own worktree; they stay there, never applied");
        } else {
            sum["workspaceChanged"] = json!(changed);
            sum["warning"] = json!("the working tree changed during this in-place read-only run; the changes may be the caller's own");
        }
    }
    if !violations.is_empty() {
        state = "rejected".into();
        sum["state"] = json!(state);
        sum["protectViolation"] = json!(violations);
        let reasons = meta["protectReasons"]
            .as_object()
            .map(|reasons| {
                reasons
                    .iter()
                    .filter(|(rule, _)| {
                        violations.iter().any(|path| {
                            path == *rule
                                || (rule.ends_with('/') && path.starts_with(rule.as_str()))
                        })
                    })
                    .map(|(path, reason)| (path.clone(), reason.clone()))
                    .collect::<serde_json::Map<String, Value>>()
            })
            .unwrap_or_default();
        if !reasons.is_empty() {
            sum["protectViolationReasons"] = json!(reasons);
        }
        sum["error"] = json!("protected paths were changed; review protectViolation and remove those changes before retrying");
        if !generated_violations.is_empty() {
            sum["error"] = json!("生成物被手改 / generated output was manually edited; it differs from generated.command output");
        }
    }
    if let Some(error) = &generation_error {
        state = "rejected".into();
        sum["state"] = json!(state);
        sum["error"] = json!(error);
    }
    let mut sync_markers_found = false;
    let sync = json(run.join("sync.json"));
    if b(&sync, "agent") {
        let top = Path::new(s(&meta["worktree"], "path"));
        let mut paths = changed.clone();
        paths.extend(strings(&sync["conflicts"]));
        paths.extend(changes::uncommitted(meta).unwrap_or_default());
        paths.sort();
        paths.dedup();
        let markers = paths
            .into_iter()
            .filter(|p| worktree::has_conflict_markers(&top.join(p)))
            .collect::<Vec<_>>();
        if !markers.is_empty() {
            sync_markers_found = true;
            state = "rejected".into();
            sum["state"] = json!(state);
            sum["syncConflictMarkers"] = json!(markers);
            sum["accept"] = json!({"ok":false,"error":"unresolved sync conflict markers"});
            sum["error"] = json!("resolve sync conflict markers before delivery");
        }
    }
    if !source_changed.is_empty() {
        sum["sourceWorkspaceChanges"] = json!(source_changed);
    }
    if outside_write {
        state = "rejected".into();
        sum["state"] = json!(state);
        let mut violation = if !source_leaks.is_empty() {
            source_leaks.clone()
        } else if !worktree_nm_links.is_empty() {
            worktree_nm_links.clone()
        } else {
            attributed_source.clone()
        };
        if !source_leaks.is_empty() && !attributed_source.is_empty() {
            violation.extend(attributed_source.iter().cloned());
            violation.sort();
            violation.dedup();
        }
        sum["sourceWriteViolation"] = json!(violation);
        if !source_leaks.is_empty() {
            sum["error"] = json!(format!(
                "source node_modules points into this worktree ({}); restore source dependencies before cleaning the worktree / 源仓库的 node_modules 指向了本工作树，清理工作树前请先恢复源仓库依赖",
                source_leaks.join(", ")
            ));
        } else if !worktree_nm_links.is_empty() {
            sum["error"] = json!(format!(
                "worktree node_modules symlinks into the source checkout ({}); package managers would follow it and overwrite the source checkout / 工作树的 node_modules 软链指向了源仓库，包管理器会顺着软链改写源仓库",
                worktree_nm_links.join(", ")
            ));
        }
        sum.as_object_mut().unwrap().remove("question");
    }
    if b(meta, "shell") && (!changed.is_empty() || outside_write) {
        state = "rejected".into();
        sum["state"] = json!(state);
        let mut paths = changed.clone();
        let mut src_hits = attributed_source.clone();
        src_hits.extend(source_leaks.iter().cloned());
        src_hits.extend(worktree_nm_links.iter().cloned());
        src_hits.sort();
        src_hits.dedup();
        paths.extend(src_hits.iter().map(|p| format!("source:{p}")));
        sum["readOnlyViolation"] = json!(paths);
        sum["error"] = json!("只读任务产生了写入 / read-only shell task wrote files");
        sum.as_object_mut().unwrap().remove("question");
    }
    let can_accept = violations.is_empty()
        && !outside_write
        && generation_error.is_none()
        && !sync_markers_found
        && verdict == "ok"
        && !question
        && !lane::stopped();
    PreAccept {
        state,
        sum,
        rec,
        changed,
        can_accept,
    }
}

pub(crate) fn finalize_post_accept(meta: &Value, run: &Path, state: &mut String, sum: &mut Value) {
    // Acceptance commands may edit files too: inspect the actual delivered tree.
    if let Ok(paths) = changes::uncommitted(meta) {
        if !paths.is_empty() {
            sum["commitsDiffer"] = json!(paths);
        }
    }
    let post_leaks = crate::source_watch::worktree_leaks(meta);
    if !post_leaks.is_empty() {
        let source_changed = crate::source_watch::changed(meta);
        if !source_changed.is_empty() {
            sum["sourceWorkspaceChanges"] = json!(source_changed);
        }
        *state = "rejected".into();
        sum["state"] = json!(*state);
        sum["sourceWriteViolation"] = json!(post_leaks);
        sum["error"] = json!(format!(
            "source node_modules points into this worktree ({}); restore source dependencies before cleaning the worktree / 源仓库的 node_modules 指向了本工作树，清理工作树前请先恢复源仓库依赖",
            post_leaks.join(", ")
        ));
        sum.as_object_mut().unwrap().remove("question");
    }
    let sync = json(run.join("sync.json"));
    if b(&sync, "agent") && sum["syncConflictMarkers"].is_null() {
        let top = Path::new(s(&meta["worktree"], "path"));
        let mut paths = strings(&sync["conflicts"]);
        paths.extend(changes::uncommitted(meta).unwrap_or_default());
        paths.sort();
        paths.dedup();
        let markers = paths
            .into_iter()
            .filter(|p| worktree::has_conflict_markers(&top.join(p)))
            .collect::<Vec<_>>();
        if !markers.is_empty() {
            *state = "rejected".into();
            sum["state"] = json!(*state);
            sum["syncConflictMarkers"] = json!(markers);
            sum["accept"] = json!({"ok":false,"error":"unresolved sync conflict markers"});
            sum["error"] = json!("resolve sync conflict markers before delivery");
        }
    }
}
fn generated_manifest(
    top: &Path,
    rules: &[String],
) -> Res<std::collections::BTreeMap<String, String>> {
    fn visit(
        top: &Path,
        path: &Path,
        out: &mut std::collections::BTreeMap<String, String>,
    ) -> Res<()> {
        use std::os::unix::fs::PermissionsExt;
        let info = match fs::symlink_metadata(path) {
            Ok(info) => info,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        if info.is_dir() {
            for item in fs::read_dir(path).map_err(|e| e.to_string())? {
                visit(top, &item.map_err(|e| e.to_string())?.path(), out)?;
            }
        } else {
            let bytes = if info.file_type().is_symlink() {
                use std::os::unix::ffi::OsStrExt;
                fs::read_link(path)
                    .map_err(|e| e.to_string())?
                    .as_os_str()
                    .as_bytes()
                    .to_vec()
            } else if info.is_file() {
                fs::read(path).map_err(|e| e.to_string())?
            } else {
                return Err(format!("unsupported generated file: {}", path.display()));
            };
            let digest = sha1_smol::Sha1::from(&bytes).digest();
            out.insert(
                path.strip_prefix(top)
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into(),
                format!(
                    "{}:{}:{digest}",
                    info.file_type().is_symlink(),
                    info.permissions().mode()
                ),
            );
        }
        Ok(())
    }
    let mut out = std::collections::BTreeMap::new();
    for rule in rules {
        visit(top, &top.join(rule.trim_end_matches('/')), &mut out)?;
    }
    Ok(out)
}
fn check_generated(
    meta: &Value,
    run: &Path,
    holder: Arc<AtomicI32>,
) -> (Vec<String>, Option<String>) {
    let spec = &meta["protectGenerated"];
    if !spec.is_object() || s(meta, "mode") != "write" {
        return (vec![], None);
    }
    let result = (|| -> Res<Vec<String>> {
        let top = Path::new(s(meta, "top"));
        let rules = spec["paths"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>();
        let _slot = lane::acquire_shell("check protected generation", None, Some(run), |ahead, _| {
            log_event(run, json!({"e":"queue","ahead":ahead}));
        })?;
        if lane::stopped() {
            return Err("stopped before generation check".into());
        }
        let before = generated_manifest(top, &rules)?;
        let mut log = File::create(run.join("protect-generate.log")).map_err(|e| e.to_string())?;
        let (code, _) = run_shell(
            (s(spec, "command"), "generate"),
            top,
            run,
            &meta["env"],
            Some(seconds(&setting("GENERATE_TIMEOUT", "10m"))?),
            &mut log,
            Some(&holder),
        )?;
        if code != 0 {
            return Err(format!(
                "generated.command failed (exit {code}); see protect-generate.log"
            ));
        }
        let after = generated_manifest(top, &rules)?;
        Ok(before
            .keys()
            .chain(after.keys())
            .filter(|path| before.get(*path) != after.get(*path))
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect())
    })();
    match result {
        Ok(paths) => (paths, None),
        Err(error) => (vec![], Some(error)),
    }
}
/// The model a run asked Pi for; "default" when it left the choice to Pi.
fn model_label(meta: &Value) -> String {
    Some(s(meta, "model"))
        .filter(|m| !m.is_empty())
        .unwrap_or("default")
        .to_string()
}

/// An attempt on the session this run already has: Pi copies the saved session with `--fork`, so
/// the colleague keeps what it read and changed and only has to carry on. `None` when the attempt
/// saved no session (the agent failed before writing one) — there is nothing to continue.
fn continuation(meta: &Value, run: &Path) -> Option<Value> {
    continuation_with(meta, run, transient::PROMPT)
}

pub(crate) fn continuation_with_file(meta: &Value, run: &Path, prompt_file: &Path) -> Option<Value> {
    let id = runs::events(run)
        .into_iter()
        .rev()
        .find(|event| s(event, "e") == "session" && !s(event, "id").is_empty())
        .map(|event| s(&event, "id").to_string())?;
    let session = agents::continuation_target(meta, &id)?;
    let mut next = meta.clone();
    next["fork"] = json!(session);
    next["agentPromptFile"] = json!(prompt_file);
    // The images went out with the first message and stay in the session; attaching them to a
    // continuation would send them a second time for no reason.
    next["images"] = json!([]);
    Some(next)
}

fn continuation_with(meta: &Value, run: &Path, message: &str) -> Option<Value> {
    let prompt = run.join("resume-prompt.md");
    write(&prompt, format!("{message}\n")).ok()?;
    continuation_with_file(meta, run, &prompt)
}

/// A command that hangs (a test waiting forever, a server that never exits) is killed at the
/// command stall limit. Killing the whole colleague then left no answer at all; instead the same
/// session gets one more turn that names the command, so the colleague can find what hung.
pub(crate) fn resume_stalled(
    meta: &Value,
    run: &Path,
    first: (String, String, i64),
    holder: Arc<AtomicI32>,
    grace: Arc<Mutex<Option<f64>>>,
) -> (String, String, i64) {
    let (verdict, answer, attempts) = first;
    let reason = json(run.join("stall.json"));
    if verdict != "stalled" || s(&reason, "kind") != "command" || lane::stopped() {
        return (verdict, answer, attempts);
    }
    let minutes = (n(&reason, "seconds") + 30) / 60;
    let message = format!(
        "Delegate stopped your command after {minutes} minutes without it finishing: `{}`. \
         Do not rerun it as is. Find what hung — run the narrower parts on their own, with a \
         timeout and per-test output — fix it if your change caused it, otherwise name it in \
         your answer, then finish the task.",
        s(&reason, "cmd")
    );
    let Some(next) = continuation_with(meta, run, &message) else {
        return (verdict, answer, attempts);
    };
    log_event(
        run,
        json!({"e":"resume","n":1,"error":"command stalled","detail":format!("命令停滞，续做一次：{}", s(&reason, "cmd"))}),
    );
    let attempts = attempts + 1;
    match agents::run_agent(&next, run, attempts, holder, grace) {
        Ok((outcome, text)) => (outcome, text, attempts),
        Err(failure) => {
            let _ = append(
                run.join("stderr.log"),
                &format!("supervisor error: {failure}\n"),
            );
            (verdict, answer, attempts)
        }
    }
}

/// Outcome of checking and (at most once) recovering from a Gemini/agy content-policy refusal
/// at the final conclusion step.
pub(crate) struct PolicyRecovery {
    pub(crate) verdict: String,
    pub(crate) answer: String,
    pub(crate) attempts: i64,
    pub(crate) refused: bool,
    pub(crate) rewrite_attempted: bool,
}

fn attempt_has_policy_refusal(run: &Path, attempt: i64) -> bool {
    let evs = runs::events(run);
    let mut saw_attempt = false;
    let mut matched = false;
    for ev in &evs {
        if ev.get("attempt").and_then(Value::as_i64) == Some(attempt) {
            saw_attempt = true;
            if s(ev, "e") == "turn_error"
                && crate::agy::is_policy_refusal_error(s(ev, "detail"))
            {
                matched = true;
            }
        }
    }
    if saw_attempt {
        return matched;
    }
    evs.iter()
        .rev()
        .find(|ev| s(ev, "e") == "turn_error")
        .is_some_and(|ev| crate::agy::is_policy_refusal_error(s(ev, "detail")))
}

/// If the final conclusion was blocked by Gemini/agy content-policy filtering, append ONE short
/// rewrite prompt at the tail of the SAME session (preserving prefix cache & model binding).
/// Never loops: a second failure marks `refused = true` so callers write a readable fallback and
/// flag status as 「结论被模型服务拒绝」 instead of treating it as a normal conclusion.
pub(crate) fn recover_policy_refusal(
    meta: &Value,
    run: &Path,
    first: (String, String, i64),
    holder: Arc<AtomicI32>,
    grace: Arc<Mutex<Option<f64>>>,
) -> PolicyRecovery {
    let (verdict, answer, attempts) = first;
    if lane::stopped() {
        return PolicyRecovery {
            verdict,
            answer,
            attempts,
            refused: false,
            rewrite_attempted: false,
        };
    }
    let answer_refused = verdict == "ok" && crate::agy::is_policy_refusal(&answer);
    let turn_err_refused = verdict == "failed"
        && answer.trim().is_empty()
        && attempt_has_policy_refusal(run, attempts);
    if !answer_refused && !turn_err_refused {
        return PolicyRecovery {
            verdict,
            answer,
            attempts,
            refused: false,
            rewrite_attempted: false,
        };
    }
    let prompt_file = run.join("policy-rewrite-prompt.md");
    let next = write(&prompt_file, format!("{}\n", crate::agy::POLICY_REWRITE_PROMPT))
        .ok()
        .and_then(|()| continuation_with_file(meta, run, &prompt_file));
    let Some(next) = next else {
        log_event(
            run,
            json!({"e":"policy_rewrite_skipped","reason":"no saved session to continue"}),
        );
        return PolicyRecovery {
            verdict: if answer_refused { "ok".into() } else { verdict },
            answer: String::new(),
            attempts,
            refused: true,
            rewrite_attempted: false,
        };
    };
    log_event(
        run,
        json!({"e":"policy_rewrite","n":1,"detail":"最终结论被模型服务策略拦截，同会话尾部重写 1 次"}),
    );
    let next_attempt = attempts + 1;
    match agents::run_agent(&next, run, next_attempt, holder, grace) {
        Ok((rv, rtext))
            if rv == "ok"
                && !rtext.trim().is_empty()
                && !crate::agy::is_policy_refusal(&rtext) =>
        {
            log_event(
                run,
                json!({"e":"policy_rewrite_ok","attempt":next_attempt}),
            );
            PolicyRecovery {
                verdict: rv,
                answer: rtext,
                attempts: next_attempt,
                refused: false,
                rewrite_attempted: true,
            }
        }
        Ok((rv, _)) => {
            log_event(
                run,
                json!({"e":"policy_rewrite_failed","attempt":next_attempt,"verdict":rv}),
            );
            PolicyRecovery {
                verdict: if answer_refused { "ok".into() } else { verdict },
                answer: String::new(),
                attempts: next_attempt,
                refused: true,
                rewrite_attempted: true,
            }
        }
        Err(failure) => {
            let _ = append(
                run.join("stderr.log"),
                &format!("supervisor error: {failure}\n"),
            );
            log_event(
                run,
                json!({"e":"policy_rewrite_failed","attempt":next_attempt,"error":clip(&failure,200)}),
            );
            PolicyRecovery {
                verdict: if answer_refused { "ok".into() } else { verdict },
                answer: String::new(),
                attempts: next_attempt,
                refused: true,
                rewrite_attempted: true,
            }
        }
    }
}

fn format_policy_refused_result(
    run: &Path,
    meta: &Value,
    files: &[String],
    rewrite_attempted: bool,
) -> String {
    let run_id = crate::events::run_id_for(run, meta);
    let review_path = run.join("review.md");
    let mut lines = vec![if rewrite_attempted {
        "结论被模型服务拒绝（已在原会话尾部重写 1 次仍被内容策略拦截）；工作区改动已保留，不当作正常结论。".to_string()
    } else {
        "结论被模型服务拒绝（被内容策略拦截且无可用会话续写）；工作区改动已保留，不当作正常结论。".to_string()
    }];
    if review_path.is_file() {
        lines.push(format!("- 审查报告：{}", review_path.display()));
    } else {
        lines.push("- 审查报告：无（未生成 review.md）".to_string());
    }
    if files.is_empty() {
        lines.push(format!("- 改动文件清单：无改动（或查 `delegate files {run_id}`）"));
    } else {
        let preview = if files.len() <= 10 {
            files.join(", ")
        } else {
            format!("{} 等 {} 个文件", files[..10].join(", "), files.len())
        };
        lines.push(format!(
            "- 改动文件清单：`delegate files {run_id}`（{} 个：{preview}）",
            files.len()
        ));
    }
    format!("{}\n", lines.join("\n"))
}

/// What a run did after waiting out an upstream outage.
pub(crate) struct Resumed {
    pub(crate) verdict: String,
    pub(crate) answer: String,
    pub(crate) attempts: i64,
    pub(crate) resumes: i64,
    pub(crate) error: String,
    pub(crate) auth_failed: bool,
}

/// A provider outage is not the task failing: wait for it to pass, check through Pi that the
/// model answers again, then let the same session carry on in this run — same model, same
/// worktree, changes kept. Only when the tries run out does the run fail (and escalate) as before.
///
/// For `agy` OAuth2 401 (`Kind::AuthRefresh`), wait once and re-probe once so `agy` can refresh
/// its access token; if the re-probe or resumed attempt still fails, judge permanent failure.
pub(crate) fn resume_transient(
    meta: &Value,
    run: &Path,
    first: (String, String, i64),
    holder: Arc<AtomicI32>,
    grace: Arc<Mutex<Option<f64>>>,
) -> Resumed {
    let (mut verdict, mut answer, mut attempts) = first;
    let waits = transient::waits();
    let is_agy =
        crate::agy::is_agy(s(meta, "model")) || s(meta, "agent") == crate::agy::BIN;
    let (mut tried, mut resumes, mut error) = (0usize, 0i64, String::new());
    let mut auth_probed = false;
    let mut auth_failed = false;
    while verdict == "failed" && !lane::stopped() {
        let Some((found_kind, found)) = transient::ended_by_kind(run, attempts) else {
            break;
        };
        if found_kind == transient::Kind::AuthRefresh {
            if !is_agy {
                break;
            }
            if auth_probed || waits.is_empty() {
                auth_failed = true;
                error.clear();
                break;
            }
            auth_probed = true;
            let wait = waits[tried.min(waits.len() - 1)];
            tried += 1;
            let waiting = format!(
                "agy 401 鉴权过期，等待 {}s 后重探刷新令牌（仅 1 次）",
                (wait * 10.0).round() / 10.0
            );
            log_event(
                run,
                json!({"e":"resume","n":tried,"waitSeconds":wait,"kind":"auth_refresh","error":found,"detail":waiting}),
            );
            if !transient::wait(wait) {
                break;
            }
            let bin = Some(s(meta, "agentBin"))
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .or_else(|| agents::agent_bin(crate::agy::colleague(s(meta, "model"))));
            let down = match bin {
                Some(bin) => probe::recovered(
                    &bin,
                    s(meta, "model"),
                    &meta["env"],
                    &meta["agy"],
                    transient::probe_timeout(),
                ),
                None => Err("missing required tools: agy".to_string()),
            };
            if let Err(down) = down {
                log_event(
                    run,
                    json!({"e":"resume_skipped","n":tried,"kind":"auth_refresh","error":down}),
                );
                auth_failed = true;
                error.clear();
                break;
            }
            let next = match continuation(meta, run) {
                Some(next) => next,
                None if attempts == 1 => meta.clone(),
                None => {
                    log_event(
                        run,
                        json!({"e":"resume_skipped","n":tried,"kind":"auth_refresh","error":"no saved session to continue"}),
                    );
                    auth_failed = true;
                    error.clear();
                    break;
                }
            };
            resumes += 1;
            attempts += 1;
            match agents::run_agent(&next, run, attempts, holder.clone(), grace.clone()) {
                Ok((outcome, text)) => {
                    verdict = outcome;
                    answer = text;
                }
                Err(failure) => {
                    error = failure.clone();
                    let _ = append(
                        run.join("stderr.log"),
                        &format!("supervisor error: {failure}\n"),
                    );
                    auth_failed = true;
                    break;
                }
            }
            continue;
        }
        if tried >= waits.len() {
            break;
        }
        error = found;
        let wait = waits[tried];
        tried += 1;
        let waiting = format!(
            "上游中断，第 {tried} 次续做，等待 {}s",
            (wait * 10.0).round() / 10.0
        );
        log_event(
            run,
            json!({"e":"resume","n":tried,"waitSeconds":wait,"error":error,"detail":waiting}),
        );
        if !transient::wait(wait) {
            break;
        }
        // Ask the same executable the attempt ran on: Pi and rpi keep different sessions.
        let bin = Some(s(meta, "agentBin"))
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(|| agents::agent_bin(crate::agy::colleague(s(meta, "model"))));
        let down = match bin {
            Some(bin) => probe::recovered(
                &bin,
                s(meta, "model"),
                &meta["env"],
                &meta["agy"],
                transient::probe_timeout(),
            ),
            None => Err("missing required tools: pi".to_string()),
        };
        if let Err(down) = down {
            // Still down: sending the task would fail again, so spend no attempt on it.
            log_event(run, json!({"e":"resume_skipped","n":tried,"error":down}));
            continue;
        }
        let Some(next) = continuation(meta, run) else {
            log_event(
                run,
                json!({"e":"resume_skipped","n":tried,"error":"no saved session to continue"}),
            );
            break;
        };
        resumes += 1;
        attempts += 1;
        match agents::run_agent(&next, run, attempts, holder.clone(), grace.clone()) {
            Ok((outcome, text)) => {
                verdict = outcome;
                answer = text;
            }
            Err(failure) => {
                error = failure.clone();
                let _ = append(
                    run.join("stderr.log"),
                    &format!("supervisor error: {failure}\n"),
                );
                break;
            }
        }
    }
    if lane::stopped() {
        verdict = "stopped".into();
    }
    Resumed {
        verdict,
        answer,
        attempts,
        resumes,
        error,
        auth_failed,
    }
}
fn escalate(
    meta: &mut Value,
    run: &Path,
    state: &str,
    recorded: bool,
    changed: &[String],
    protected: bool,
) -> Res<bool> {
    if runs::agent_pinned(meta)
        || protected
        || !["malformed", "failed", "timeout", "rejected"].contains(&state)
    {
        return Ok(false);
    }
    if !changed.is_empty() || (s(meta, "mode") != "read-only" && !recorded) {
        return Ok(false);
    }
    let candidates = meta["tierCandidates"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let Some(next) = crate::models::next_candidate(
        s(meta, "model"),
        &candidates,
        meta["tierCandidateIndex"].as_u64().unwrap_or(0) as usize,
    ) else {
        return Ok(false);
    };
    if meta["images"].as_array().is_some_and(|a| !a.is_empty()) {
        let listed = agents::pi_models();
        crate::models::guard_images(s(&next, "model"), &listed)?;
    }
    meta["escalatedFrom"] = json!(model_label(meta));
    meta["model"] = next["model"].clone();
    meta["modelName"] = next["name"].clone();
    meta["tierCandidateIndex"] = next["index"].clone();
    meta["thinking"] = next["thinking"].clone();
    meta["fork"] = Value::Null;
    // The next model may run on the other colleague (Pi or agy); a fresh session there.
    let colleague = crate::agy::colleague(s(meta, "model"));
    if s(meta, "agent") != colleague {
        let (bin, version) = agents::agent_identity(colleague)?;
        meta["agent"] = json!(colleague);
        meta["agentBin"] = json!(bin);
        meta["agentVersion"] = json!(version);
    }
    write_json(run.join("meta.json"), meta)?;
    Ok(true)
}
fn append_prepare_notice(run: &Path, line: &str) {
    let _ = append(run.join("prepare-notices.txt"), &format!("{line}\n"));
}

fn prepare_run_in_supervisor(run: &Path, meta: &mut Value) -> Res<()> {
    // 1. Deferred fork session copy (moved out of foreground `reply`).
    if meta["pendingFork"].is_object() {
        let source = PathBuf::from(s(&meta["pendingFork"], "source"));
        let target = PathBuf::from(s(&meta["pendingFork"], "target"));
        if !source.is_file() {
            return Err("missing parent session".into());
        }
        if let Some(dir) = target.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        fs::copy(&source, &target).map_err(|e| e.to_string())?;
        let beside = |path: &Path| {
            let mut name = path.as_os_str().to_owned();
            name.push(".progress.jsonl");
            PathBuf::from(name)
        };
        if beside(&source).is_file() {
            fs::copy(beside(&source), beside(&target)).map_err(|e| e.to_string())?;
        }
        if let Some(obj) = meta.as_object_mut() {
            obj.remove("pendingFork");
        }
    }

    // 2. Deferred `reply` Auto / Agent baseline sync (moved out of foreground `reply`).
    if meta["pendingSync"].is_object() {
        let mode_str = s(&meta["pendingSync"], "mode").to_string();
        let parent_dir = PathBuf::from(s(&meta["pendingSync"], "parentDir"));
        let mode = match mode_str.as_str() {
            "agent" => worktree::SyncMode::Agent,
            "strict" => worktree::SyncMode::Strict,
            _ => worktree::SyncMode::Auto,
        };
        let parent_meta = json(parent_dir.join("meta.json"));
        let report = worktree::sync_with_mode(&parent_dir, &parent_meta, mode)?;
        let prompt_path = run.join("prompt.md");
        let mut prompt_text = read(&prompt_path);
        let is_zh = prompt_text
            .chars()
            .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
        let short_head = if !report.source_dirty && !report.base_head.is_empty() {
            Some(&report.base_head[..report.base_head.len().min(7)])
        } else {
            None
        };
        if report.synced {
            let notice = if let Some(short) = short_head {
                format!(
                    "delegate: 基线已同步到主干 {short}（同步 {} 个文件）",
                    report.paths.len()
                )
            } else {
                format!(
                    "delegate: 基线已同步到当前源快照（同步 {} 个文件）",
                    report.paths.len()
                )
            };
            append_prepare_notice(run, &notice);
            if mode == worktree::SyncMode::Auto {
                let fact = match (is_zh, short_head) {
                    (true, Some(short)) => format!(
                        "基线已同步到主干 {short}（同步 {} 个文件）。",
                        report.paths.len()
                    ),
                    (true, None) => format!(
                        "基线已同步到当前源快照（同步 {} 个文件）。",
                        report.paths.len()
                    ),
                    (false, Some(short)) => format!(
                        "Baseline synced to main {short} ({} file(s) updated).",
                        report.paths.len()
                    ),
                    (false, None) => format!(
                        "Baseline synced to current source snapshot ({} file(s) updated).",
                        report.paths.len()
                    ),
                };
                prompt_text = format!("{}\n\n{fact}\n", prompt_text.trim_end());
            } else if is_zh {
                let base_note = short_head
                    .map(|short| format!("；基线已同步到主干 {short}"))
                    .unwrap_or_default();
                prompt_text = format!(
                    "{}\n\n主控在上一轮之后的改动已同步进工作目录：{}{base_note}\n",
                    prompt_text.trim_end(),
                    if report.paths.is_empty() {
                        "（无）".to_string()
                    } else {
                        report.paths.join("、")
                    }
                );
            } else {
                let base_note = short_head
                    .map(|short| format!(" (baseline synced to main {short})"))
                    .unwrap_or_default();
                prompt_text = format!(
                    "{}\n\nThe caller's changes since the previous round have been synced into the workdir: {}{base_note}\n",
                    prompt_text.trim_end(),
                    if report.paths.is_empty() {
                        "(none)".to_string()
                    } else {
                        report.paths.join(", ")
                    }
                );
            }
            if !report.conflicts.is_empty() {
                prompt_text = format!(
                    "Resolve these sync conflicts FIRST before any other work: {}. Remove all conflict markers; acceptance fails while any remain.\n\n{}",
                    report.conflicts.join(", "),
                    prompt_text
                );
            }
            write(&prompt_path, &prompt_text)?;

            let sync_doc = json!({
                "paths": report.paths,
                "conflicts": report.conflicts,
                "mode": mode_str,
                "baseHead": report.base_head,
            });
            write_json(run.join("sync.json"), &sync_doc)?;
            if !report.base_tree.is_empty() {
                meta["chainBase"] = json!(report.base_tree);
            }
            if !report.base_head.is_empty() {
                meta["netBase"] = json!(report.base_head);
            }
            if let Some(commit) = &report.base_commit {
                if !commit.is_empty() {
                    write(run.join("worktree-base-commit"), commit)?;
                    crate::refs::pin_slot(run, meta, "worktree-base", commit);
                }
            }
            let synced_base = json(parent_dir.join(".sync-base"));
            if !s(&synced_base, "tree").is_empty() {
                meta["appliedBase"] = synced_base;
            }
            let refreshed_parent = json(parent_dir.join("meta.json"));
            if b(&refreshed_parent, "netBaseRebased") {
                meta["netBaseRebased"] = json!(true);
                if !s(&refreshed_parent, "netBaseTree").is_empty() {
                    meta["netBaseTree"] = refreshed_parent["netBaseTree"].clone();
                }
            }
        } else if mode == worktree::SyncMode::Auto && !report.conflicts.is_empty() {
            append_prepare_notice(
                run,
                &format!(
                    "delegate: 主干改动与本链存在冲突，保持旧基线继续；重叠文件：{}（可用 --no-sync 跳过同步，或用 --sync=agent 写入冲突标记交同事解决）",
                    report.conflicts.join("、")
                ),
            );
        } else if mode != worktree::SyncMode::Auto {
            if is_zh {
                prompt_text = format!(
                    "{}\n\n主控在上一轮之后的改动已同步进工作目录：（无）\n",
                    prompt_text.trim_end()
                );
            } else {
                prompt_text = format!(
                    "{}\n\nThe caller's changes since the previous round have been synced into the workdir: (none)\n",
                    prompt_text.trim_end()
                );
            }
            write(&prompt_path, &prompt_text)?;
        }
        if let Some(obj) = meta.as_object_mut() {
            obj.remove("pendingSync");
        }
    }

    // 3. Deferred `numbers_taken`, `changes::snapshot`, `source_watch::baseline`, `refs::pin_run`.
    if b(meta, "pendingPrepare") {
        if cfg!(debug_assertions) && std::env::var("DELEGATE_TEST_FAIL_PREPARE").as_deref() == Ok("1") {
            return Err("injected prepare failure".into());
        }
        let mode = s(meta, "mode").to_string();
        if mode != "read-only" && meta["worktree"].is_object() {
            let taken = crate::worktree::numbers_taken(
                Path::new(s(&meta["worktree"], "source")),
                Path::new(s(&meta["worktree"], "path")),
            );
            if !taken.is_empty() {
                let list = taken
                    .iter()
                    .map(|(directory, prefix, owner)| format!("{directory} {prefix} ({owner})"))
                    .collect::<Vec<_>>()
                    .join("、");
                let prompt_path = run.join("prompt.md");
                let mut text = read(&prompt_path);
                text.push_str(&format!(
                    "\n\n未提交的编号文件已占用这些编号，你的工作区里看不到；在同目录新增编号文件时用更大的编号 / Numbered files not yet committed hold these numbers and are not in your worktree; number new files in those directories above them: {list}\n"
                ));
                write(&prompt_path, &text)?;
                append_prepare_notice(run, &format!("delegate: 已占用编号 {list}"));
            }
        }

        if meta["worktree"]["in"].is_null() {
            let run_id = run.file_name().unwrap_or_default().to_string_lossy();
            let claimed_slot = b(&meta["worktree"], "pooled")
                && s(&meta["worktree"], "claimedBy") == run_id;
            let wt_path = PathBuf::from(s(&meta["worktree"], "path"));
            let new_tree = meta["worktree"].is_object()
                && (claimed_slot || !wt_path.exists());
            let snap_top = if meta["worktree"].is_object() {
                if wt_path.exists() && !claimed_slot {
                    Some(wt_path.clone())
                } else {
                    Some(PathBuf::from(s(&meta["worktree"], "source")))
                }
            } else {
                git_top_fast(Path::new(s(meta, "workdir")))
            };
            let exclude = strings(&meta["snapshotExclude"]);
            let mut base = snap_top
                .as_deref()
                .and_then(|p| changes::snapshot(p, run, &exclude));
            if meta["worktree"].is_object() && base.is_none() {
                return Err(format!(
                    "cannot snapshot {} for the worktree",
                    s(&meta["worktree"], "source")
                ));
            }
            if new_tree {
                if let Some(v) = &mut base {
                    v["large"] = json!({});
                    v["submodules"] = json!({});
                }
            }
            if s(meta, "chainBase").is_empty() {
                if let Some(tree_sha) = base.as_ref().map(|x| s(x, "tree")).filter(|x| !x.is_empty()) {
                    meta["chainBase"] = json!(tree_sha);
                }
            }
            meta["base"] = base.unwrap_or(Value::Null);
            if new_tree {
                meta["top"] = json!(wt_path);
            } else if let Some(st) = snap_top {
                if meta["top"].is_null() {
                    meta["top"] = json!(st);
                }
            }
        }

        if meta["worktree"].is_object() {
            let source = PathBuf::from(s(&meta["worktree"], "source"));
            meta["sourceStatusBaseline"] = crate::source_watch::baseline(&source)?;
            if s(meta, "netBase").is_empty() {
                if let Ok(head) = git_text(&source, &["rev-parse", "-q", "--verify", "HEAD^{commit}"]) {
                    if !head.is_empty() {
                        meta["netBase"] = json!(head);
                    }
                }
            }
        }

        meta["phase"] = json!("running");
        if let Some(obj) = meta.as_object_mut() {
            obj.remove("pendingPrepare");
            obj.remove("newTree");
        }
        write_json(run.join("meta.json"), meta)?;
        if run.join("worktree-base-commit").is_file() {
            let commit = read(run.join("worktree-base-commit"));
            let commit = commit.trim();
            if !commit.is_empty() {
                crate::refs::pin_slot(run, meta, "worktree-base", commit);
            }
        }
        crate::refs::pin_run(run, meta);
        crate::index::upsert_run(run);
        crate::board::refresh_for_run(run);
        runs::prune();
    }
    Ok(())
}

fn inner(run: &Path, holder: Arc<AtomicI32>, grace: Arc<Mutex<Option<f64>>>) -> Res<()> {
    let mut meta = json(run.join("meta.json"));
    if !s(&meta, "after").is_empty() {
        return finish_run(
            run,
            json!({"state":"needs-redispatch","error":"--after was removed in 6.22.0; re-dispatch this run"}),
            1,
        );
    }
    if run.join(".waiting").exists() {
        if !run.join("queue.json").is_file() {
            crate::queue::enter(run, &mut meta)?;
        }
        crate::board::refresh_for_run(run);
        if !admit_waiting(run, &mut meta)? {
            return finish_run(run, json!({"state":"stopped"}), 1);
        }
    }
    prepare_run_in_supervisor(run, &mut meta)?;
    if meta["worktree"]["in"].is_string() {
        let reconstructed_tree = s(&meta["worktree"], "reconstructedTree").to_string();
        if !reconstructed_tree.is_empty() {
            let reconstructed_base = s(&meta["worktree"], "reconstructedBase").to_string();
            let mut base = json!({
                "tree": reconstructed_tree,
                "large": {},
                "submodules": {},
            });
            if !reconstructed_base.is_empty() {
                base["head"] = json!(reconstructed_base);
            }
            meta["chainBase"] = json!(reconstructed_tree);
            meta["base"] = base;
            write_json(run.join("meta.json"), &meta)?;
            crate::refs::pin_run(run, &meta);
        } else {
            let in_run = Path::new(s(&meta["worktree"], "in"));
            let source = Path::new(s(&meta["worktree"], "source"));
            if !source.is_dir() || !source.join(".git").exists() {
                return Err(format!(
                    "--in upstream worktree vanished before snapshot ({}); refusing to fall back to trunk",
                    source.display()
                ));
            }
            let upstream = json(in_run.join("meta.json"));
            let exclude = upstream["snapshotExclude"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let base =
                changes::snapshot(source, run, &exclude).ok_or("cannot snapshot upstream worktree")?;
            meta["chainBase"] = base["tree"].clone();
            meta["base"] = base;
            write_json(run.join("meta.json"), &meta)?;
            crate::refs::pin_run(run, &meta);
        }
    }
    let started = epoch();
    let mut setup_error = None;
    let claimed_slot = b(&meta["worktree"], "pooled")
        && s(&meta["worktree"], "claimedBy")
            == run.file_name().unwrap_or_default().to_string_lossy();
    if meta["worktree"].is_object()
        && (claimed_slot || !Path::new(s(&meta["worktree"], "path")).exists())
    {
        setup_error = match worktree::prepare(&meta, run) {
            Ok(error) => error,
            Err(e) => Some(clip(&format!("worktree setup failed: {e}"), 300)),
        };
    }
    if setup_error.is_none() {
        crate::deny::prepare(&meta, run)?;
    }
    let (mut verdict, mut answer, mut attempts) = if setup_error.is_none() {
        attempts_from(&meta, run, 1, holder.clone(), grace.clone())
    } else {
        ("failed".into(), String::new(), 0)
    };
    let (mut resumes, mut transient_error, mut auth_failed) = (0i64, String::new(), false);
    let (mut policy_refused, mut policy_rewrite_attempted) = (false, false);
    if setup_error.is_none() {
        let resumed = resume_transient(
            &meta,
            run,
            (verdict, answer, attempts),
            holder.clone(),
            grace.clone(),
        );
        (verdict, answer, attempts) = (resumed.verdict, resumed.answer, resumed.attempts);
        (resumes, transient_error, auth_failed) =
            (resumed.resumes, resumed.error, resumed.auth_failed);
        (verdict, answer, attempts) = resume_stalled(
            &meta,
            run,
            (verdict, answer, attempts),
            holder.clone(),
            grace.clone(),
        );
        let recovered = recover_policy_refusal(
            &meta,
            run,
            (verdict, answer, attempts),
            holder.clone(),
            grace.clone(),
        );
        (verdict, answer, attempts) = (recovered.verdict, recovered.answer, recovered.attempts);
        policy_refused = recovered.refused;
        policy_rewrite_attempted = recovered.rewrite_attempted;
    }
    let mut pre = settle_pre_accept(
        &mut meta,
        run,
        &verdict,
        &setup_error,
        holder.clone(),
        verdict == "ok" && agents::is_question(&answer),
    );
    if !auth_failed && transient_error.is_empty() && verdict == "failed" {
        if let Some((kind, msg)) = transient::ended_by_kind(run, attempts) {
            if kind != transient::Kind::AuthRefresh {
                transient_error = msg;
            }
        }
    }
    let original_model = model_label(&meta);
    let protected = !pre.sum["protectViolation"].is_null();
    if !lane::stopped()
        && pre.sum["question"].is_null()
        && !transient_error.is_empty()
        && escalate(&mut meta, run, &pre.state, pre.rec.is_some(), &pre.changed, protected)?
    {
        log_event(
            run,
            json!({"e":"escalate","from":original_model,"to":model_label(&meta),"after":pre.state}),
        );
        (verdict, answer, attempts) =
            attempts_from(&meta, run, attempts + 1, holder.clone(), grace.clone());
        pre = settle_pre_accept(
            &mut meta,
            run,
            &verdict,
            &setup_error,
            holder.clone(),
            verdict == "ok" && agents::is_question(&answer),
        );
        pre.sum["escalatedFrom"] = json!(original_model);
    }
    let PreAccept {
        mut state,
        mut sum,
        mut rec,
        mut changed,
        can_accept,
    } = pre;
    sum["attempts"] = json!(attempts);
    if resumes > 0 {
        sum["resumes"] = json!(resumes);
    }
    if !transient_error.is_empty() {
        sum["transientError"] = json!(clip(&transient_error, 300));
    }
    if !policy_refused && !answer.trim().is_empty() {
        write(
            run.join("result.md"),
            format!("{}\n", answer.trim_end_matches('\n')),
        )?;
    }
    crate::review::run_accept_and_review(
        &mut meta,
        run,
        can_accept,
        &mut state,
        &mut sum,
        &mut rec,
        &mut changed,
        &mut answer,
        &mut attempts,
        &mut policy_refused,
        &mut policy_rewrite_attempted,
        holder.clone(),
        grace.clone(),
    )?;
    finalize_post_accept(&meta, run, &mut state, &mut sum);
    sum["attempts"] = json!(attempts);
    if resumes > 0 {
        sum["resumes"] = json!(resumes);
    }
    if !transient_error.is_empty() {
        sum["transientError"] = json!(clip(&transient_error, 300));
    }
    if !policy_refused && !answer.trim().is_empty() {
        write(
            run.join("result.md"),
            format!("{}\n", answer.trim_end_matches('\n')),
        )?;
    }
    if ["delivered", "answered"].contains(&state.as_str())
        && !s(&meta, "evidenceCommand").is_empty()
    {
        let result = evidence(&meta, run, holder.clone());
        sum["evidence"] = result.clone();
        meta["evidence"] = result;
        let _ = write_json(run.join("meta.json"), &meta);
    }
    let evs = runs::events(run);
    let turns = evs
        .iter()
        .filter(|x| s(x, "e") == "turn")
        .collect::<Vec<_>>();
    let mut files = if rec.is_some() {
        changed
    } else {
        evs.iter()
            .filter(|e| ["edit", "write"].contains(&s(e, "e")) && !s(e, "path").is_empty())
            .map(|e| {
                repository_relative(
                    s(e, "path"),
                    if s(&meta, "top").is_empty() {
                        s(&meta, "workdir")
                    } else {
                        s(&meta, "top")
                    },
                )
            })
            .collect::<Vec<_>>()
    };
    files.sort();
    files.dedup();
    if policy_refused {
        sum["policyRefused"] = json!(true);
        if policy_rewrite_attempted {
            sum["policyRewriteAttempted"] = json!(true);
        }
        let fallback = format_policy_refused_result(run, &meta, &files, policy_rewrite_attempted);
        write(run.join("result.md"), &fallback)?;
    } else if policy_rewrite_attempted {
        sum["policyRewriteAttempted"] = json!(true);
    }
    let sessions = evs
        .iter()
        .filter(|x| s(x, "e") == "session" && !s(x, "id").is_empty())
        .collect::<Vec<_>>();
    let session_id = sessions
        .last()
        .map(|last| s(last, "id").to_string())
        .unwrap_or_default();
    if !session_id.is_empty() {
        sum["session"] = json!(&session_id);
    }
    let (tokens, tokens_delta) = compute_tokens(run, &meta, &turns, &session_id);
    sum["elapsedSeconds"] = json!((epoch() - started) as i64);
    sum["model"] = turns
        .last()
        .map(|x| x["model"].clone())
        .unwrap_or(Value::Null);
    sum["turns"] = json!(turns.len());
    sum["files"] = json!(files);
    sum["tokens"] = tokens;
    if let Some(delta) = tokens_delta {
        sum["tokensDelta"] = delta;
    }
    crate::usage::summarize_run_into_summary(run, &meta, &mut sum);
    if lane::queued_seconds(run) >= 1.0 {
        sum["queuedSeconds"] = json!(lane::queued_seconds(run) as i64);
    }
    if let Ok(x) = grace.lock() {
        if let Some(g) = *x {
            sum["graceSeconds"] = json!(g);
        }
    }
    if !["delivered", "answered", "rejected", "stopped", "blocked_mainline"].contains(&state.as_str()) {
        let errors = evs
            .iter()
            .filter(|e| ["turn_error", "tool_error"].contains(&s(e, "e")))
            .map(|e| s(e, "detail"))
            .collect::<Vec<_>>();
        let stderr = read(run.join("stderr.log"));
        let tail = stderr
            .trim()
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("; ");
        let hint = if auth_failed && state == "failed" {
            "agy 登录失效，`delegate agy account status`".into()
        } else {
            match state.as_str() {
                "malformed" => "answer was empty or a leaked tool call".into(),
                "timeout" => format!("{} exceeded {}", s(&meta, "agent"), s(&meta, "timeout")),
                "stalled" => {
                    let reason = json(run.join("stall.json"));
                    if s(&reason, "kind") == "command" {
                        format!(
                            "命令 {} 连续 {} 秒无新动作而停滞",
                            s(&reason, "cmd"),
                            n(&reason, "seconds")
                        )
                    } else {
                        format!("idle：已 {} 秒无新动作而停滞", n(&reason, "seconds"))
                    }
                }
                _ => String::new(),
            }
        };
        let message = [
            setup_error.unwrap_or(hint),
            errors.last().unwrap_or(&"").to_string(),
            tail,
        ]
        .into_iter()
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
        if !message.is_empty() {
            sum["error"] = json!(clip(&message, 600));
        }
    }
    let pgid = read(run.join("agent.pid")).trim().parse().unwrap_or(0);
    if b(&sum, "question") {
        crate::lane::save_priority(run)?;
    }
    crate::cleanup::record(run, pgid);
    if pgid > 0 && group_members(pgid) {
        end_group(pgid, 2.0);
    }
    if let Some(cleanup) = crate::cleanup::summary(run) {
        sum["cleanup"] = cleanup;
    }
    let warnings = json(run.join("warnings.json"));
    if warnings.is_array() && !warnings.as_array().unwrap().is_empty() {
        sum["warnings"] = warnings;
    }
    worktree::capture_final_entries(&meta, run);
    crate::card::generate_and_save(run, &meta, &mut sum);
    finish_run(
        run,
        sum,
        if ["delivered", "answered"].contains(&state.as_str()) {
            0
        } else {
            1
        },
    )?;
    Ok(())
}
fn admit_waiting(run: &Path, meta: &mut Value) -> Res<bool> {
    let slots = state_dir();
    loop {
        let state_guard = crate::locks::acquire_state(&slots).map_err(|e| e.to_string())?;
        if lane::stopped() {
            return Ok(false);
        }
        let others = launch::active_machine(&slots);
        if let Some(reason) = crate::queue::reason(meta, &others)? {
            crate::queue::update(run, &reason, crate::queue::ahead(meta))?;
        } else {
            if s(meta, "mode") == "write" && !b(meta, "parallel") && !meta["worktree"].is_object() {
                for other in &others {
                    let m = json(other.join("meta.json"));
                    if s(&m, "mode") == "write" && s(&m, "workdir") == s(meta, "workdir") {
                        return Err(format!(
                            "write run {} is still active in {}",
                            other.display(),
                            s(meta, "workdir")
                        ));
                    }
                }
            }
            meta["startedEpoch"] = json!(epoch() as i64);
            meta["startedAt"] = json!(iso());
            write_json(run.join("meta.json"), meta)?;
            fs::remove_file(run.join(".waiting")).map_err(|e| e.to_string())?;
            drop(state_guard);
            // Claim worktree pool slot outside `.state.lock` under `PoolClaim`.
            let origin = PathBuf::from(s(meta, "workdir"));
            let mut wd = origin.clone();
            let mut tree = meta["worktree"].clone();
            launch::allocate_tree(&mut tree, &mut wd, &origin, run, s(meta, "mode"));
            if tree.is_object() && s(&meta["worktree"], "path").is_empty() {
                meta["top"] = tree["path"].clone();
            }
            meta["workdir"] = json!(wd);
            meta["worktree"] = tree;
            write_json(run.join("meta.json"), meta)?;
            crate::events::emit(
                &crate::events::run_id_for(run, meta),
                &crate::events::repo_for_meta(meta),
                "started",
                json!({
                    "name": s(meta, "name"),
                    "mode": s(meta, "mode"),
                    "tier": meta["tier"],
                    "model": meta["model"],
                    "parent": meta["parent"],
                    "fromQueue": true,
                }),
            );
            crate::index::upsert_run(run);
            crate::board::refresh_for_run(run);
            return Ok(true);
        }
        drop(state_guard);
        // Poll also when memory is the only blocker, or no run is active.
        std::thread::sleep(Duration::from_millis(100));
    }
}
pub fn supervise(run: &Path) -> Res<()> {
    if cfg!(debug_assertions) {
        if let Ok(delay) = std::env::var("DELEGATE_TEST_SUPERVISOR_DELAY") {
            let delay = seconds(&delay)?;
            write(run.join("startup.pid"), std::process::id().to_string())?;
            std::thread::sleep(Duration::from_secs_f64(delay));
        }
    }
    crate::cleanup::probe_systemd();
    let _life = locked_file(
        &run.join("supervisor.lock"),
        &std::process::id().to_string(),
    )?;
    write(run.join("pid"), std::process::id().to_string())?;
    let mut signals = lane::supervisor_signal_pipe().map_err(|e| e.to_string())?;
    let holder = Arc::new(AtomicI32::new(0));
    let grace = Arc::new(Mutex::new(None));
    let h = holder.clone();
    let runpath = run.to_path_buf();
    std::thread::spawn(move || {
        let mut byte = [0];
        if signals.read_exact(&mut byte).is_ok() {
            lane::wake_lane_waiter_after_stop();
            let pid = h.load(Ordering::SeqCst);
            if pid > 0 {
                crate::cleanup::record(&runpath, pid);
                kill_group(pid, 5.0);
            }
            kill_extra_stop_holders(&runpath);
        }
    });
    if let Err(e) = inner(run, holder, grace) {
        let _ = append(run.join("stderr.log"), &format!("supervisor error: {e}\n"));
        if !run.join("exit_code").exists() {
            let pgid = read(run.join("agent.pid")).trim().parse().unwrap_or(0);
            crate::cleanup::record(run, pgid);
            if pgid > 0 && group_members(pgid) {
                end_group(pgid, 2.0);
            }
            let mut summary = json!({"state":"failed","error":clip(&e,600)});
            if let Some(cleanup) = crate::cleanup::summary(run) {
                summary["cleanup"] = cleanup;
            }
            let warnings = json(run.join("warnings.json"));
            if warnings.as_array().is_some_and(|items| !items.is_empty()) {
                summary["warnings"] = warnings;
            }
            let _ = finish_run(run, summary, 1);
        }
    }
    Ok(())
}

/// Compute `summary.json["tokens"]` and (for `agy` reply runs continuing the same session)
/// `summary.json["tokensDelta"]`.
///
/// Pi reports per-turn delta usage on each `turn` event, so turns are summed.
/// `agy` reports session-cumulative usage on every `turn` event, so we take the last turn
/// to avoid double-counting across rework/resume turns, and subtract the parent run's last
/// turn when a `reply` child continues the same `session` ID.
fn compute_tokens(
    run: &Path,
    meta: &Value,
    turns: &[&Value],
    session_id: &str,
) -> (Value, Option<Value>) {
    let is_agy = crate::agy::is_agy(s(meta, "model"))
        || turns
            .last()
            .is_some_and(|x| s(x, "provider") == crate::agy::BIN);
    let mut tokens = json!({});
    if is_agy {
        let last_usage = turns.last().map(|x| &x["usage"]);
        for key in ["input", "output", "cacheRead"] {
            tokens[key] = json!(last_usage.map_or(0, |u| n(u, key)));
        }
    } else {
        for key in ["input", "output", "cacheRead"] {
            tokens[key] = json!(turns.iter().map(|x| n(&x["usage"], key)).sum::<i64>());
        }
    }

    let delta = if is_agy {
        crate::usage::ancestor_session_usage(run, meta, session_id).map(|pu| {
            crate::usage::TokenCounts::from_json(&tokens)
                .sub_clamped(pu)
                .to_json()
        })
    } else {
        None
    };
    (tokens, delta)
}

#[cfg(test)]
mod token_tests {
    use super::*;

    #[test]
    fn agy_takes_last_turn_cumulative_and_pi_sums_turns() {
        let dir = std::env::temp_dir().join(format!(
            "delegate-tokens-test-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&dir).unwrap();
        let t1 = json!({"e":"turn","provider":"agy","usage":{"input":100,"output":20,"cacheRead":47_570_000}});
        let t2 = json!({"e":"turn","provider":"agy","usage":{"input":130,"output":35,"cacheRead":56_680_000}});
        let turns = vec![&t1, &t2];

        let agy_meta = json!({"model":"agy/gemini-4-neon"});
        let (tok, delta) = compute_tokens(&dir, &agy_meta, &turns, "conv-1");
        assert_eq!(tok, json!({"input": 130, "output": 35, "cacheRead": 56_680_000}));
        assert!(delta.is_none());

        let pi_t1 = json!({"e":"turn","provider":"anthropic","usage":{"input":100,"output":20,"cacheRead":500}});
        let pi_t2 = json!({"e":"turn","provider":"anthropic","usage":{"input":80,"output":15,"cacheRead":700}});
        let pi_meta = json!({"model":"anthropic/claude"});
        let (pi_tok, pi_delta) = compute_tokens(&dir, &pi_meta, &[&pi_t1, &pi_t2], "s-1");
        assert_eq!(pi_tok, json!({"input": 180, "output": 35, "cacheRead": 1200}));
        assert!(pi_delta.is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn agy_reply_records_tokens_delta_for_same_session_only() {
        let base = std::env::temp_dir().join(format!(
            "delegate-tokens-delta-{}-{}",
            std::process::id(),
            now_ns()
        ));
        let parent = base.join("run-parent");
        let child = base.join("run-child");
        fs::create_dir_all(&parent).unwrap();
        fs::create_dir_all(&child).unwrap();
        write_json(
            parent.join("summary.json"),
            &json!({"session":"conv-42","tokens":{"input":999,"output":999,"cacheRead":999}}),
        )
        .unwrap();
        // Parent events.jsonl has two cumulative turns; child must subtract parent's LAST turn.
        write(
            parent.join("events.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"e":"turn","provider":"agy","usage":{"input":100,"output":10,"cacheRead":1000}}),
                json!({"e":"turn","provider":"agy","usage":{"input":250,"output":40,"cacheRead":5000}})
            ),
        )
        .unwrap();

        let ct = json!({"e":"turn","provider":"agy","usage":{"input":310,"output":65,"cacheRead":8200}});
        let child_meta = json!({"model":"agy/gemini-4-neon","parent":"run-parent"});
        let (tok, delta) = compute_tokens(&child, &child_meta, &[&ct], "conv-42");
        assert_eq!(tok, json!({"input": 310, "output": 65, "cacheRead": 8200}));
        assert_eq!(
            delta,
            Some(json!({"input": 60, "output": 25, "cacheRead": 3200}))
        );

        // Fresh session (`reply --fresh`) -> different session id -> no subtraction delta.
        let (_, fresh_delta) = compute_tokens(&child, &child_meta, &[&ct], "conv-99");
        assert!(fresh_delta.is_none());

        let _ = fs::remove_dir_all(&base);
    }
}
