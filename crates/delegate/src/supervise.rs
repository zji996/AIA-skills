use crate::agents;
use crate::changes;
use crate::common::*;
use crate::lane;
use crate::launch;
use crate::runs;
use crate::worktree;
use serde_json::{json, Value};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicI32, Ordering},
    Arc,
};

type Recorded = Option<(Vec<Value>, Value)>;

pub fn log_event(run: &Path, mut event: Value) {
    event["at"] = json!(iso());
    let _ = append(run.join("events.jsonl"), &format!("{event}\n"));
}
fn accept(meta: &Value, run: &Path, holder: Arc<AtomicI32>) -> Value {
    let mut result = json!({"command":s(meta,"accept")});
    let label = format!(
        "accept {}",
        run.file_name().unwrap_or_default().to_string_lossy()
    );
    let slot = lane::acquire(&label, None, |ahead, _| {
        log_event(run, json!({"e":"queue","ahead":ahead}))
    });
    let Ok(slot) = slot else {
        result["ok"] = json!(false);
        result["exitCode"] = Value::Null;
        result["tail"] = json!("stopped while queued for the heavy lane");
        return result;
    };
    if lane::stopped() {
        result["ok"] = json!(false);
        result["exitCode"] = Value::Null;
        result["tail"] = json!("stopped while queued for the heavy lane");
        return result;
    }
    let path = run.join("accept.log");
    let mut log = match File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            result["ok"] = json!(false);
            result["exitCode"] = json!(1);
            result["tail"] = json!(e.to_string());
            return result;
        }
    };
    let _ = writeln!(log, "$ {}", s(meta, "accept"));
    let _ = log.flush();
    let (code, timed) = run_shell(
        s(meta, "accept"),
        Path::new(s(meta, "workdir")),
        &meta["env"],
        meta["acceptTimeoutSeconds"].as_f64().unwrap_or(600.0),
        &mut log,
        Some(&holder),
    )
    .unwrap_or((1, false));
    let _ = writeln!(
        log,
        "{}\n[exit {code}]",
        if timed { "\n[accept timed out]" } else { "" }
    );
    if slot.queued >= 1.0 {
        result["queuedSeconds"] = json!(slot.queued as i64);
    }
    result["ok"] = json!(code == 0);
    result["exitCode"] = json!(code);
    if code != 0 {
        let text = read(&path);
        let body = text.split_once('\n').map(|(_, x)| x).unwrap_or("");
        let body = body.rsplit_once("\n[exit ").map(|(x, _)| x).unwrap_or(body);
        result["tail"] = json!(body
            .trim()
            .chars()
            .rev()
            .take(1500)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>());
    }
    result
}
fn relative(path: &str, workdir: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        p.strip_prefix(workdir)
            .map(|x| x.to_string_lossy().into())
            .unwrap_or_else(|_| path.into())
    } else {
        path.into()
    }
}
fn attempts_from(
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
    (verdict, answer, attempt)
}
fn settle(
    meta: &Value,
    run: &Path,
    verdict: &str,
    setup_error: &Option<String>,
    holder: Arc<AtomicI32>,
) -> (String, Value, Recorded, Vec<String>) {
    let mut state = if verdict == "ok" {
        "answered".to_string()
    } else {
        verdict.into()
    };
    let mut sum = json!({"state":state});
    let rec = if setup_error.is_none() {
        changes::record(meta, run)
    } else {
        None
    };
    let changed = rec
        .as_ref()
        .map(|x| {
            x.0.iter()
                .map(|c| s(c, "path").to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some((_, totals)) = &rec {
        sum["changes"] = totals.clone();
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
    if verdict == "ok" && !s(meta, "accept").is_empty() && !lane::stopped() {
        sum["accept"] = accept(meta, run, holder);
        state = if b(&sum["accept"], "ok") {
            "delivered"
        } else {
            "rejected"
        }
        .into();
        sum["state"] = json!(state);
    }
    (state, sum, rec, changed)
}
fn escalate(
    meta: &mut Value,
    run: &Path,
    state: &str,
    recorded: bool,
    changed: &[String],
) -> Res<bool> {
    if s(meta, "tier") != "cheap"
        || !["malformed", "failed", "timeout", "rejected"].contains(&state)
    {
        return Ok(false);
    }
    if s(meta, "mode") != "read-only" && (!recorded || !changed.is_empty()) {
        return Ok(false);
    }
    let strong = launch::strong_agent()?;
    if strong == s(meta, "agent") || !agents::agent_available(&strong) {
        return Ok(false);
    }
    let slots = state_dir();
    let _guard = lock(&slots.join(".start.lock"), true, false).map_err(|e| e.to_string())?;
    let others: Vec<PathBuf> = launch::active_machine(&slots)
        .into_iter()
        .filter(|r| r != run)
        .collect();
    if launch::capacity(&strong, &others).is_err() {
        log_event(
            run,
            json!({"e":"escalate_skipped","reason":format!("no room for {strong}")}),
        );
        return Ok(false);
    }
    if s(meta, "mode") == "read-only" && strong == "codex" {
        let prompt = read(run.join("prompt.md"));
        write(
            run.join("prompt.md"),
            launch::contract(&prompt, None, true, false),
        )?;
    }
    meta["escalatedFrom"] = meta["agent"].clone();
    meta["agent"] = json!(strong);
    meta["tier"] = json!("strong");
    meta["fork"] = Value::Null;
    write_json(run.join("meta.json"), meta)?;
    Ok(true)
}
fn inner(run: &Path, holder: Arc<AtomicI32>, grace: Arc<std::sync::Mutex<Option<f64>>>) -> Res<()> {
    let mut meta = json(run.join("meta.json"));
    let started = epoch();
    let mut setup_error = None;
    if meta["worktree"].is_object() && !Path::new(s(&meta["worktree"], "path")).exists() {
        setup_error = match worktree::prepare(&meta, run) {
            Ok(error) => error,
            Err(e) => Some(clip(&format!("worktree setup failed: {e}"), 300)),
        };
    }
    let (mut verdict, mut answer, mut attempts) = if setup_error.is_none() {
        attempts_from(&meta, run, 1, holder.clone(), grace.clone())
    } else {
        ("failed".into(), String::new(), 0)
    };
    let (mut state, mut sum, mut rec, mut changed) =
        settle(&meta, run, &verdict, &setup_error, holder.clone());
    let cheap = s(&meta, "agent").to_string();
    if !lane::stopped() && escalate(&mut meta, run, &state, rec.is_some(), &changed)? {
        log_event(
            run,
            json!({"e":"escalate","from":cheap,"to":s(&meta,"agent"),"after":state}),
        );
        (verdict, answer, attempts) =
            attempts_from(&meta, run, attempts + 1, holder.clone(), grace.clone());
        (state, sum, rec, changed) = settle(&meta, run, &verdict, &setup_error, holder.clone());
        sum["escalatedFrom"] = json!(cheap);
    }
    sum["attempts"] = json!(attempts);
    if !answer.trim().is_empty() {
        write(
            run.join("result.md"),
            format!("{}\n", answer.trim_end_matches('\n')),
        )?;
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
            .map(|e| relative(s(e, "path"), s(&meta, "workdir")))
            .collect::<Vec<_>>()
    };
    files.sort();
    files.dedup();
    let sessions = evs
        .iter()
        .filter(|x| s(x, "e") == "session" && !s(x, "id").is_empty())
        .collect::<Vec<_>>();
    if let Some(last) = sessions.last() {
        sum["session"] = json!(s(last, "id"));
    }
    let mut tokens = json!({});
    for key in ["input", "output", "cacheRead"] {
        tokens[key] = json!(turns.iter().map(|x| n(&x["usage"], key)).sum::<i64>());
    }
    sum["elapsedSeconds"] = json!((epoch() - started) as i64);
    sum["model"] = turns
        .last()
        .map(|x| x["model"].clone())
        .unwrap_or(Value::Null);
    sum["turns"] = json!(turns.len());
    sum["files"] = json!(files);
    sum["tokens"] = tokens;
    if lane::queued_seconds(run) >= 1.0 {
        sum["queuedSeconds"] = json!(lane::queued_seconds(run) as i64);
    }
    if let Ok(x) = grace.lock() {
        if let Some(g) = *x {
            sum["graceSeconds"] = json!(g);
        }
    }
    if !["delivered", "answered", "rejected", "stopped"].contains(&state.as_str()) {
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
        let hint = match state.as_str() {
            "malformed" => "answer was empty or a leaked tool call".into(),
            "timeout" => format!("{} exceeded {}", s(&meta, "agent"), s(&meta, "timeout")),
            _ => String::new(),
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
    write_json(run.join("summary.json"), &sum)?;
    write(
        run.join("exit_code"),
        if ["delivered", "answered"].contains(&state.as_str()) {
            "0\n"
        } else {
            "1\n"
        },
    )?;
    Ok(())
}
pub fn supervise(run: &Path) -> Res<()> {
    let _life = locked_file(
        &run.join("supervisor.lock"),
        &std::process::id().to_string(),
    )?;
    write(run.join("pid"), std::process::id().to_string())?;
    let mut signals = lane::supervisor_signal_pipe().map_err(|e| e.to_string())?;
    let holder = Arc::new(AtomicI32::new(0));
    let grace = Arc::new(std::sync::Mutex::new(None));
    let h = holder.clone();
    std::thread::spawn(move || {
        let mut byte = [0];
        if signals.read_exact(&mut byte).is_ok() {
            lane::wake_lane_waiter_after_stop();
            let pid = h.load(Ordering::SeqCst);
            if pid > 0 {
                kill_group(pid, 5.0);
            }
        }
    });
    if let Err(e) = inner(run, holder, grace) {
        let _ = append(run.join("stderr.log"), &format!("supervisor error: {e}\n"));
        if !run.join("exit_code").exists() {
            let _ = write_json(
                run.join("summary.json"),
                &json!({"state":"failed","error":clip(&e,600)}),
            );
            let _ = write(run.join("exit_code"), "1\n");
        }
    }
    Ok(())
}
