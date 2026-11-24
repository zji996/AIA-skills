use crate::common::*;
use crate::worktree;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub fn all_runs_in(root: &Path) -> Vec<PathBuf> {
    let mut v = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("meta.json").is_file())
        .collect::<Vec<_>>();
    v.sort_by_cached_key(|p| json(p.join("meta.json"))["startedNs"].as_u64().unwrap_or(0));
    v
}
pub fn all_runs_in_with_index(root: &Path, index: &Value) -> Vec<PathBuf> {
    let mut v = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("meta.json").is_file())
        .collect::<Vec<_>>();
    v.sort_by_cached_key(|p| {
        crate::index::cached_started_ns(p, index)
            .unwrap_or_else(|| json(p.join("meta.json"))["startedNs"].as_u64().unwrap_or(0))
    });
    v
}
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
    v.sort_by_cached_key(|p| json(p.join("meta.json"))["startedNs"].as_u64().unwrap_or(0));
    v
}
pub fn all_runs_sorted_with_index(index: &Value) -> Vec<PathBuf> {
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
    v.sort_by_cached_key(|p| {
        crate::index::cached_started_ns(p, index)
            .unwrap_or_else(|| json(p.join("meta.json"))["startedNs"].as_u64().unwrap_or(0))
    });
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
    if matches.len() == 1 {
        return Ok(matches[0].clone());
    }
    let archive = runs_root().join(".applied").join(reference);
    if archive.join("meta.json").is_file() {
        return Ok(archive);
    }
    let evidence = cache_dir().join("archive").join(reference).join("record");
    if evidence.join("meta.json").is_file() {
        return Ok(evidence);
    }
    if matches.is_empty() && named.is_empty() {
        let archived = archived_runs();
        let named_arch = archived
            .iter()
            .filter(|p| s(&json(p.join("meta.json")), "name") == reference)
            .collect::<Vec<_>>();
        if !named_arch.is_empty() {
            if let Some(newest) = named_arch
                .into_iter()
                .max_by_key(|p| json(p.join("meta.json"))["startedNs"].as_u64().unwrap_or(0))
            {
                return Ok(newest.clone());
            }
        }
        let matches_arch = archived
            .iter()
            .filter(|p| {
                let meta = json(p.join("meta.json"));
                s(&meta, "run").contains(reference)
                    || p.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .contains(reference)
            })
            .collect::<Vec<_>>();
        if matches_arch.len() == 1 {
            return Ok(matches_arch[0].clone());
        }
    }
    if matches.len() > 1 {
        let names: Vec<String> = matches
            .iter()
            .take(5)
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        return Err(format!(
            "run '{reference}' matched {} runs under {}（{}）",
            matches.len(),
            root.display(),
            names.join(", ")
        ));
    }
    let suggestions = suggest_similar_runs(reference, &runs);
    if !suggestions.is_empty() {
        return Err(format!(
            "run '{reference}' matched 0 runs under {}；相近 run：{}（可用 delegate status --all 查看）",
            root.display(),
            suggestions.join(", ")
        ));
    }
    Err(format!("run '{reference}' matched 0 runs under {} (runs live under the git root of the directory where start ran; pass a run directory or set DELEGATE_RUNS)",root.display()))
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

fn strip_run_timestamp(s: &str) -> &str {
    // Strip leading `YYYYMMDD-HHMMSS-` when present to compare human slugs cleanly.
    let bytes = s.as_bytes();
    if bytes.len() > 16
        && bytes[8] == b'-'
        && bytes[15] == b'-'
        && bytes[..8].iter().all(|b| b.is_ascii_digit())
        && bytes[9..15].iter().all(|b| b.is_ascii_digit())
    {
        &s[16..]
    } else {
        s
    }
}

/// Suggest up to 5 existing runs with similar prefix or slug tokens when a run lookup misses.
pub fn suggest_similar_runs(reference: &str, repo_runs: &[PathBuf]) -> Vec<String> {
    let ref_lc = reference.trim().to_ascii_lowercase();
    if ref_lc.is_empty() {
        return Vec::new();
    }
    let ref_slug = strip_run_timestamp(&ref_lc);
    let ref_tokens: Vec<&str> = ref_slug
        .split(['-', '_', '/'])
        .filter(|t| t.len() >= 2)
        .collect();

    let mut seen = std::collections::BTreeSet::<String>::new();
    let mut candidates = Vec::<String>::new();
    let mut push_path = |p: &Path| {
        let meta = json(p.join("meta.json"));
        let id = {
            let r = s(&meta, "run");
            if !r.is_empty() {
                r.to_string()
            } else {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }
        };
        if !id.is_empty() && seen.insert(id.clone()) {
            candidates.push(id);
        }
    };
    for r in repo_runs {
        push_path(r);
    }
    for r in crate::launch::machine_runs(&crate::common::state_dir()) {
        push_path(&r);
    }
    for r in archived_runs() {
        push_path(&r);
    }

    let mut scored: Vec<(usize, String)> = Vec::new();
    for cand in candidates {
        let cand_lc = cand.to_ascii_lowercase();
        let cand_slug = strip_run_timestamp(&cand_lc);
        let full_pref = common_prefix_len(&ref_lc, &cand_lc);
        let slug_pref = common_prefix_len(ref_slug, cand_slug);
        let exact_slug = !ref_slug.is_empty() && ref_slug == cand_slug;
        let slug_sub = !ref_slug.is_empty()
            && !cand_slug.is_empty()
            && ((ref_slug.len() >= 3 && cand_slug.contains(ref_slug))
                || (cand_slug.len() >= 3 && ref_slug.contains(cand_slug)));
        let cand_tokens: Vec<&str> = cand_slug
            .split(['-', '_', '/'])
            .filter(|t| t.len() >= 2)
            .collect();
        let shared_tokens = ref_tokens
            .iter()
            .filter(|t| cand_tokens.contains(t))
            .count();

        if exact_slug || slug_sub || shared_tokens > 0 || slug_pref >= 3 || full_pref >= 4 {
            let score = (if exact_slug { 1000 } else { 0 })
                + (if slug_sub { 300 } else { 0 })
                + shared_tokens * 120
                + slug_pref * 20
                + full_pref * 10;
            scored.push((score, cand));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    scored.into_iter().take(5).map(|(_, id)| id).collect()
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
    for pid_file in ["launch.pid", "supervisor.pid"] {
        if run.join(pid_file).is_file() {
            if let Ok(pid) = read(run.join(pid_file)).trim().parse::<i32>() {
                if pid_alive(pid) {
                    return if run.join(".waiting").exists() {
                        "waiting".into()
                    } else {
                        "starting".into()
                    };
                }
            }
        }
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
    other_runs_hint_with(runs, false);
}
pub fn other_runs_hint_with(runs: &[PathBuf], machine: bool) {
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
    let collect_cmd = if machine {
        "wait --machine --all"
    } else {
        "wait --all"
    };
    eprintln!("delegate: {} other runs not started by this session ({names}{more}); collect with {collect_cmd} or remove with clean <run>", runs.len());
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
pub fn next_step(
    run: &Path,
    meta: &Value,
    state: &str,
    sum: &Value,
    live_worktree: bool,
) -> Option<String> {
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
    if b(sum, "policyRefused") {
        if s(meta, "mode") == "write" && !run.join(".applied").exists() {
            return Some(format!(
                "结论被模型服务拒绝；先核对 review.md 与 {script} files {name}，再决定 {script} apply {name} 或 {script} reply {name} '重写结论'"
            ));
        }
        return Some(format!(
            "结论被模型服务拒绝；查看 {script} files {name} 或用 {script} reply {name} '重写结论'"
        ));
    }
    if state == "delivered" || state == "answered" {
        let rev = {
            let f = json(run.join("review.json"));
            if f.is_object() {
                f
            } else {
                sum["review"].clone()
            }
        };
        if s(&rev, "verdict") == "NEEDS_CONTROLLER" && !run.join(".applied").exists() {
            if let Some(sign) = crate::review::sign_status(run, meta, &rev, live_worktree) {
                if b(&sign, "required") && !b(&sign, "valid") {
                    return Some(format!(
                        "review controller-review.diff (NEEDS_CONTROLLER) and sign with {script} sign {name} before {script} apply {name}"
                    ));
                }
            }
            return Some(format!(
                "review review.md (NEEDS_CONTROLLER), then {script} reply {name} '<decision>' or {script} apply {name}"
            ));
        }
        let changed_files = if meta["worktree"].is_object() {
            if let Some(pending) = sum.get("pendingChanges").filter(|v| v.is_object()) {
                n(pending, "files")
            } else if n(&sum["changes"], "files") > 0 {
                n(&sum["changes"], "files")
            } else {
                chain_pending_apply_files_opts(run, meta, sum, &all_runs(), live_worktree)
            }
        } else {
            n(
                sum.get("pendingChanges")
                    .filter(|v| v.is_object())
                    .unwrap_or(&sum["changes"]),
                "files",
            )
        };
        if s(meta, "mode") != "write" || changed_files == 0 {
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
        "blocked_mainline" => Some(format!(
            "主干阻塞（主干修复阻塞文件后自动重验；也可 {script} reply {name} '重跑'）"
        )),
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
/// Zero-lock, zero-side-effect run status (does not run `detect_applied` writes or `git cat-file`
/// object probes unless `check_missing_objects` is explicitly enabled for single-run inspection).
pub fn status(run: &Path) -> Value {
    status_with_opts(run, false)
}

pub fn status_detail(run: &Path) -> Value {
    status_with_opts(run, true)
}

pub fn status_with_opts(run: &Path, check_missing_objects: bool) -> Value {
    let meta = json(run.join("meta.json"));
    let st = state(run);
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
        "area",
        "areaGroup",
        "areaExperiment",
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
    if !s(&meta, "workId").is_empty() {
        out["workId"] = meta["workId"].clone();
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
    if !s(&meta, "top").is_empty() {
        out["top"] = meta["top"].clone();
    }
    if b(&meta, "shell") {
        out["shell"] = json!(true);
    }
    if !s(&meta, "netBase").is_empty() {
        out["netBase"] = meta["netBase"].clone();
    }
    if !s(&meta, "caller").is_empty() {
        out["caller"] = meta["caller"].clone();
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
        let act = crate::activity::inspect(run, &meta, &st);
        out["activity"] = act;
    }
    let stall_warning = json(run.join("stall-warning.json"));
    if stall_warning.is_object() {
        out["stallWarning"] = stall_warning;
    }
    let review_doc = {
        let f = json(run.join("review.json"));
        if f.is_object() {
            f
        } else if sum["review"].is_object() {
            sum["review"].clone()
        } else {
            Value::Null
        }
    };
    if review_doc.is_object() {
        out["review"] = crate::review::compact_review_summary(&review_doc);
    }
    if let Some(sign) = crate::review::sign_status(run, &meta, &review_doc, check_missing_objects) {
        out["sign"] = sign;
    }
    if let Some(pm) = crate::premerge::status_for_run(run, &meta, &sum, None) {
        out["premerge"] = pm;
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
            "mainlineBlock",
            "card",
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
            "policyRefused",
            "policyRewriteAttempted",
            "queuedSeconds",
            "graceSeconds",
            "stall",
            "denied",
            "tokens",
            "tokensDelta",
            "reviewTokens",
            "agyAccount",
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
    if let Some(step) = next_step(run, &meta, &st, &sum, check_missing_objects) {
        out["next"] = json!(step);
    }
    if !run.join(".applied-cleaned").exists() && !crate::archive::is_record(run) {
        let missing = if check_missing_objects {
            crate::refs::missing_objects(run, &meta)
        } else {
            let cached = json(run.join("missing-objects.json"));
            cached
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| {
                    let slot = match s(item, "ref") {
                        "base" => "base",
                        "after" => "after",
                        "chain-base" => "chain-base",
                        "applied" => "applied",
                        "applied-base" => "applied-base",
                        "sync-base" => "sync-base",
                        "worktree-base" => "worktree-base",
                        _ => return None,
                    };
                    let sha = s(item, "sha").to_string();
                    (!sha.is_empty()).then_some((slot, sha))
                })
                .collect()
        };
        if !missing.is_empty() {
            out["missingObjects"] = json!(missing
                .iter()
                .map(|(slot, sha)| json!({"ref": slot, "sha": sha}))
                .collect::<Vec<_>>());
            out["recovery"] = json!(crate::refs::recovery_hint(run, &meta));
            out["next"] = json!(crate::refs::recovery_message(run, &meta, &missing));
        }
    }
    if !s(&meta, "parent").is_empty() {
        let (chain_name, chain_round) = crate::notes::chain_display_info(run);
        if chain_round > 1 {
            out["chainName"] = json!(chain_name);
            out["chainRound"] = json!(chain_round);
        }
    }
    if crate::work::any_notes_exist() {
        if let Some(chain_notes) = crate::notes::for_run_lazy(run) {
            if !chain_notes.items.is_empty() {
                out["notes"] = chain_notes.items_json();
                out["pendingNotes"] = json!(chain_notes.pending_count());
            }
        }
    }
    out["dir"] = json!(run.to_string_lossy());
    out
}
pub fn unmerged(run: &Path) -> bool {
    let m = json(run.join("meta.json"));
    s(&m, "mode") == "write"
        && m["worktree"].is_object()
        && Path::new(s(&m["worktree"], "path")).exists()
        && (!run.join(".applied").exists() || run.join(".generate-pending").exists())
}
pub fn remove(run: &Path) -> (usize, crate::cleanup::Containers) {
    let meta = json(run.join("meta.json"));
    let path = s(&meta["worktree"], "path").to_string();
    let _ = crate::work::on_run_cleaned(run, &meta);
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

/// Apply cleanup (lightweight retention mode): archives evidence and stages worktree removal
/// asynchronously via `gc::stage_worktree_only_to_trash`, keeping `.applied-cleaned` + patches.
/// Returns `(diagnostics, staged_gc)`.
pub fn remove_after_apply(run: &Path) -> (Vec<String>, bool) {
    if active(&state(run)) || agent_alive(run) {
        return (vec![format!("kept active run {}", run.display())], false);
    }
    let meta = json(run.join("meta.json"));
    let path = s(&meta["worktree"], "path").to_string();
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
            Err(error) => return (vec![format!("kept worktree: archive failed: {error}")], false),
        }
    }
    let _ = crate::cleanup::record(run, 0);
    let mut diagnostics = Vec::new();
    let mut staged_gc = false;
    if !shared && !path.is_empty() {
        match crate::gc::stage_worktree_only_to_trash(run, &meta) {
            Ok(Some(_)) => staged_gc = true,
            Ok(None) => {}
            Err(error) => diagnostics.push(format!("could not stage worktree {path} to trash: {error}")),
        }
    }
    if crate::pool::is_slot(Path::new(&path)) {
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
        "changes.json",
        "applied.patch",
        "applied-files.txt",
        "prompt.md",
        "result.md",
        "result-original.md",
        "summary.json",
        "review.json",
        "review.md",
        "review",
        "controller-review.diff",
        "sign.json",
        "compression.json",
        "exit_code",
        ".applied",
        ".delivered",
        ".truncated",
        ".refs-pinned",
    ];
    if let Ok(entries) = fs::read_dir(run) {
        for entry in entries.flatten() {
            let fname = entry.file_name();
            let fname_str = fname.to_string_lossy();
            if keep.contains(&fname_str.as_ref()) || is_round_result_name(&fname_str) {
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
    if !shared
        && !crate::pool::is_slot(Path::new(&path))
        && Path::new(&path).is_absolute()
        && path != s(&meta["worktree"], "source")
        && Path::new(&path).exists()
    {
        diagnostics.push(format!("could not remove worktree {path}"));
    }
    (diagnostics, staged_gc)
}

/// Fail-closed full automatic clean of an applied run (or superseded predecessor in an applied chain):
/// preserves archive first, deletes pinned refs, stages run + unshared worktree into trash via
/// `gc::stage_clean_to_trash`, records clean in history/index/board/events.
pub fn auto_clean_applied_run(run: &Path, remove_worktree: bool) -> Res<Option<PathBuf>> {
    if !run.is_dir() {
        return Ok(None);
    }
    let st = state(run);
    if active(&st) || agent_alive(run) {
        return Err(format!("kept active run {}", run.display()));
    }
    let meta = json(run.join("meta.json"));
    let archived = crate::archive::preserve(run)?;
    let _ = crate::work::on_run_cleaned(run, &meta);
    crate::refs::delete_run(run, &meta);
    let _ = crate::cleanup::reclaim_on_clean(run, false);
    let wt_path = s(&meta["worktree"], "path").to_string();
    if remove_worktree && !wt_path.is_empty() && crate::pool::is_slot(Path::new(&wt_path)) {
        for other in all_runs() {
            if other == run {
                continue;
            }
            let mut released = json(other.join("meta.json"));
            if s(&released["worktree"], "path") == wt_path && !active(&state(&other)) {
                released["worktree"]["path"] = json!(format!("{wt_path}#released"));
                let _ = write_json(other.join("meta.json"), &released);
            }
        }
    }
    let run_id_ev = crate::events::run_id_for(run, &meta);
    let repo_ev = crate::events::repo_for_meta(&meta);
    let _job = crate::gc::stage_clean_to_trash(run, &meta, remove_worktree)?;
    crate::history::record_clean(run, &meta, archived.as_deref());
    crate::index::remove_run(run);
    crate::board::refresh_for_meta(&meta);
    crate::events::emit(
        &run_id_ev,
        &repo_ev,
        "cleaned",
        json!({
            "name": s(&meta, "name"),
            "state": st,
            "autoCleanedOnApply": true,
            "archive": archived.as_ref().map(|p| p.to_string_lossy().into_owned()),
        }),
    );
    Ok(archived)
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
                let meta = json(run.join("meta.json"));
                match crate::archive::preserve(&run) {
                    Ok(archived) => {
                        remove(&run);
                        crate::history::record_clean(&run, &meta, archived.as_deref());
                        crate::board::refresh_for_meta(&meta);
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
pub fn continues_conversation_from(run: &Path, meta: &Value, st: &str, sum: &Value) -> bool {
    // A lightweight applied round deliberately has no session. It still owns
    // the conversation head: reply will reopen it from current source context.
    if run.join(".applied-cleaned").exists() {
        return true;
    }
    if st == "malformed" {
        return false;
    }
    if active(st) {
        return true;
    }
    // agy keeps its conversation itself: the recorded id is what a reply continues.
    let session = s(sum, "session");
    !session.is_empty() && crate::agents::continuation_target(meta, session).is_some()
}

pub fn continues_conversation(run: &Path) -> bool {
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
    let meta = json(run.join("meta.json"));
    let sum = json(run.join("summary.json"));
    continues_conversation_from(run, &meta, &st, &sum)
}

/// Lightweight auxiliary metadata cached alongside `status` in `state/index.json`
/// so non-`--all` `status` classifies runs without re-opening `meta.json` or `summary.json`.
pub fn index_meta(run: &Path) -> Value {
    let meta = json(run.join("meta.json"));
    let st = state(run);
    let sum = json(run.join("summary.json"));
    let cont = continues_conversation_from(run, &meta, &st, &sum);
    json!({
        "resultReadAt": s(&meta, "resultReadAt"),
        "delivered": run.join(".delivered").exists(),
        "truncated": run.join(".truncated").exists(),
        "appliedCleaned": run.join(".applied-cleaned").exists(),
        "appliedAt": s(&meta, "appliedAt"),
        "repliedAt": s(&meta, "repliedAt"),
        "hasSyncBase": run.join(".sync-base").exists(),
        "continuesConversation": cont,
        "worktreeSource": s(&meta["worktree"], "source"),
    })
}

/// Return every run directory in the reply/continuation chain containing `run`
/// (ancestors via `parent`, `run` itself, and all descendants whose `parent` links into the chain),
/// ordered chronologically by `(startedNs, dir_name)`.
pub fn chain_rounds(run: &Path) -> Vec<PathBuf> {
    use std::collections::{HashMap, HashSet, VecDeque};

    let all = all_runs();
    let mut by_id: HashMap<String, PathBuf> = HashMap::new();
    let mut parent_of: HashMap<String, String> = HashMap::new();
    let mut children_of: HashMap<String, Vec<String>> = HashMap::new();

    let mut register = |dir: &Path| {
        let meta = json(dir.join("meta.json"));
        let id = {
            let r = s(&meta, "run");
            if !r.is_empty() {
                r.to_string()
            } else {
                dir.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }
        };
        if id.is_empty() {
            return;
        }
        let parent = s(&meta, "parent").to_string();
        by_id.entry(id.clone()).or_insert_with(|| dir.to_path_buf());
        if !parent.is_empty() {
            parent_of.entry(id.clone()).or_insert_with(|| parent.clone());
            let kids = children_of.entry(parent).or_default();
            if !kids.contains(&id) {
                kids.push(id);
            }
        }
    };

    register(run);
    for dir in &all {
        register(dir);
    }
    // Also scan sibling run directories next to `run` in case `run` lives in a repo outside cwd.
    if let Some(runs_parent) = run.parent() {
        if let Ok(rd) = fs::read_dir(runs_parent) {
            for ent in rd.flatten() {
                let p = ent.path();
                if p.is_dir() && p.join("meta.json").is_file() {
                    register(&p);
                }
            }
        }
    }

    let start_meta = json(run.join("meta.json"));
    let start_id = {
        let r = s(&start_meta, "run");
        if !r.is_empty() {
            r.to_string()
        } else {
            run.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }
    };
    if start_id.is_empty() {
        return vec![run.to_path_buf()];
    }

    // BFS across undirected parent<->child edges so root, middle, leaf, or sibling rounds are all found.
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    visited.insert(start_id.clone());
    queue.push_back(start_id);

    while let Some(cur) = queue.pop_front() {
        if let Some(par) = parent_of.get(&cur) {
            if !par.is_empty() && by_id.contains_key(par) && visited.insert(par.clone()) {
                queue.push_back(par.clone());
            }
        }
        if let Some(kids) = children_of.get(&cur) {
            for kid in kids {
                if by_id.contains_key(kid) && visited.insert(kid.clone()) {
                    queue.push_back(kid.clone());
                }
            }
        }
    }

    let mut result: Vec<(u64, String, PathBuf)> = visited
        .into_iter()
        .filter_map(|id| {
            let dir = by_id.get(&id)?.clone();
            let ns = json(dir.join("meta.json"))["startedNs"].as_u64().unwrap_or(0);
            Some((ns, id, dir))
        })
        .collect();
    result.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    result.into_iter().map(|(_, _, dir)| dir).collect()
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

/// Return run IDs that have been superseded by a valid continuation reply.
/// Unlike a blind `parent`/`repliedAt` scan, a live child that failed without continuing the
/// conversation (e.g. `malformed` / `crashed` with no saved session) does NOT suppress its
/// delivered parent from `status` / `board`.
pub fn superseded_by_reply(
    all: &[PathBuf],
    hist_entries: &[crate::history::HistoryEntry],
) -> std::collections::HashSet<String> {
    use std::collections::{HashMap, HashSet};
    let mut live_children: HashMap<String, Vec<&PathBuf>> = HashMap::new();
    for run in all {
        let meta = json(run.join("meta.json"));
        let parent = s(&meta, "parent");
        if !parent.is_empty() {
            live_children
                .entry(parent.to_string())
                .or_default()
                .push(run);
        }
    }

    let mut superseded = HashSet::<String>::new();
    for (parent, children) in &live_children {
        if children
            .iter()
            .any(|c| active(&state(c)) || continues_conversation(c))
        {
            superseded.insert(parent.clone());
        }
    }
    for e in hist_entries {
        if !e.parent.is_empty()
            && !live_children.contains_key(&e.parent)
            && !matches!(e.state.as_str(), "malformed" | "crashed")
        {
            superseded.insert(e.parent.clone());
        }
    }
    for run in all {
        let meta = json(run.join("meta.json"));
        let id = crate::notes::run_id(run, &meta);
        if !s(&meta, "repliedAt").is_empty() && !live_children.contains_key(&id) {
            superseded.insert(id);
        }
    }
    superseded
}

/// Zero-disk-read version of `superseded_by_reply` driven directly by `index::status_and_meta_cached`.
pub fn superseded_by_reply_cached(
    entries: &[(PathBuf, Value, Value)],
    hist_entries: &[crate::history::HistoryEntry],
) -> std::collections::HashSet<String> {
    use std::collections::{HashMap, HashSet};
    let mut live_children: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, (_, status, _)) in entries.iter().enumerate() {
        let parent = s(status, "parent");
        if !parent.is_empty() {
            live_children.entry(parent).or_default().push(i);
        }
    }

    let mut superseded = HashSet::<String>::new();
    for (parent, indices) in &live_children {
        if indices.iter().any(|&idx| {
            let (_, status, idx_meta) = &entries[idx];
            active(s(status, "state")) || b(idx_meta, "continuesConversation")
        }) {
            superseded.insert((*parent).to_string());
        }
    }
    for e in hist_entries {
        if !e.parent.is_empty()
            && !live_children.contains_key(e.parent.as_str())
            && !matches!(e.state.as_str(), "malformed" | "crashed")
        {
            superseded.insert(e.parent.clone());
        }
    }
    for (run, status, idx_meta) in entries {
        let id = {
            let r = s(status, "run");
            if !r.is_empty() {
                r.to_string()
            } else {
                run.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }
        };
        if !s(idx_meta, "repliedAt").is_empty() && !live_children.contains_key(id.as_str()) {
            superseded.insert(id);
        }
    }
    superseded
}

/// Zero-disk-read, zero-git version of `chain_pending_apply_files` driven by cached status + meta.
pub fn chain_pending_apply_files_cached(
    idx: usize,
    entries: &[(PathBuf, Value, Value)],
) -> i64 {
    let (_, status, _) = &entries[idx];
    if s(status, "mode") != "write" || s(status, "worktree").is_empty() {
        return 0;
    }
    if let Some(pending) = status.get("pendingChanges").filter(|v| v.is_object()) {
        return n(pending, "files");
    }
    let own = n(&status["changes"], "files");
    if own > 0 {
        return own;
    }
    let wt_path = s(status, "worktree");
    let mut parent = s(status, "parent").to_string();
    let mut visited = std::collections::HashSet::new();
    while !parent.is_empty() && visited.insert(parent.clone()) {
        let Some((_, anc_status, anc_meta)) = entries.iter().find(|(r, st, _)| {
            s(st, "run") == parent.as_str()
                || r.file_name().is_some_and(|name| name == parent.as_str())
        }) else {
            break;
        };
        if b(anc_status, "applied")
            || b(anc_meta, "appliedCleaned")
            || b(anc_meta, "hasSyncBase")
            || !s(anc_meta, "appliedAt").is_empty()
        {
            break;
        }
        if !wt_path.is_empty() && s(anc_status, "worktree") != wt_path {
            break;
        }
        if let Some(pending) = anc_status.get("pendingChanges").filter(|v| v.is_object()) {
            let pf = n(pending, "files");
            if pf > 0 {
                return pf;
            }
        }
        let af = n(&anc_status["changes"], "files");
        if af > 0 {
            return af;
        }
        parent = s(anc_status, "parent").to_string();
    }
    0
}

/// Count unapplied worktree files for `run` across its continuation chain.
/// Handles null/missing `pendingChanges` and zero-edit reply tails whose unapplied ancestors
/// in the same worktree carry pending changes.
pub fn chain_pending_apply_files(run: &Path, meta: &Value, sum: &Value, all: &[PathBuf]) -> i64 {
    chain_pending_apply_files_opts(run, meta, sum, all, true)
}

pub fn chain_pending_apply_files_opts(
    run: &Path,
    meta: &Value,
    sum: &Value,
    all: &[PathBuf],
    live_git: bool,
) -> i64 {
    if s(meta, "mode") != "write" || !meta["worktree"].is_object() {
        return 0;
    }
    if let Some(pending) = sum.get("pendingChanges").filter(|v| v.is_object()) {
        return n(pending, "files");
    }
    if live_git {
        if let Some(pending) = crate::worktree::pending_changes(run, meta) {
            if pending.is_object() {
                return n(&pending, "files");
            }
        }
    }
    let own = n(&sum["changes"], "files");
    if own > 0 {
        return own;
    }
    let wt_path = s(&meta["worktree"], "path");
    let mut parent = s(meta, "parent").to_string();
    let mut visited = std::collections::HashSet::new();
    while !parent.is_empty() && visited.insert(parent.clone()) {
        let Some(anc) = all
            .iter()
            .find(|r| r.file_name().is_some_and(|name| name == parent.as_str()))
        else {
            break;
        };
        if anc.join(".applied").exists()
            || anc.join(".applied-cleaned").exists()
            || anc.join(".sync-base").exists()
        {
            break;
        }
        let anc_meta = json(anc.join("meta.json"));
        if !s(&anc_meta, "appliedAt").is_empty() {
            break;
        }
        if !wt_path.is_empty() && s(&anc_meta["worktree"], "path") != wt_path {
            break;
        }
        let anc_sum = json(anc.join("summary.json"));
        if let Some(pending) = anc_sum.get("pendingChanges").filter(|v| v.is_object()) {
            let pf = n(pending, "files");
            if pf > 0 {
                return pf;
            }
        }
        let af = n(&anc_sum["changes"], "files");
        if af > 0 {
            return af;
        }
        parent = s(&anc_meta, "parent").to_string();
    }
    0
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultRound {
    pub round: usize,
    pub run_id: String,
    pub intra_round: usize,
    pub kind: &'static str,
    pub path: PathBuf,
    pub total_lines: usize,
    pub total_chars: usize,
    pub current: bool,
}

impl ResultRound {
    pub fn kind_zh(&self) -> &'static str {
        match self.kind {
            "rework" => "返工",
            "reply" => "reply",
            _ => "初稿",
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "round": self.round,
            "run": self.run_id,
            "intraRound": self.intra_round,
            "kind": self.kind,
            "path": self.path.to_string_lossy(),
            "totalLines": self.total_lines,
            "totalChars": self.total_chars,
            "current": self.current,
        })
    }

    pub fn to_json_with_text(&self, text: &str) -> Value {
        let mut obj = self.to_json();
        obj["text"] = json!(text);
        obj
    }
}

/// Parse `round-<N>-result.md` (1-based positive integer, no leading zeros).
pub fn parse_round_result_name(name: &str) -> Option<usize> {
    let rest = name.strip_prefix("round-")?.strip_suffix("-result.md")?;
    if rest.is_empty() || rest.starts_with('0') || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse::<usize>().ok().filter(|&n| n >= 1)
}

pub fn is_round_result_name(name: &str) -> bool {
    parse_round_result_name(name).is_some()
}

/// List preserved result files within a single run directory in ascending round order.
/// When `round-N-result.md` files exist, returns them in order (and appends `result.md` only if
/// it exists with non-empty content differing from the last preserved round). Otherwise falls
/// back to `result.md` as round 1.
pub fn intra_run_result_files(run: &Path) -> Vec<(usize, PathBuf)> {
    let mut numbered: Vec<(usize, PathBuf)> = fs::read_dir(run)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            let fname = entry.file_name();
            let n = parse_round_result_name(&fname.to_string_lossy())?;
            Some((n, path))
        })
        .collect();
    numbered.sort_by_key(|(n, _)| *n);

    let main_result = run.join("result.md");
    if numbered.is_empty() {
        if main_result.is_file() {
            return vec![(1, main_result)];
        }
        return Vec::new();
    }
    if main_result.is_file() {
        let main_text = read(&main_result);
        if !main_text.trim().is_empty() {
            let last_matches = numbered
                .last()
                .is_some_and(|(_, p)| read(p).trim_end_matches('\n') == main_text.trim_end_matches('\n'));
            if !last_matches {
                let next_n = numbered.last().map(|(n, _)| n + 1).unwrap_or(1);
                numbered.push((next_n, main_result));
            }
        }
    }
    numbered
}

fn find_parent_run(current: &Path, meta: &Value, parent_id: &str) -> Option<PathBuf> {
    if parent_id.is_empty() {
        return None;
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(base) = current.parent() {
        candidates.push(base.join(parent_id));
        candidates.push(base.join(".applied").join(parent_id));
    }
    let recorded_dir = s(meta, "dir");
    if !recorded_dir.is_empty() {
        if let Some(orig_base) = Path::new(recorded_dir).parent() {
            candidates.push(orig_base.join(parent_id));
            candidates.push(orig_base.join(".applied").join(parent_id));
        }
    }
    let root = runs_root();
    candidates.push(root.join(parent_id));
    candidates.push(root.join(".applied").join(parent_id));
    if let Some(old) = legacy_runs_root() {
        candidates.push(old.join(parent_id));
        candidates.push(old.join(".applied").join(parent_id));
    }
    candidates.push(cache_dir().join("archive").join(parent_id).join("record"));
    candidates.into_iter().find(|p| p.join("meta.json").is_file())
}

/// Walk `parent` links from `run` back to the root of its reply chain, returning run directories
/// ordered oldest -> `run` (zero locks, zero git subprocesses, includes archived ancestors).
pub fn chain_ancestors_to_run(run: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut current = run.to_path_buf();
    loop {
        let meta = json(current.join("meta.json"));
        let id = crate::notes::run_id(&current, &meta);
        if id.is_empty() || !seen.insert(id) {
            break;
        }
        let parent_id = s(&meta, "parent").to_string();
        let next = find_parent_run(&current, &meta, &parent_id);
        chain.push(current);
        match next {
            Some(p) => current = p,
            None => break,
        }
    }
    chain.reverse();
    chain
}

/// Collect all preserved author result rounds in chronological order.
/// When `include_chain` is true, walks `reply` ancestors (including archived records) as well as
/// intra-run review-rework rounds (`round-N-result.md`).
pub fn collect_result_rounds(run: &Path, include_chain: bool) -> Vec<ResultRound> {
    let dirs = if include_chain {
        chain_ancestors_to_run(run)
    } else {
        vec![run.to_path_buf()]
    };
    let mut out = Vec::new();
    for (run_idx, dir) in dirs.iter().enumerate() {
        let meta = json(dir.join("meta.json"));
        let rid = crate::notes::run_id(dir, &meta);
        for (intra_round, path) in intra_run_result_files(dir) {
            let raw = read(&path);
            let total_lines = raw.lines().count();
            let total_chars = raw.chars().count();
            let kind = if intra_round > 1 {
                "rework"
            } else if run_idx > 0 {
                "reply"
            } else {
                "initial"
            };
            let round = out.len() + 1;
            out.push(ResultRound {
                round,
                run_id: rid.clone(),
                intra_round,
                kind,
                path,
                total_lines,
                total_chars,
                current: false,
            });
        }
    }
    if let Some(last) = out.last_mut() {
        last.current = true;
    }
    out
}

/// Canonical per-run incremental token usage (`{input, output, cacheRead}`):
///
/// - For same-session `agy` reply runs, `summary.json["tokens"]` holds session-cumulative counts
///   while `summary.json["tokensDelta"]` holds this round's incremental usage; prefer `tokensDelta`.
/// - For root `agy` runs and all Pi runs, `summary.json["tokens"]` is already this run's
///   incremental usage (and `tokensDelta` is omitted).
///
/// Callers (`history`, `show`, `card`, and per-account `agy-usage`) should use this helper (or
/// read `tokensDelta` falling back to `tokens`) so summing across runs never double-counts.
pub fn effective_tokens_delta(summary: &Value) -> Option<Value> {
    for key in ["tokensDelta", "tokens"] {
        let v = &summary[key];
        if v.as_object().is_some_and(|o| !o.is_empty()) {
            return Some(v.clone());
        }
    }
    None
}

/// Format a `{input, output, cacheRead}` token usage object for human display.
pub fn format_tokens_usage(tokens: &Value) -> String {
    if !tokens.is_object() {
        return String::new();
    }
    let input = n(tokens, "input");
    let output = n(tokens, "output");
    let cache = n(tokens, "cacheRead");
    if input == 0 && output == 0 && cache == 0 {
        return String::new();
    }
    if cache > 0 {
        format!("in {input} / out {output} / cache {cache}")
    } else {
        format!("in {input} / out {output}")
    }
}

#[cfg(test)]
mod agy_conversation_tests {
    use super::*;
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

    #[test]
    fn parses_round_result_filenames_strictly() {
        assert_eq!(parse_round_result_name("round-1-result.md"), Some(1));
        assert_eq!(parse_round_result_name("round-12-result.md"), Some(12));
        assert_eq!(parse_round_result_name("round-0-result.md"), None);
        assert_eq!(parse_round_result_name("round-01-result.md"), None);
        assert_eq!(parse_round_result_name("result.md"), None);
        assert_eq!(parse_round_result_name("round-1-verdict.md"), None);
    }

    #[test]
    fn collects_intra_run_and_chain_rounds_in_order() {
        let base = std::env::temp_dir().join(format!("delegate-rounds-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let r1 = base.join("20261124-010000-task");
        let r2 = base.join("20261124-020000-reply");
        fs::create_dir_all(&r1).unwrap();
        fs::create_dir_all(&r2).unwrap();
        fs::write(r1.join("meta.json"), r#"{"run":"20261124-010000-task","name":"task"}"#).unwrap();
        fs::write(r1.join("round-1-result.md"), "r1 initial\np99=12ms\n").unwrap();
        fs::write(r1.join("round-2-result.md"), "r1 rework\n").unwrap();
        fs::write(r1.join("result.md"), "r1 rework\n").unwrap();
        fs::write(
            r2.join("meta.json"),
            r#"{"run":"20261124-020000-reply","name":"reply to task","parent":"20261124-010000-task"}"#,
        )
        .unwrap();
        fs::write(r2.join("result.md"), "r2 reply answer\n").unwrap();

        let all = collect_result_rounds(&r2, true);
        assert_eq!(all.len(), 3);
        assert_eq!((all[0].round, all[0].kind, all[0].current), (1, "initial", false));
        assert_eq!(all[0].path, r1.join("round-1-result.md"));
        assert_eq!((all[1].round, all[1].kind, all[1].current), (2, "rework", false));
        assert_eq!(all[1].path, r1.join("round-2-result.md"));
        assert_eq!((all[2].round, all[2].kind, all[2].current), (3, "reply", true));
        assert_eq!(all[2].path, r2.join("result.md"));

        let single = collect_result_rounds(&r2, false);
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].path, r2.join("result.md"));

        let _ = fs::remove_dir_all(&base);
    }
}
