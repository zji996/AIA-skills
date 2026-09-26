use crate::common::*;
use crate::worktree;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub fn all_runs() -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(runs_root()) else {
        return vec![];
    };
    let mut v = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("meta.json").is_file())
        .collect::<Vec<_>>();
    v.sort_by_key(|p| json(p.join("meta.json"))["startedNs"].as_u64().unwrap_or(0));
    v
}
pub fn resolve(reference: &str) -> Res<PathBuf> {
    let path = Path::new(reference);
    if path.join("meta.json").is_file() {
        return path.canonicalize().map_err(|e| e.to_string());
    }
    let root = runs_root();
    let runs = all_runs();
    if reference == "last" {
        return runs
            .last()
            .cloned()
            .ok_or_else(|| format!("no runs under {}", root.display()));
    }
    if root.join(reference).join("meta.json").is_file() {
        return Ok(root.join(reference));
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
pub fn state(run: &Path) -> String {
    if run.join("exit_code").is_file() {
        let sum = json(run.join("summary.json"));
        return sum
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("crashed")
            .into();
    }
    if supervisor_alive(run) {
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
pub fn active(s: &str) -> bool {
    s == "running" || s == "starting"
}
pub fn events(run: &Path) -> Vec<Value> {
    read(run.join("events.jsonl"))
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
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
    if state == "delivered" || state == "answered" {
        if s(meta, "mode") != "write" || n(&sum["changes"], "files") == 0 {
            return None;
        }
        if meta["worktree"].is_null() {
            return Some(format!(
                "review {script} diff {name}; the changes are already in the working tree"
            ));
        }
        if !run.join(".applied").exists() {
            return Some(format!(
                "review {script} diff {name} --total, then merge with {script} apply {name}"
            ));
        }
        return None;
    }
    match state {
        "rejected" => Some(format!(
            "read accept.tail; {script} reply {name} '<what to fix>' or take it over"
        )),
        "malformed" => Some("hand it to the other agent or do it yourself".into()),
        "failed" => Some("read error; fix the cause or take it over".into()),
        "timeout" => Some("split the task smaller or take it over".into()),
        "killed" | "crashed" => Some("take it over or start it again".into()),
        _ => None,
    }
}
pub fn status(run: &Path) -> Value {
    let meta = json(run.join("meta.json"));
    let st = state(run);
    let sum = json(run.join("summary.json"));
    let mut out = json!({"run":meta.get("run").and_then(Value::as_str).unwrap_or_else(||run.file_name().and_then(|x|x.to_str()).unwrap_or("")),"name":meta.get("name"),"state":st,"agent":meta.get("agent").and_then(Value::as_str).unwrap_or("pi"),"mode":meta.get("mode")});
    if !s(&meta, "parent").is_empty() {
        out["parent"] = meta["parent"].clone();
    }
    if meta["worktree"].is_object() {
        out["worktree"] = json!(s(&meta["worktree"], "path"));
    }
    if sum.is_object() {
        for key in [
            "elapsedSeconds",
            "attempts",
            "model",
            "turns",
            "files",
            "changes",
            "accept",
            "readOnlyViolation",
            "workspaceChanged",
            "queuedSeconds",
            "graceSeconds",
            "tokens",
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
            .map(|x| s(x, "path").to_string())
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
    let result = run.join("result.md");
    if result.is_file() && fs::metadata(&result).is_ok_and(|m| m.len() > 0) {
        out["result"] = json!(result.to_string_lossy());
        out["resultChars"] = json!(read(&result).chars().count());
    }
    if let Some(step) = next_step(run, &meta, &st, &sum) {
        out["next"] = json!(step);
    }
    out["dir"] = json!(run.to_string_lossy());
    out
}
pub fn unmerged(run: &Path) -> bool {
    let m = json(run.join("meta.json"));
    s(&m, "mode") == "write"
        && m["worktree"].is_object()
        && Path::new(s(&m["worktree"], "path")).exists()
        && !run.join(".applied").exists()
}
pub fn remove(run: &Path) {
    let meta = json(run.join("meta.json"));
    let path = s(&meta["worktree"], "path").to_string();
    let _ = fs::remove_dir_all(run);
    if !path.is_empty()
        && !all_runs()
            .iter()
            .any(|r| s(&json(r.join("meta.json"))["worktree"], "path") == path)
    {
        worktree::remove(&meta);
    }
}
pub fn prune() {
    let Ok(days) = setting("KEEP_DAYS", "7").parse::<u64>() else {
        return;
    };
    if days == 0 {
        return;
    }
    for run in all_runs() {
        let done = run.join("exit_code");
        if run.join(".delivered").is_file() && done.is_file() && !unmerged(&run) {
            use std::time::UNIX_EPOCH;
            let age = fs::metadata(&done)
                .and_then(|x| x.modified())
                .ok()
                .and_then(|x| x.duration_since(UNIX_EPOCH).ok())
                .map(|x| epoch() - x.as_secs_f64())
                .unwrap_or(0.0);
            if age > days as f64 * 86400.0 {
                remove(&run);
            }
        }
    }
}
pub fn latest(mut run: PathBuf) -> PathBuf {
    loop {
        let mut replies = all_runs()
            .into_iter()
            .filter(|r| {
                s(&json(r.join("meta.json")), "parent")
                    == run.file_name().unwrap_or_default().to_string_lossy()
                    && state(r) != "malformed"
            })
            .collect::<Vec<_>>();
        if let Some(next) = replies.pop() {
            run = next;
        } else {
            return run;
        }
    }
}
