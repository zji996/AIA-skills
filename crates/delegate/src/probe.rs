use crate::agents;
use crate::common::*;
use crate::config;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};

/// The request probe sends: one short answer, so a failure is the provider's, not the task's.
const PROMPT: &str = "只回复 ok";

struct Args {
    tier: Option<String>,
    model: Option<String>,
    timeout: f64,
}

struct Outcome {
    ok: bool,
    seconds: f64,
    first: Option<f64>,
    error: String,
}

/// Events read so far, independent of the agent's streaming details.
#[derive(Default)]
struct Seen {
    ok: bool,
    first: Option<f64>,
    error: String,
    stray: String,
}

fn parse(args: &[String]) -> Res<Args> {
    let mut out = Args {
        tier: None,
        model: None,
        timeout: 60.0,
    };
    let mut i = 0;
    while i < args.len() {
        let (key, inline) = args[i]
            .split_once('=')
            .map_or((args[i].as_str(), None), |(k, v)| (k, Some(v.to_string())));
        let name = match key {
            "--tier" | "--model" | "--timeout" => key,
            other if other.starts_with('-') => {
                return Err(format!("unrecognized arguments: {other}"))
            }
            other => return Err(format!("probe takes no arguments: {other}")),
        };
        let value = match inline {
            Some(v) => v,
            None => {
                i += 1;
                args.get(i)
                    .cloned()
                    .ok_or_else(|| format!("argument {name}: expected one argument"))?
            }
        };
        match name {
            "--tier" => {
                if !agents::ALL_TIERS.contains(&value.as_str()) {
                    return Err(format!(
                        "--tier: unknown tier {value} (default, cheap or strong)"
                    ));
                }
                out.tier = Some(value);
            }
            "--model" => out.model = Some(value),
            _ => out.timeout = seconds(&value)?,
        }
        i += 1;
    }
    if out.tier.is_some() && out.model.is_some() {
        return Err("--tier and --model are exclusive".into());
    }
    Ok(out)
}

/// The model id without its `:thinking` suffix; like Pi, only a last level counts as one.
fn model_id(pattern: &str) -> &str {
    pattern
        .rsplit_once(':')
        .filter(|(_, level)| agents::THINKING.contains(level))
        .map_or(pattern, |(model, _)| model)
}

fn note_line(rule: &Value, now: agents::Clock, running: &dyn Fn(&str) -> usize) -> String {
    let mut parts = vec![];
    if let Some(days) = rule["days"].as_str() {
        parts.push(days.to_string());
    }
    if let Some(hours) = rule["hours"].as_str() {
        parts.push(hours.to_string());
    }
    if let Some(cap) = rule["maxActive"].as_u64() {
        parts.push(format!("maxActive {}/{cap}", running(s(rule, "model"))));
    }
    parts.push(
        if agents::rule_matches(rule, now, running) {
            "hit"
        } else {
            "miss"
        }
        .to_string(),
    );
    parts.join(" ")
}

/// Where this model sits in each tier: `cheap[mon-fri 09:00-12:00 hit]` or `default`, plus
/// `=now` for the model the tier picks at this minute (`picks` is one snapshot of that).
fn tier_notes(
    config: &Value,
    picks: &Value,
    model: &str,
    now: agents::Clock,
    running: &dyn Fn(&str) -> usize,
) -> Vec<String> {
    let mut notes = vec![];
    for tier in agents::ALL_TIERS {
        let value = &config["tiers"][tier];
        let current = model_id(s(picks, tier)) == model_id(model);
        // A rule list names models; a single model or an environment override
        // (`DELEGATE_<TIER>_MODEL`) has no rule to describe, so `=now` says it all.
        let mut parts = vec![];
        if let Some(rules) = value.as_array() {
            for rule in rules {
                match rule.as_str() {
                    Some(m) if model_id(m) == model_id(model) => parts.push("else".to_string()),
                    Some(_) => {}
                    None if model_id(s(rule, "model")) == model_id(model) => {
                        parts.push(note_line(rule, now, running))
                    }
                    None => {}
                }
            }
        }
        if parts.is_empty() {
            if current {
                notes.push(format!("{tier}=now"));
            }
            continue;
        }
        let mut note = format!("{tier}[{}]", parts.join(", "));
        if current {
            note.push_str("=now");
        }
        notes.push(note);
    }
    notes
}

fn push_event(seen: &mut Seen, value: &Value, at: f64) {
    let kind = s(value, "type").to_string();
    if seen.first.is_none() && (kind.contains("delta") || kind == "message_start") {
        seen.first = Some(at);
    }
    let fail = |seen: &mut Seen, message: &str| {
        seen.ok = false;
        seen.error = message.lines().next().unwrap_or("").trim().to_string();
    };
    match kind.as_str() {
        "message_end" => {
            let message = &value["message"];
            if s(message, "role") != "assistant" {
                return;
            }
            if seen.first.is_none() {
                seen.first = Some(at);
            }
            let error = s(message, "errorMessage");
            if !error.is_empty() {
                fail(seen, error);
                return;
            }
            match s(message, "stopReason") {
                "stop" | "length" => seen.ok = true,
                "error" | "aborted" => fail(seen, "stopReason error"),
                _ => {}
            }
        }
        // rpi's print mode ends with `result` instead of Pi's `agent_settled`.
        "result" => {
            if seen.first.is_none() {
                seen.first = Some(at);
            }
            match s(value, "outcome") {
                "completed" => seen.ok = true,
                "" => {}
                other => {
                    let detail = [s(value, "error"), s(value, "message")]
                        .into_iter()
                        .find(|text| !text.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("outcome {other}"));
                    fail(seen, &detail);
                }
            }
        }
        "turn_error" | "error" => {
            let detail = [s(value, "detail"), s(value, "message"), s(value, "error")]
                .into_iter()
                .find(|text| !text.is_empty())
                .unwrap_or("error");
            fail(seen, detail);
        }
        "auto_retry_start" => {
            let detail = s(value, "errorMessage");
            if !detail.is_empty() {
                fail(seen, detail);
            }
        }
        _ => {}
    }
}

/// One Pi print-mode request, in its own temporary directory and with the same binary and
/// environment a real dispatch uses. Never touches a repository or a run directory.
fn probe_one(
    bin: &Path,
    pattern: &str,
    index: usize,
    root: &Path,
    timeout: f64,
    extra_env: &Value,
    thinking_off: bool,
) -> Outcome {
    let started = Instant::now();
    let result = |ok: bool, error: String, first: Option<f64>| Outcome {
        ok,
        seconds: started.elapsed().as_secs_f64(),
        first,
        error,
    };
    let dir = root.join(format!("m{index}"));
    if let Err(e) = fs::create_dir_all(dir.join("session")) {
        return result(false, e.to_string(), None);
    }
    let prompt = dir.join("prompt.md");
    if let Err(e) = write(&prompt, PROMPT) {
        return result(false, e.to_string(), None);
    }
    let stderr_path = dir.join("stderr.log");
    let Ok(stderr) = fs::File::create(&stderr_path) else {
        return result(false, "cannot create stderr log".into(), None);
    };
    let Ok(stdin) = fs::File::open(&prompt) else {
        return result(false, "cannot read the probe prompt".into(), None);
    };
    let mut command = Command::new(bin);
    command
        .arg("--session-dir")
        .arg(dir.join("session"))
        .arg("--mode")
        .arg("json")
        .arg("--model")
        .arg(model_id(pattern));
    if thinking_off {
        command.args(["--thinking", "off"]);
    }
    command
        .arg("-p")
        .current_dir(&dir)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr));
    clean_env(&mut command, extra_env);
    command
        .env("DELEGATE_AGENT", agents::AGENT)
        .env("PI_DELEGATE_ACTIVE", "1");
    group(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return result(false, e.to_string(), None),
    };
    let pid = child.id() as i32;
    let Some(stdout) = child.stdout.take() else {
        kill_group(pid, 1.0);
        let _ = child.wait();
        return result(false, "cannot read Pi's output".into(), None);
    };
    let (tx, rx) = channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&raw).trim_end().to_string();
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
    let mut seen = Seen::default();
    let deadline = started + Duration::from_secs_f64(timeout);
    let mut timed_out = false;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => {
                let at = started.elapsed().as_secs_f64();
                match serde_json::from_str::<Value>(&line) {
                    Ok(event) => push_event(&mut seen, &event, at),
                    Err(_) if seen.stray.is_empty() => {
                        if !line.trim().is_empty() {
                            seen.stray = line.trim().to_string();
                        }
                    }
                    Err(_) => {}
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                timed_out = true;
                break;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // Stop the request before waiting for it: the reader only sees end-of-file once the
    // process is gone, so joining first would make --timeout wait for the model anyway.
    if timed_out {
        kill_group(pid, 1.0);
    }
    let status = if timed_out {
        child.wait().ok()
    } else {
        let grace = Instant::now() + Duration::from_secs(2);
        loop {
            match child.try_wait() {
                Ok(Some(done)) => break Some(done),
                _ if Instant::now() >= grace => {
                    kill_group(pid, 1.0);
                    break child.wait().ok();
                }
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    };
    let _ = reader.join();
    let stderr = read(&stderr_path)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string();
    let exited = status.is_some_and(|s| s.success());
    let ok = !timed_out && seen.ok && exited;
    let mut error = seen.error;
    if error.is_empty() {
        error = stderr;
    }
    if error.is_empty() {
        error = seen.stray;
    }
    if timed_out {
        error = format!("timeout {timeout}s");
    } else if !ok && error.is_empty() {
        error = match status.and_then(|done| done.code()) {
            Some(code) => format!("exit {code} without an answer"),
            None => "no answer".into(),
        };
    }
    result(ok, clip(&error, 300), seen.first)
}

/// Whether `model` answers right now, asked exactly the way `probe` asks (same binary,
/// environment and thinking level). A run resuming after an interrupted upstream waits for this
/// instead of sending the task into a provider that is still down.
pub fn recovered(bin: &Path, model: &str, env: &Value, timeout: f64) -> Result<(), String> {
    // Pi's own default model has no id to probe; nothing to check, so let the resume go out.
    if model.trim().is_empty() {
        return Ok(());
    }
    let root = std::env::temp_dir().join(format!("delegate-probe-{}", now_ns()));
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let thinking = !agents::pi_models()
        .iter()
        .any(|(name, thinking)| name == model_id(model) && !*thinking);
    let outcome = probe_one(bin, model, 0, &root, timeout, env, thinking);
    fs::remove_dir_all(&root).ok();
    if outcome.ok {
        Ok(())
    } else {
        Err(outcome.error)
    }
}

/// Check the connectivity of each tier model through Pi itself. Same binary, environment and
/// provider configuration as `start`; no worktree, no run directory, no capacity slot.
pub fn command(args: &[String]) -> Res<i32> {
    let o = parse(args)?;
    agents::missing_tools(agents::AGENT)?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let config = config::load(git_top(&cwd).as_deref())?;
    let active = crate::launch::active_machine(&state_dir());
    let busy: Vec<String> = active
        .iter()
        .map(|run| s(&json(run.join("meta.json")), "model").to_string())
        .collect();
    let running = |model: &str| busy.iter().filter(|m| *m == model).count();
    // One clock and one occupancy snapshot for the whole report: `now` decides both the
    // rules it prints and the model the tiers pick, so the last line cannot disagree.
    let now = agents::local_clock();
    let picks: Value = agents::ALL_TIERS
        .iter()
        .map(|tier| {
            (
                tier.to_string(),
                Value::String(agents::tier_model_with(&config.value, tier, &running)),
            )
        })
        .collect::<serde_json::Map<_, _>>()
        .into();
    let tiers: Vec<&str> = match &o.tier {
        Some(tier) => vec![tier.as_str()],
        None => agents::ALL_TIERS.to_vec(),
    };
    let mut jobs: Vec<String> = vec![];
    for tier in &tiers {
        let models = agents::tier_candidates(&config.value, tier);
        if o.tier.is_some() && models.is_empty() {
            return Err(format!(
                "tier {tier} has no model; set it with: {} tiers --{tier} <provider/model[:thinking]>",
                script().display()
            ));
        }
        for model in models {
            // Either way the same request goes out (`--thinking off` is added), so a model
            // named in two tiers is probed once.
            if !jobs.iter().any(|job| model_id(job) == model_id(&model)) {
                jobs.push(model);
            }
        }
    }
    let listed = agents::pi_models();
    if let Some(model) = &o.model {
        agents::check_tier_model(model, &listed)?;
        jobs = vec![model.clone()];
    }
    if jobs.is_empty() {
        return Err(format!(
            "no model to probe: set the tiers first: {} tiers --cheap <provider/model[:thinking]> --strong <provider/model[:thinking]>",
            script().display()
        ));
    }
    let bin = agents::agent_bin(agents::AGENT).ok_or("missing required tools: pi")?;
    // `off` keeps the probe cheap: the lowest thinking level Pi offers. A model that Pi
    // says has no thinking keeps its own default rather than an unsupported flag.
    let off = |model: &str| {
        !listed
            .iter()
            .any(|(name, thinking)| name == model_id(model) && !*thinking)
    };
    let root = std::env::temp_dir().join(format!("delegate-probe-{}", now_ns()));
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let results: Vec<Outcome> = std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .iter()
            .enumerate()
            .map(|(index, model)| {
                let (bin, root, env) = (bin.clone(), root.clone(), config.value["env"].clone());
                let thinking = off(model);
                scope.spawn(move || probe_one(&bin, model, index, &root, o.timeout, &env, thinking))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or(Outcome {
                        ok: false,
                        seconds: 0.0,
                        first: None,
                        error: "probe thread panicked".into(),
                    })
            })
            .collect()
    });
    fs::remove_dir_all(&root).ok();
    let width = jobs
        .iter()
        .map(|model| model.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, 48);
    for (model, outcome) in jobs.iter().zip(&results) {
        let status = if outcome.ok { "ok" } else { "FAIL" };
        let first = outcome
            .first
            .map(|t| format!("  首字 {t:.1}s"))
            .unwrap_or_else(|| "  首字 -".into());
        let notes = tier_notes(&config.value, &picks, model, now, &running);
        let tiers = if notes.is_empty() {
            "-".to_string()
        } else {
            notes.join(" ")
        };
        let error = if outcome.ok {
            String::new()
        } else {
            format!("  error: {}", outcome.error)
        };
        println!(
            "{status:<4} {model:<width$} {:>5.1}s{first}  {tiers}{error}",
            outcome.seconds
        );
    }
    let default = s(&picks, "default").to_string();
    let candidates: Vec<(&str, String)> = if default.is_empty() {
        ["cheap", "strong"]
            .into_iter()
            .map(|tier| (tier, s(&picks, tier).to_string()))
            .collect()
    } else {
        vec![("default", default)]
    };
    let mut failed = false;
    let picked = candidates
        .iter()
        .map(|(tier, model)| {
            let outcome = jobs
                .iter()
                .position(|job| model_id(job) == model_id(model))
                .map(|index| &results[index]);
            let state = match outcome {
                None => "未测",
                Some(outcome) if outcome.ok => "ok",
                Some(_) => {
                    failed = true;
                    "FAIL"
                }
            };
            let who = if candidates.len() > 1 {
                format!("{} {model}", if *tier == "cheap" { "只读 cheap" } else { "写入 strong" })
            } else {
                model.clone()
            };
            format!("{who}（{state}）")
        })
        .collect::<Vec<_>>()
        .join("、");
    println!("start 不带 --tier 现在选：{picked}");
    Ok(if failed { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn notes_describe_rule_windows_and_the_current_pick() {
        let config = json!({"tiers": {"cheap": "p/fast", "strong": [
            {"model": "p/peak", "days": "mon-fri", "hours": "09:00-12:00", "maxActive": 3},
            "p/fast"]}});
        let idle = |_: &str| 0;
        let peak = json!({"cheap": "p/fast", "strong": "p/peak"});
        let fallback = json!({"cheap": "p/fast", "strong": "p/fast"});
        // Wednesday 10:00: the peak rule holds, so it is also what the tier picks.
        assert_eq!(
            tier_notes(&config, &peak, "p/peak", agents::Clock { weekday: 3, minute: 600 }, &idle),
            ["strong[mon-fri 09:00-12:00 maxActive 0/3 hit]=now"]
        );
        // Wednesday 13:00: outside the window the fallback model runs.
        assert_eq!(
            tier_notes(
                &config,
                &fallback,
                "p/fast",
                agents::Clock { weekday: 3, minute: 780 },
                &idle
            ),
            ["cheap=now", "strong[else]=now"]
        );
        // At its cap the peak rule steps aside even inside its window.
        let busy = |model: &str| if model == "p/peak" { 3 } else { 0 };
        assert_eq!(
            tier_notes(
                &config,
                &fallback,
                "p/peak",
                agents::Clock { weekday: 3, minute: 600 },
                &busy
            ),
            ["strong[mon-fri 09:00-12:00 maxActive 3/3 miss]"]
        );
    }

    #[test]
    fn models_dedupe_by_id_and_arguments_reject_usage_errors() {
        assert_eq!(model_id("p/m:high"), "p/m");
        assert_eq!(model_id("ollama/qwen2.5-coder:7b"), "ollama/qwen2.5-coder:7b");
        let parse_ok = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            parse(&args)
        };
        let o = parse_ok(&["--timeout", "1m"]).unwrap();
        assert_eq!(o.timeout, 60.0);
        assert!(parse_ok(&["--tier", "cheap"]).is_ok());
        assert!(parse_ok(&["--tier", "huge"]).is_err());
        assert!(parse_ok(&["--tier", "cheap", "--model", "p/m"]).is_err());
        assert!(parse_ok(&["surprise"]).is_err());
    }
}
