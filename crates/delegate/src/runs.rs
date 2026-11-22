use crate::common::*;
use crate::worktree;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub fn all_runs() -> Vec<PathBuf> {
    let mut roots = vec![runs_root()];
    if let Some(old) = legacy_runs_root() {
        roots.push(old);
    }
    let mut v = roots
        .into_iter()
        .filter_map(|root| fs::read_dir(root).ok())
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("meta.json").is_file())
        .collect::<Vec<_>>();
    v.sort_by_key(|p| json(p.join("meta.json"))["startedNs"].as_u64().unwrap_or(0));
    v
}
pub fn archived_runs() -> Vec<PathBuf> {
    let roots = all_runs_roots();
    let mut runs = roots.iter()
        .cloned()
        .flat_map(|root| {
            fs::read_dir(root.join(".applied"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
        })
        .filter(|p| p.join("meta.json").is_file())
        .collect::<Vec<_>>();
    let existing = all_runs()
        .into_iter()
        .chain(runs.iter().cloned())
        .map(|p| s(&json(p.join("meta.json")), "run").to_string())
        .collect::<std::collections::BTreeSet<_>>();
    runs.extend(
        fs::read_dir(cache_dir().join("archive"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path().join("record"))
            .filter(|p| {
                let meta = json(p.join("meta.json"));
                p.join("meta.json").is_file()
                    && Path::new(s(&meta, "dir")).parent().is_some_and(|root| roots.iter().any(|r| r == root))
                    && !existing.contains(s(&meta, "run"))
            }),
    );
    runs
}
fn all_runs_roots() -> Vec<PathBuf> {
    let mut roots = vec![runs_root()];
    if let Some(old) = legacy_runs_root() {
        roots.push(old);
    }
    roots
}
pub fn resolve(reference: &str) -> Res<PathBuf> {
    let path = Path::new(reference);
    if path.join("meta.json").is_file() {
        let resolved = path.canonicalize().map_err(|e| e.to_string())?;
        crate::refs::backfill_one(&resolved);
        return Ok(resolved);
    }
    let root = runs_root();
    let runs = all_runs();
    if reference == "last" {
        if let Some(run) = runs.last().cloned() {
            return Ok(run);
        }
        if let Some(run) = archived_runs()
            .into_iter()
            .max_by_key(|p| json(p.join("meta.json"))["startedNs"].as_u64().unwrap_or(0))
        {
            return Ok(run);
        }
        return Err(format!("no runs under {}", root.display()));
    }
    if let Some(exact) = runs
        .iter()
        .find(|p| p.file_name().is_some_and(|name| name == reference))
    {
        return Ok(exact.clone());
    }
    let archive = runs_root().join(".applied").join(reference);
    if archive.join("meta.json").is_file() {
        return Ok(archive);
    }
    let evidence = cache_dir().join("archive").join(reference).join("record");
    if evidence.join("meta.json").is_file() {
        return Ok(evidence);
    }
    let archived = archived_runs();
    if reference != "last" {
        let named = archived
            .iter()
            .filter(|p| s(&json(p.join("meta.json")), "name") == reference)
            .collect::<Vec<_>>();
        if named.len() == 1 {
            return Ok(named[0].clone());
        }
        let matches = archived
            .iter()
            .filter(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .contains(reference)
            })
            .collect::<Vec<_>>();
        if matches.len() == 1 {
            return Ok(matches[0].clone());
        }
    }
    // An active run started from another repository is still reachable by its id.
    if let Some(active) = crate::launch::machine_runs(&crate::common::state_dir())
        .into_iter()
        .find(|p| p.file_name().is_some_and(|name| name == reference))
    {
        return Ok(active);
    }
    let named = runs
        .iter()
        .filter(|p| s(&json(p.join("meta.json")), "name") == reference)
        .collect::<Vec<_>>();
    if named.len() == 1 {
        return Ok(named[0].clone());
    }
    let matches = runs
        .iter()
        .filter(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .contains(reference)
        })
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(format!("run '{reference}' matched {} runs under {} (runs live under the git root of the directory where start ran; pass a run directory or set DELEGATE_RUNS)",matches.len(),root.display()));
    }
    Ok(matches[0].clone())
}
/// Like `resolve`, but a conversation name follows its reply chain to the newest run.
pub fn resolve_head(reference: &str) -> Res<PathBuf> {
    let run = resolve(reference)?;
    if s(&json(run.join("meta.json")), "name") == reference {
        return Ok(latest(run));
    }
    Ok(run)
}
pub fn supervisor_alive(run: &Path) -> bool {
    let pid = read(run.join("pid")).trim().parse::<i32>().unwrap_or(0);
    pid_alive(pid)
        && fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|x| x.windows(10).any(|w| w == b"_supervise"))
}
pub fn agent_alive(run: &Path) -> bool {
    let pid = read(run.join("agent.pid"))
        .trim()
        .parse::<i32>()
        .unwrap_or(0);
    pid_alive(pid) && unsafe { libc::getpgid(pid) == pid }
}
pub fn legacy_after_waiting(run: &Path) -> bool {
    if run.join("exit_code").is_file() || !run.join(".waiting").exists() {
        return false;
    }
    let meta = json(run.join("meta.json"));
    if s(&meta, "after").is_empty() {
        return false;
    }
    !run.join("queue.json").is_file()
        || s(&json(run.join("queue.json")), "reason").contains("upstream")
}

pub fn redispatch_command(run: &Path, meta: &Value) -> String {
    let mut parts = vec![script().to_string_lossy().into_owned(), "start".into()];
    let mode = s(meta, "mode");
    if mode == "read-only" {
        parts.push("--read-only".into());
        if b(meta, "shell") {
            parts.push("--shell".into());
        }
        if meta["worktree"].is_null() {
            parts.push("--in-place".into());
        }
    } else if meta["worktree"].is_object() {
        parts.push("--worktree".into());
    } else {
        parts.push("--in-place".into());
    }
    let in_run = s(&meta["worktree"], "in");
    if !in_run.is_empty() {
        parts.push("--in".into());
        parts.push(
            Path::new(in_run)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| in_run.into()),
        );
    }
    let workdir = Some(s(&meta["worktree"], "sourceWorkdir"))
        .filter(|w| !w.is_empty())
        .unwrap_or_else(|| s(meta, "workdir"));
    if !workdir.is_empty() {
        parts.push("--workdir".into());
        parts.push(workdir.into());
    }
    if agent_pinned(meta) {
        let model = Some(s(meta, "modelName"))
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| s(meta, "model"));
        if !model.is_empty() {
            parts.push("--model".into());
            parts.push(model.into());
        }
    } else if !s(meta, "tier").is_empty() {
        parts.push("--tier".into());
        parts.push(s(meta, "tier").into());
    }
    if !s(meta, "thinking").is_empty() {
        parts.push("--thinking".into());
        parts.push(s(meta, "thinking").into());
    }
    if !s(meta, "accept").is_empty() {
        parts.push("--accept".into());
        parts.push(s(meta, "accept").into());
    }
    for p in strings(&meta["protect"]) {
        parts.push("--protect".into());
        parts.push(p);
    }
    for a in strings(&meta["allow"]) {
        parts.push("--allow".into());
        parts.push(a);
    }
    if !s(meta, "name").is_empty() {
        parts.push("--name".into());
        parts.push(s(meta, "name").into());
    }
    parts.push("--prompt-file".into());
    parts.push(run.join("prompt.md").to_string_lossy().into_owned());
    parts
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn finished_within(run: &Path, window_seconds: f64) -> bool {
    let now = epoch();
    let sum = json(run.join("summary.json"));
    if let Some(ts) = iso_epoch(s(&sum, "finishedAt")) {
        return (now - ts as f64) <= window_seconds;
    }
    let done = run.join("exit_code");
    if let Ok(modified) = fs::metadata(&done)
        .and_then(|m| m.modified())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).map_err(std::io::Error::other))
    {
        return (now - modified.as_secs_f64()) <= window_seconds;
    }
    let meta = json(run.join("meta.json"));
    meta["startedEpoch"]
        .as_f64()
        .is_some_and(|started| (now - started) <= window_seconds)
}

pub fn state(run: &Path) -> String {
    if run.join("exit_code").is_file() {
        let sum = json(run.join("summary.json"));
        return sum
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("crashed")
            .into();
    }
    if legacy_after_waiting(run) {
        return "needs-redispatch".into();
    }
    if supervisor_alive(run) {
        if crate::measure::waiting_ahead(run).is_some() {
            return "waiting".into();
        }
        if run.join(".waiting").exists() {
            return "waiting".into();
        }
        return "running".into();
    }
    if run.join("pid").is_file() {
        return "crashed".into();
    }
    if epoch()
        - json(run.join("meta.json"))["startedEpoch"]
            .as_f64()
            .unwrap_or(0.0)
        <= 15.0
    {
        "starting".into()
    } else {
        "crashed".into()
    }
}
/// Write rounds cut short by a timeout, a stall or the caller's `stop`: continuing
/// one finishes the same work, so it spends no rework round.
pub fn interrupted(state: &str) -> bool {
    matches!(state, "timeout" | "stalled" | "stopped")
}
pub fn active(s: &str) -> bool {
    s == "running" || s == "starting" || s == "waiting"
}
pub fn age_seconds(meta: &Value) -> Option<i64> {
    meta["startedEpoch"]
        .as_f64()
        .map(|started| (epoch() - started).max(0.0) as i64)
}
pub fn other_runs_hint(runs: &[PathBuf]) {
    if runs.is_empty() {
        return;
    }
    let names = runs
        .iter()
        .take(3)
        .map(|run| {
            let meta = json(run.join("meta.json"));
            let name = meta["name"]
                .as_str()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| {
                    run.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("?")
                });
            let age = age_seconds(&meta).unwrap_or(0);
            let age = if age >= 86400 {
                format!("{}d", age / 86400)
            } else if age >= 3600 {
                format!("{}h", age / 3600)
            } else if age >= 60 {
                format!("{}m", age / 60)
            } else {
                format!("{age}s")
            };
            format!("{} {age}", name.replace(['\n', '\r'], " "))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let more = if runs.len() > 3 { ", ..." } else { "" };
    eprintln!("delegate: {} other runs not started by this session ({names}{more}); collect with wait --all or remove with clean <run>", runs.len());
}
pub fn events(run: &Path) -> Vec<Value> {
    read(run.join("events.jsonl"))
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}
pub fn agent_pinned(meta: &Value) -> bool {
    meta["agentPinned"]
        .as_bool()
        .unwrap_or_else(|| s(meta, "tier").is_empty() && !s(meta, "agent").is_empty())
}
pub fn next_step(run: &Path, meta: &Value, state: &str, sum: &Value) -> Option<String> {
    let name = if !s(meta, "run").is_empty() {
        s(meta, "run").to_string()
    } else {
        run.file_name()?.to_string_lossy().into_owned()
    };
    let script = script();
    let script = script.display();
    if active(state) {
        return Some(format!("{script} wait {name}"));
    }
    if run.join(".applied").exists()
        && !run.join(".generate-pending").exists()
        && s(meta, "mode") == "write"
        && s(&json(run.join(".applied")), "appliedBy") == "detected"
    {
        return Some(format!("{script} clean {name}"));
    }
    if state == "needs-redispatch" {
        return Some(format!(
            "原 --after 已在 6.22.0 移除，需要重派：{}",
            redispatch_command(run, meta)
        ));
    }
    if b(sum, "question") {
        return Some(format!(
            "the colleague asks for a decision; answer with {script} reply {name} '<decision>' (not rework)"
        ));
    }
    if state == "delivered" || state == "answered" {
        if s(meta, "mode") != "write"
            || n(
                sum.get("pendingChanges").unwrap_or(&sum["changes"]),
                "files",
            ) == 0
        {
            return None;
        }
        if meta["worktree"].is_null() {
            return Some(format!(
                "review {script} diff {name}; the changes are already in the working tree"
            ));
        }
        if !run.join(".applied").exists() || run.join(".generate-pending").exists() {
            return Some(format!(
                "review {script} diff {name} --total, then merge with {script} apply {name}"
            ));
        }
        return None;
    }
    if b(sum, "infrastructureFailure") {
        return Some(format!("验收未完成（排队/基础设施）；重排：{script} reply {name} '<retry>' (no rework round)；或本地：DELEGATE_REMOTE=0 {script} reply {name} '<retry locally>'"));
    }
    match state {
        "rejected" if !sum["protectViolation"].is_null() => Some(format!(
            "protected paths were changed; review protectViolation, restore them, then {script} reply {name} '<what to fix>'"
        )),
        "rejected" => Some(format!(
            "read accept.tail; {script} reply {name} '<what to fix>' or take it over"
        )),
        "malformed" => Some(format!("{script} reply {name} --tier review '<what to fix>' or do it yourself")),
        "failed" => Some(if s(sum, "transientError").is_empty() {
            "read error; fix the cause or take it over".into()
        } else if n(sum, "resumes") > 0 {
            format!(
                "上游中断（已续做 {} 次）仍未恢复；等节点恢复后再接：{script} reply {name} '接着继续'",
                n(sum, "resumes")
            )
        } else {
            format!("上游中断，续做前的探测未通过；等节点恢复后再接：{script} reply {name} '接着继续'")
        }),
        "timeout" if s(meta, "mode") == "read-only" => Some(format!(
            "{script} reply {name} 'answer with what you found so far' or split the question"
        )),
        "timeout" | "stalled" => Some(format!(
            "{} reply {} {}{}",
            shell_quote(&script.to_string()),
            shell_quote(&name),
            if sum["session"].as_str().is_none_or(str::is_empty)
                || ([crate::agents::AGENT, crate::agy::BIN].contains(&s(meta, "agent"))
                    && crate::agents::continuation_target(meta, s(sum, "session")).is_none())
            {
                "--fresh "
            } else {
                ""
            },
            shell_quote("continue from your diff, then finish"),
        )),
        "killed" | "crashed" => Some("take it over or start it again".into()),
        _ => None,
    }
}
pub fn status(run: &Path) -> Value {
    let meta = json(run.join("meta.json"));
    let st = state(run);
    if !active(&st) && json(run.join("changes.json"))["finalEntries"].is_object() {
        worktree::detect_applied(run, &meta);
    }
    let sum = json(run.join("summary.json"));
    let mut out = json!({"run":meta.get("run").and_then(Value::as_str).unwrap_or_else(||run.file_name().and_then(|x|x.to_str()).unwrap_or("")),"name":meta.get("name"),"state":st,"agent":meta.get("agent").and_then(Value::as_str).unwrap_or(crate::agents::AGENT),"mode":meta.get("mode")});
    if let Some(archive) = crate::archive::status(run) {
        out["archive"] = archive["path"].clone();
        out["cleaned"] = json!(run == Path::new(s(&archive, "path")).join("record"));
    }
    if !s(&meta, "agentBin").is_empty() {
        out["agentBin"] = meta["agentBin"].clone();
    }
    if let Some(sources) = meta.get("configSources") {
        out["configSources"] = sources.clone();
    }
    for key in [
        "standing",
        "standingSources",
        "defaults",
        "maxAnswer",
        "resultDisplayChars",
    ] {
        if let Some(value) = meta.get(key).filter(|v| !v.is_null()) {
            out[key] = value.clone();
        }
    }
    let compression = json(run.join("compression.json"));
    if compression.is_object() {
        out["compression"] = compression;
    }
    if let Some(age) = age_seconds(&meta) {
        out["ageSeconds"] = json!(age);
    }
    if !s(&meta, "tier").is_empty() {
        out["tier"] = meta["tier"].clone();
    }
    out["tierInferred"] = json!(b(&meta, "tierInferred"));
    if agent_pinned(&meta) || meta.get("agentPinned").is_some() {
        out["agentPinned"] = json!(agent_pinned(&meta));
    }
    if !s(&meta, "model").is_empty() {
        out["model"] = meta["model"].clone();
    }
    if !s(&meta, "modelName").is_empty() {
        out["modelName"] = meta["modelName"].clone();
    }
    if run.join(".applied").exists() && !run.join(".generate-pending").exists() {
        out["applied"] = json!(true);
        out["appliedBy"] = json!(json(run.join(".applied"))["appliedBy"]
            .as_str()
            .unwrap_or("delegate"));
    }
    if !s(&meta, "parent").is_empty() {
        out["parent"] = meta["parent"].clone();
    }
    if st == "needs-redispatch" {
        out["redispatch"] = json!(redispatch_command(run, &meta));
    }
    if meta["promptProtectedPaths"]
        .as_array()
        .is_some_and(|items| !items.is_empty())
    {
        out["promptProtectedPaths"] = meta["promptProtectedPaths"].clone();
    }
    if meta["worktree"].is_object() {
        out["worktree"] = json!(s(&meta["worktree"], "path"));
    }
    if !s(&meta, "netBase").is_empty() {
        out["netBase"] = meta["netBase"].clone();
    }
    if let Some(bytes) = meta["worktreeBytes"].as_u64() {
        out["worktreeBytes"] = json!(bytes);
    }
    let unresolved = sum["conflictMarkers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|path| {
            !worktree::conflict_labels(&Path::new(s(&meta["worktree"], "source")).join(path))
                .is_empty()
        })
        .collect::<Vec<_>>();
    if !unresolved.is_empty() {
        out["conflictMarkers"] = json!(unresolved);
    }
    if active(&st) {
        let source_changes = strings(&json(run.join("source-observed.json")));
        if crate::source_watch::busy(&meta) {
            out["busy"] = json!("source workspace; showing cached observation");
        }
        if !source_changes.is_empty() {
            out["sourceWorkspaceChanges"] = json!(source_changes);
        }
    }
    if sum.is_object() {
        for key in [
            "finishedAt",
            "elapsedSeconds",
            "attempts",
            "resumes",
            "transientError",
            "model",
            "turns",
            "files",
            "changes",
            "pendingChanges",
            "commitsDiffer",
            "syncConflictMarkers",
            "shape",
            "accept",
            "infrastructureFailure",
            "evidence",
            "readOnlyViolation",
            "sourceWriteViolation",
            "sourceWorkspaceChanges",
            "protectViolation",
            "protectViolationReasons",
            "workspaceChanged",
            "escalatedFrom",
            "question",
            "queuedSeconds",
            "graceSeconds",
            "stall",
            "denied",
            "tokens",
            "cleanup",
            "warnings",
            "warning",
            "error",
        ] {
            let v = &sum[key];
            if !v.is_null()
                && !v.as_array().is_some_and(Vec::is_empty)
                && !v.as_object().is_some_and(serde_json::Map::is_empty)
            {
                out[key] = v.clone();
            }
        }
    } else {
        let evs = events(run);
        let turns = evs.iter().filter(|x| s(x, "e") == "turn").count();
        out["elapsedSeconds"] =
            json!((epoch() - meta["startedEpoch"].as_f64().unwrap_or(epoch())).max(0.0) as i64);
        out["turns"] = json!(turns);
        let mut files = evs
            .iter()
            .filter(|x| ["edit", "write"].contains(&s(x, "e")) && !s(x, "path").is_empty())
            .map(|x| {
                repository_relative(
                    s(x, "path"),
                    if s(&meta, "top").is_empty() {
                        s(&meta, "workdir")
                    } else {
                        s(&meta, "top")
                    },
                )
            })
            .collect::<Vec<_>>();
        files.sort();
        files.dedup();
        if !files.is_empty() {
            out["files"] = json!(files);
        }
        if st == "running" {
            if let Some(last) = evs.iter().rev().find(|x| {
                !["turn", "result", "settled", "agent_end", "bash_done"].contains(&s(x, "e"))
            }) {
                let detail = ["cmd", "path", "arg", "detail"]
                    .iter()
                    .map(|k| s(last, k))
                    .find(|x| !x.is_empty())
                    .unwrap_or("");
                out["last"] = json!(format!(
                    "{}{}",
                    s(last, "e"),
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {detail}")
                    }
                ));
                if let Some(at) = iso_epoch(s(last, "at")) {
                    out["idleSeconds"] = json!((epoch() as i64 - at).max(0));
                }
            }
        }
    }
    if active(&st) {
        if let Some(queue) = crate::queue::status(run) {
            out["queue"] = queue;
            out.as_object_mut().unwrap().remove("elapsedSeconds");
        } else if run.join(".waiting").exists() {
            out.as_object_mut().unwrap().remove("elapsedSeconds");
        }
    }
    if crate::remote::enabled(&meta["remote"]) {
        out["remote"] = meta["remote"].clone();
        if let Ok(holders) = crate::lane::remote_holders() {
            out["remoteLane"] = json!({"holders":holders.iter().filter(|h| h.2).map(|h| &h.0).collect::<Vec<_>>(), "queued":holders.iter().filter(|h| !h.2).count()});
        }
    }
    if let Some(ahead) = crate::measure::waiting_ahead(run) {
        out["measureWaitingAhead"] = json!(ahead);
    }
    let blind = crate::output::blind_files(&out["files"], &meta["acceptBlind"]);
    if !blind.is_empty() {
        out["acceptBlind"] = json!(blind);
    }
    if let Some(pending) = crate::pending_reply::status(run) { out["pendingReply"] = pending; }
    let result = run.join("result.md");
    if meta["agentDeny"]
        .as_array()
        .is_some_and(|rules| !rules.is_empty())
    {
        out["denied"] = json!(crate::deny::count(run));
    }
    if result.is_file() && fs::metadata(&result).is_ok_and(|m| m.len() > 0) {
        out["result"] = json!(result.to_string_lossy());
        out["resultChars"] = json!(read(&result).chars().count());
    }
    if let Some(step) = next_step(run, &meta, &st, &sum) {
        out["next"] = json!(step);
    }
    if !run.join(".applied-cleaned").exists() && !crate::archive::is_record(run) {
        let missing = crate::refs::missing_objects(run, &meta);
        if !missing.is_empty() {
            out["missingObjects"] = json!(missing
                .iter()
                .map(|(slot, sha)| json!({"ref": slot, "sha": sha}))
                .collect::<Vec<_>>());
            out["recovery"] = json!(crate::refs::recovery_hint(run, &meta));
            out["next"] = json!(crate::refs::recovery_message(run, &meta, &missing));
        }
    }
    let (chain_name, chain_round) = crate::notes::chain_display_info(run);
    if chain_round > 1 {
        out["chainName"] = json!(chain_name);
        out["chainRound"] = json!(chain_round);
    }
    if let Some(chain_notes) = crate::notes::for_run_lazy(run) {
        if !chain_notes.items.is_empty() {
            out["notes"] = chain_notes.items_json();
            out["pendingNotes"] = json!(chain_notes.pending_count());
        }
    }
    out["dir"] = json!(run.to_string_lossy());
    out
}
pub fn unmerged(run: &Path) -> bool {
    let m = json(run.join("meta.json"));
    if !active(&state(run)) {
        worktree::detect_applied(run, &m);
    }
    s(&m, "mode") == "write"
        && m["worktree"].is_object()
        && Path::new(s(&m["worktree"], "path")).exists()
        && (!run.join(".applied").exists() || run.join(".generate-pending").exists())
}
pub fn remove(run: &Path) -> (usize, crate::cleanup::Containers) {
    let meta = json(run.join("meta.json"));
    let path = s(&meta["worktree"], "path").to_string();
    crate::notes::on_run_removed(run, &meta);
    crate::refs::delete_run(run, &meta);
    let stopped = crate::cleanup::record(run, 0);
    let _ = fs::remove_dir_all(run);
    let containers = if !path.is_empty()
        && !all_runs()
            .iter()
            .any(|r| s(&json(r.join("meta.json"))["worktree"], "path") == path)
    {
        crate::remote::clean(&meta);
        worktree::remove(&meta)
    } else {
        crate::cleanup::Containers::default()
    };
    (stopped, containers)
}

/// Apply cleanup has the same scope as explicit clean, but cannot change apply's exit status.
pub fn remove_after_apply(run: &Path) -> Vec<String> {
    if active(&state(run)) || agent_alive(run) {
        return vec![format!("kept active run {}", run.display())];
    }
    let meta = json(run.join("meta.json"));
    let path = s(&meta["worktree"], "path");
    let shared = !path.is_empty()
        && all_runs().iter().any(|other| {
            other != run
                && s(&json(other.join("meta.json"))["worktree"], "path") == path
                && (active(&state(other)) || !other.join(".applied").exists())
        });
    if !shared {
        match crate::archive::preserve(run) {
            Ok(Some(path)) => eprintln!("delegate: archive: {}", path.display()),
            Ok(None) => {}
            Err(error) => return vec![format!("kept worktree: archive failed: {error}")],
        }
    }
    let _ = crate::cleanup::record(run, 0);
    let containers = if !shared {
        crate::remote::clean(&meta);
        worktree::remove(&meta)
    } else {
        crate::cleanup::Containers::default()
    };
    if crate::pool::is_slot(Path::new(path)) {
        // Every applied round sharing the slot must stop holding it, not just the
        // latest reply. Active/unapplied sharers are still protected by `shared`.
        for other in all_runs()
            .into_iter()
            .filter(|other| other == run || !shared)
        {
            let mut released = json(other.join("meta.json"));
            if s(&released["worktree"], "path") == path && !active(&state(&other)) {
                released["worktree"]["path"] = json!(format!("{path}#released"));
                let _ = write_json(other.join("meta.json"), &released);
            }
        }
    }
    let keep = [
        "meta.json",
        "archive.json",
        "pending-reply.json",
        "reply.lock",
        "reply-delivered.json",
        "applied.patch",
        "prompt.md",
        "result.md",
        "result-original.md",
        "summary.json",
        "compression.json",
        "exit_code",
        ".applied",
        ".delivered",
        ".truncated",
        ".refs-pinned",
    ];
    if let Ok(entries) = fs::read_dir(run) {
        for entry in entries.flatten() {
            if keep.contains(&entry.file_name().to_string_lossy().as_ref()) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                let _ = fs::remove_dir_all(path);
            } else {
                let _ = fs::remove_file(path);
            }
        }
    }
    let _ = write(run.join(".applied-cleaned"), "1\n");
    let _ = fs::remove_file(run.join("exit_code"));
    let _ = write(run.join("exit_code"), "0\n");
    containers.warn_kept();
    let mut diagnostics = containers.diagnostic.clone().into_iter().collect::<Vec<_>>();
    if !containers.removed_names.is_empty() {
        diagnostics.push(format!("removed containers: {}", containers.removed_names.join(", ")));
    }
    if !shared
        && !crate::pool::is_slot(Path::new(path))
        && Path::new(path).is_absolute()
        && path != s(&meta["worktree"], "source")
        && Path::new(path).exists()
    {
        diagnostics.push(format!("could not remove worktree {path}"));
    }
    diagnostics
}

pub fn record_worktree_bytes(run: &Path) {
    let mut meta = json(run.join("meta.json"));
    if s(&meta, "mode") != "write" || meta["worktreeBytes"].as_u64().is_some() {
        return;
    }
    let path = Path::new(s(&meta["worktree"], "path"));
    if !path.is_absolute() || !path.is_dir() || path == Path::new(s(&meta["worktree"], "source")) {
        return;
    }
    let mut command = std::process::Command::new("du");
    command.args(["-sb", "--"]).arg(path);
    if let Ok(output) = crate::cleanup::bounded_output(command, std::time::Duration::from_secs(20))
    {
        if let Some(bytes) = output
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<u64>().ok())
        {
            meta["worktreeBytes"] = json!(bytes);
            let _ = write_json(run.join("meta.json"), &meta);
        }
    }
}
pub fn prune() {
    let Ok(days) = setting("KEEP_DAYS", "7").parse::<u64>() else {
        return;
    };
    if days == 0 {
        return;
    }
    for run in all_runs().into_iter().chain(archived_runs()) {
        // Archive history follows the archive's own age/LRU policy, not KEEP_DAYS.
        if crate::archive::is_record(&run) { continue; }
        let done = run.join("exit_code");
        // 只显示过截断答复的 run 不自动清理，全文仍待读取。
        if run.join(".delivered").is_file()
            && crate::pending_reply::status(&run).is_none()
            && !run.join(".truncated").exists()
            && done.is_file()
            && !unmerged(&run)
        {
            use std::time::UNIX_EPOCH;
            let age_file = if run.join(".applied-cleaned").exists() {
                run.join(".applied-cleaned")
            } else {
                done.clone()
            };
            let age = fs::metadata(&age_file)
                .and_then(|x| x.modified())
                .ok()
                .and_then(|x| x.duration_since(UNIX_EPOCH).ok())
                .map(|x| epoch() - x.as_secs_f64())
                .unwrap_or(0.0);
            if age > days as f64 * 86400.0 {
                match crate::archive::preserve(&run) {
                    Ok(_) => {
                        remove(&run);
                    }
                    Err(error) => {
                        eprintln!("delegate: keep {}: archive failed: {error}", run.display())
                    }
                }
            }
        }
    }
}
/// A reply round carries the conversation on unless it was malformed or
/// ended without saving a session (the agent failed before it wrote one);
/// the next reply then continues from the round before it.
pub fn continues_conversation(run: &Path) -> bool {
    // A lightweight applied round deliberately has no session. It still owns
    // the conversation head: reply will reopen it from current source context.
    if run.join(".applied-cleaned").exists() {
        return true;
    }
    let st = state(run);
    if st == "malformed" {
        return false;
    }
    if active(&st) {
        return true;
    }
    // agy keeps its conversation itself: the recorded id is what a reply continues.
    let session = s(&json(run.join("summary.json")), "session").to_string();
    !session.is_empty()
        && crate::agents::continuation_target(&json(run.join("meta.json")), &session).is_some()
}

pub fn latest(mut run: PathBuf) -> PathBuf {
    loop {
        let mut replies = all_runs()
            .into_iter()
            .filter(|r| {
                s(&json(r.join("meta.json")), "parent")
                    == run.file_name().unwrap_or_default().to_string_lossy()
                    && continues_conversation(r)
            })
            .collect::<Vec<_>>();
        if let Some(next) = replies.pop() {
            run = next;
            continue;
        }
        // Naming a round that saved no session continues the round before it.
        while !continues_conversation(&run) {
            let parent = s(&json(run.join("meta.json")), "parent").to_string();
            match (!parent.is_empty())
                .then(|| resolve(&parent).ok())
                .flatten()
            {
                Some(previous) => run = previous,
                None => break,
            }
        }
        return run;
    }
}

#[cfg(test)]
mod agy_conversation_tests {
    use super::continues_conversation;
    use std::fs;

    #[test]
    fn a_finished_agy_round_continues_its_conversation() {
        let run = std::env::temp_dir().join(format!("delegate-agy-run-{}", std::process::id()));
        fs::create_dir_all(&run).unwrap();
        fs::write(run.join("meta.json"), r#"{"model":"agy/gemini-4-neon","sessionDir":"/nonexistent"}"#).unwrap();
        fs::write(run.join("summary.json"), r#"{"state":"delivered","session":"c-1"}"#).unwrap();
        fs::write(run.join("exit_code"), "0").unwrap();
        assert!(continues_conversation(&run));
        fs::write(run.join("summary.json"), r#"{"state":"delivered","session":""}"#).unwrap();
        assert!(!continues_conversation(&run));
        fs::remove_dir_all(&run).unwrap();
    }
}
