use crate::common::*;
use crate::lane;
use serde_json::{json, Value};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, AtomicI32, Ordering},
    Arc, Condvar, Mutex,
};
use std::time::{Duration, Instant};

struct WatchState {
    finished: bool,
    last: Instant,
    commands: i32,
    command_started: Option<Instant>,
    // Lane queue seconds already counted when the command started; queueing
    // for the heavy lane is not the command being stuck.
    command_queued: f64,
    command: String,
}

/// Harness-facing protocol version; bump when docs/delegate-protocol.md §2 breaks.
pub const PROTOCOL: u32 = 1;

/// The one colleague. Tiers choose its model, not another agent.
pub const AGENT: &str = "pi";
/// Tiers that must name a model; `default` is optional and, when set, runs
/// whatever the caller did not give a tier.
pub const TIERS: [&str; 2] = ["cheap", "strong"];
pub const ALL_TIERS: [&str; 3] = ["default", "cheap", "strong"];

/// The Pi `--model` pattern a tier runs (`provider/id[:thinking]`); empty means
/// Pi's own default from its settings.json. A tier may be a rule list, resolved
/// at the local time of the call (start, reply, `tiers`).
pub fn tier_model(config: &Value, tier: &str) -> String {
    tier_model_with(config, tier, &|_| 0)
}

/// `tier_model` given how many runs on this machine already use each model, which
/// rules with `maxActive` read; `start` asks under the machine start lock.
pub fn tier_model_with(config: &Value, tier: &str, running: &dyn Fn(&str) -> usize) -> String {
    let configured = resolve_tier(&config["tiers"][tier], local_clock(), running);
    setting(&format!("{}_MODEL", tier.to_uppercase()), &configured)
}

/// The local weekday (1 = Monday … 7 = Sunday) and minutes since midnight, the clock tier rules read.
#[derive(Clone, Copy)]
pub struct Clock {
    pub weekday: u32,
    pub minute: u32,
}

pub fn local_clock() -> Clock {
    let now = format_time(true, "%u %H:%M");
    let (day, time) = now.split_once(' ').unwrap_or(("1", "00:00"));
    let (h, m) = time
        .split_once(':')
        .map(|(h, m)| (h.parse().unwrap_or(0), m.parse().unwrap_or(0)))
        .unwrap_or((0, 0));
    Clock {
        weekday: day.parse().unwrap_or(1),
        minute: h * 60 + m,
    }
}

/// `"HH:MM-HH:MM"` as minutes; the end is exclusive and may wrap past midnight.
pub fn parse_hours(hours: &str) -> Option<(u32, u32)> {
    let clock = |t: &str| {
        let (h, m) = t.trim().split_once(':')?;
        let (h, m) = (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?);
        (h < 24 && m < 60 && t.trim().len() == 5).then_some(h * 60 + m)
    };
    let (start, end) = hours.split_once('-')?;
    let (start, end) = (clock(start)?, clock(end)?);
    (start != end).then_some((start, end))
}

/// One or more windows joined by commas, e.g. `"09:00-12:00,14:00-18:00"`.
pub fn parse_windows(hours: &str) -> Option<Vec<(u32, u32)>> {
    hours.split(',').map(parse_hours).collect()
}

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// Weekdays as a set of 1 (Monday) … 7: `"mon-fri"`, `"sat,sun"`, or both mixed.
pub fn parse_days(days: &str) -> Option<Vec<u32>> {
    let day = |d: &str| {
        DAYS.iter()
            .position(|name| *name == d.trim())
            .map(|i| i as u32 + 1)
    };
    let mut set = Vec::new();
    for part in days.split(',') {
        match part.split_once('-') {
            Some((from, to)) => {
                let (from, to) = (day(from)?, day(to)?);
                if from > to {
                    return None;
                }
                set.extend(from..=to);
            }
            None => set.push(day(part)?),
        }
    }
    Some(set)
}

/// A tier is a model string, or rules tried in order: `{"model", "days"?, "hours"?, "maxActive"?}`
/// matches when the local day and time fall inside it and fewer than `maxActive` runs on this
/// machine use its model; a plain string always matches.
/// A window that wraps midnight counts against the day it started on only for its first part.
pub fn resolve_tier(value: &Value, now: Clock, running: &dyn Fn(&str) -> usize) -> String {
    if let Some(model) = value.as_str() {
        return model.to_string();
    }
    let within = |hours: &str| {
        parse_windows(hours).is_some_and(|windows| {
            windows.iter().any(|&(start, end)| {
                if start < end {
                    (start..end).contains(&now.minute)
                } else {
                    now.minute >= start || now.minute < end
                }
            })
        })
    };
    let matches = |rule: &Value| {
        let day = rule["days"]
            .as_str()
            .is_none_or(|days| parse_days(days).is_some_and(|set| set.contains(&now.weekday)));
        let time = rule["hours"].as_str().is_none_or(within);
        let room = rule["maxActive"]
            .as_u64()
            .is_none_or(|cap| (running(s(rule, "model")) as u64) < cap);
        day && time && room
    };
    value
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|rule| match rule {
            Value::String(model) => Some(model.clone()),
            _ => matches(rule).then(|| rule["model"].as_str().unwrap_or("").to_string()),
        })
        .unwrap_or_default()
}

/// A rule list as one line for `delegate tiers`, e.g. `mon-fri 09:00-12:00 a, else b`.
pub fn describe_tier(value: &Value) -> Option<String> {
    let rules = value.as_array()?;
    Some(
        rules
            .iter()
            .map(|rule| match rule.as_str() {
                Some(model) => format!("else {model}"),
                None => {
                    let cap = rule["maxActive"].as_u64().map(|n| format!("<{n} running"));
                    let when = [
                        rule["days"].as_str(),
                        rule["hours"].as_str(),
                        cap.as_deref(),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                    format!("{} {}", when.join(" "), s(rule, "model"))
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// Fixed system-prompt addendum for every colleague: the same text each run, so
/// it stays in the cached prefix. Asking first is cheaper than a wrong build.
pub const COLLEAGUE: &str = "You are a delegated colleague: a delegator gave you this task and reviews your answer; nobody watches while you work.
- When the way forward is unclear, the brief contradicts the code, or the task would need a large change the brief did not ask for, stop and ask instead of guessing: start the answer with the line `需要主控决定` (`Needs a decision` if the brief is in English), then the question, the option you lean to and why. The delegator replies, and answering you does not count as rework.
- Otherwise finish the work yourself; do not stop at a plan.
- After a context compaction, continue from the summary; do not restart or redo finished work.
- Answer in the brief's language: the conclusion first, then evidence (file:line) and what you did not verify.
";
pub const QUESTION_MARKERS: [&str; 2] = ["需要主控决定", "Needs a decision"];

/// The colleague stopped to ask: its answer opens with a question marker.
pub fn is_question(answer: &str) -> bool {
    answer
        .lines()
        .map(|line| line.trim().trim_start_matches(['#', '*', '>', ' ']))
        .find(|line| !line.is_empty())
        .is_some_and(|line| QUESTION_MARKERS.iter().any(|m| line.starts_with(m)))
}

pub const THINKING: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Pi's models as (`provider/id`, supports thinking), from `pi --list-models`;
/// empty when Pi is missing or prints nothing usable.
pub fn pi_models() -> Vec<(String, bool)> {
    let Some(bin) = agent_bin(AGENT) else {
        return vec![];
    };
    let out = std::process::Command::new(bin)
        .args(["--offline", "--list-models"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else { return vec![] };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip_while(|line| !line.starts_with("provider"))
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            (cols.len() >= 5).then(|| (format!("{}/{}", cols[0], cols[1]), cols[4] == "yes"))
        })
        .collect()
}

/// Both tiers must name a model: the caller is usually a model itself, and an
/// unset tier would quietly run everything on Pi's default with no escalation.
pub fn require_tiers(models: &Value) -> Res<()> {
    let missing: Vec<&str> = TIERS
        .iter()
        .copied()
        .filter(|tier| models[*tier].as_str().unwrap_or("").is_empty())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "model tiers not configured: {}\n{}",
        missing.join(", "),
        tiers_guide()
    ))
}

/// What a caller needs to configure the tiers itself in one step.
pub fn tiers_guide() -> String {
    let models = pi_models();
    let list = if models.is_empty() {
        "  (pi --list-models printed nothing; check that Pi is installed and has models)".into()
    } else {
        models
            .iter()
            .map(|(m, thinking)| format!("  {m}{}", if *thinking { "  [thinking]" } else { "" }))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "两档都要配置：cheap 给读材料、侦察、文案、看图（快、便宜），strong 给写代码与严密审查（最强）；\
         可选 default：不写 --tier 时用它，不配则只读走 cheap、写入走 strong。\n\
         用户指定过模型就照用户的；否则从下面 Pi 的可用模型中选，配置后在答复里告诉用户选了什么。\n\
         Pi models:\n{list}\n\
         配置 / configure: {} tiers --cheap <provider/model[:thinking]> --strong <provider/model[:thinking]> [--default <…>]\n\
         thinking: {}",
        script().display(),
        THINKING.join("|")
    )
}

/// Check a tier value against Pi's model list (skipped when the list is empty).
pub fn check_tier_model(pattern: &str, models: &[(String, bool)]) -> Res<()> {
    // Like Pi: only a last `:level` naming a thinking level is a suffix, so ids
    // such as `ollama/qwen2.5-coder:7b` stay whole.
    let model = pattern
        .rsplit_once(':')
        .filter(|(_, level)| THINKING.contains(level))
        .map_or(pattern, |(model, _)| model);
    if !model.contains('/') {
        return Err(format!(
            "{pattern}: write the model as provider/id[:thinking]"
        ));
    }
    if !models.is_empty() && !models.iter().any(|(m, _)| m == model) {
        return Err(format!(
            "{model} is not among Pi's models (pi --list-models)"
        ));
    }
    Ok(())
}

pub fn nesting_error() -> Option<String> {
    let caller = setting("AGENT", "");
    let caller = if caller.is_empty() {
        if env::var_os("PI_DELEGATE_ACTIVE").is_some() {
            AGENT
        } else {
            ""
        }
    } else {
        &caller
    };
    if !caller.is_empty() {
        Some(format!("refusing nested delegation: this is a delegated {caller} run; do the work yourself and report back to your caller (`lane` for heavy checks still works)"))
    } else {
        None
    }
}
pub fn agent_available(agent: &str) -> bool {
    agent_bin(agent).is_some()
}
/// The executable the colleague runs: `pi` on PATH. pi-kit's channel decides
/// whether that is Pi or rpi (Rust Pi, same arguments and JSON events);
/// `DELEGATE_PI_BIN` overrides it with another name or a path (absolute or `~/`).
pub fn pi_bin() -> String {
    setting("PI_BIN", AGENT)
}
fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
/// The colleague's executable name for `agent` (the configured Pi for `pi`).
fn bin_name(agent: &str) -> String {
    if agent == AGENT {
        pi_bin()
    } else {
        agent.to_string()
    }
}
pub fn agent_bin(agent: &str) -> Option<PathBuf> {
    agent_bins(agent).into_iter().next().map(|(_, real)| real)
}
/// Every distinct executable named `agent` on PATH, in PATH order, as (PATH entry, real
/// path); the first one runs. Entries are compared by real path, but reported as found,
/// since launchers such as snap resolve to one shared binary. A configured path is the
/// only candidate.
fn agent_bins(agent: &str) -> Vec<(PathBuf, PathBuf)> {
    let name = bin_name(agent);
    if name.contains('/') {
        let path = match name.strip_prefix("~/") {
            Some(rest) => home().join(rest),
            None => PathBuf::from(&name),
        };
        return path
            .canonicalize()
            .ok()
            .filter(|real| executable(real))
            .map(|real| vec![(path, real)])
            .unwrap_or_default();
    }
    let mut found: Vec<(PathBuf, PathBuf)> = vec![];
    let dirs = env::var_os("PATH")
        .map(|p| env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    for dir in dirs {
        let path = dir.join(&name);
        if executable(&path) {
            if let Ok(real) = path.canonicalize() {
                if !found.iter().any(|(_, seen)| *seen == real) {
                    found.push((path, real));
                }
            }
        }
    }
    found
}
/// The harness-facing capability line (`delegate protocol`, docs/delegate-protocol.md).
pub fn protocol() -> Value {
    let config = crate::config::load(git_top(&env::current_dir().unwrap_or_default()).as_deref())
        .map(|c| c.value)
        .unwrap_or_else(|_| json!({}));
    let bins = agent_bins(AGENT);
    let version = agent_identity(AGENT).ok().and_then(|(_, v)| v);
    let models: serde_json::Map<String, Value> = ALL_TIERS
        .iter()
        .map(|tier| (tier.to_string(), json!(tier_model(&config, tier))))
        .collect();
    let caller = caller_source().map(|(id, source)| json!({"id": id, "source": source}));
    json!({
        "protocol": PROTOCOL,
        "version": env!("CARGO_PKG_VERSION"),
        "caller": caller,
        "agents": [{
            "name": AGENT,
            "tiers": TIERS,
            "models": models,
            "available": !bins.is_empty(),
            "bin": bins.first().map(|(_, real)| real),
            "version": version,
            "shadowed": bins.iter().skip(1).map(|(path, _)| path).collect::<Vec<_>>(),
        }],
        "capacityDefaults": {"maxActive": 12, "maxHeavy": 2, "repoMaxActive": 8},
    })
}
pub fn agent_identity(agent: &str) -> Res<(PathBuf, Option<String>)> {
    let bin = agent_bin(agent).ok_or_else(|| format!("missing required tools: {agent}"))?;
    let mut command = std::process::Command::new(&bin);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    group(&mut command);
    let version = command.spawn().ok().and_then(|mut child| {
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if started.elapsed() < Duration::from_secs(2) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => {
                    end_group(child.id() as i32, 0.2);
                    let _ = child.wait();
                    return None;
                }
            }
        }
        child.wait_with_output().ok().and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .chain(String::from_utf8_lossy(&out.stderr).lines())
                .last()
                .map(str::to_string)
                .filter(|line| !line.is_empty())
        })
    });
    Ok((bin, version))
}
pub fn missing_tools(agent: &str) -> Res<()> {
    if agent_available(agent) {
        return Ok(());
    }
    if agent != AGENT {
        return Err(format!("missing required tools: {agent}"));
    }
    Err(format!(
        "missing required tools: {AGENT}\n  {AGENT}: {}",
        pi_install_hint()
    ))
}

fn pi_install_hint() -> String {
    let mut here = script();
    let mut kit = None;
    while let Some(parent) = here.parent() {
        let p = parent.join("third_party/pi-kit/install.sh");
        if p.is_file() || parent.join("crates/delegate/Cargo.toml").is_file() {
            kit = Some(p);
            break;
        }
        here = parent.to_path_buf();
    }
    let hint=kit.map(|p|format!("sh {} --additive",p.display())).unwrap_or_else(||"curl -fsSL https://git.aiatechco.com:31443/zji996/pi-kit/raw/branch/main/install.sh | sh -s -- --additive\n    (GitHub: https://raw.githubusercontent.com/zji996/pi-kit/main/install.sh)".into());
    hint
}
fn usage(v: &Value, k: &str) -> Value {
    v.get(k).cloned().unwrap_or(Value::Null)
}
fn filter_pi(v: &Value) -> Vec<Value> {
    let kind = s(v, "type");
    let tool = s(v, "toolName");
    let arg = &v["args"];
    match kind {
        "tool_execution_start" => {
            if tool == "bash" {
                let cmd = s(arg, "command");
                vec![
                    json!({"e":"bash","cmd":if cmd.trim_start().starts_with("node -e"){"node -e [inline script]".into()}else{clip(cmd,180)}}),
                ]
            } else if ["read", "edit", "write"].contains(&tool) {
                vec![json!({"e":tool,"path":s(arg,"path")})]
            } else {
                let text = [s(arg, "pattern"), s(arg, "path")]
                    .into_iter()
                    .filter(|x| !x.is_empty())
                    .collect::<Vec<_>>()
                    .join(" @ ");
                vec![json!({"e":"tool","tool":tool,"arg":clip(&text,160)})]
            }
        }
        "tool_execution_end" => {
            let mut out = vec![];
            if tool == "bash" {
                out.push(json!({"e":"bash_done","ok":!b(v,"isError")}));
            }
            if b(v, "isError") {
                let detail = v["result"]["content"]
                    .as_array()
                    .and_then(|a| a.first())
                    .map(|x| s(x, "text"))
                    .unwrap_or("")
                    .lines()
                    .rfind(|x| !x.trim().is_empty())
                    .unwrap_or("");
                out.push(json!({"e":"tool_error","tool":tool,"detail":clip(detail,240)}));
            }
            out
        }
        "message_end" if s(&v["message"], "role") == "assistant" => {
            let m = &v["message"];
            let text = m["content"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|x| s(x, "type") == "text")
                        .map(|x| s(x, "text"))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            let u = &m["usage"];
            let mut out = vec![
                json!({"e":"turn","provider":usage(m,"provider"),"model":usage(m,"model"),"stopReason":usage(m,"stopReason"),"usage":{"input":usage(u,"input"),"output":usage(u,"output"),"cacheRead":usage(u,"cacheRead")}}),
            ];
            if s(m, "stopReason") == "stop" && !text.trim().is_empty() {
                out.push(json!({"e":"result","text":text}));
            } else if !s(m, "errorMessage").is_empty() {
                out.push(json!({"e":"turn_error","detail":clip(s(m,"errorMessage"),300)}));
            }
            out
        }
        "auto_retry_start" => vec![
            json!({"e":"retry","attempt":usage(v,"attempt"),"maxAttempts":usage(v,"maxAttempts"),"errorMessage":clip(s(v,"errorMessage"),300)}),
        ],
        "compaction_start" | "compaction_end" => vec![json!({"e":kind,"reason":usage(v,"reason")})],
        // rpi's print mode ends with `result` instead of Pi's `agent_settled`.
        "agent_settled" => vec![json!({"e":"settled"})],
        "result" if s(v, "outcome") == "completed" => vec![json!({"e":"settled"})],
        _ => vec![],
    }
}
fn stall_seconds(meta: &Value, key: &str) -> Option<f64> {
    let env_name = format!("STALL_{}", key.to_uppercase());
    let default = if key == "idle" { "30m" } else { "90m" };
    let configured = meta["stall"]
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or(default);
    let value = setting(&env_name, configured);
    if value.trim() == "0" {
        return None;
    }
    seconds(&value).ok().filter(|n| *n > 0.0)
}

fn command_pi(meta: &Value, session: Option<&str>) -> Vec<String> {
    let fork = s(meta, "fork");
    let images = meta["images"].as_array().cloned().unwrap_or_default();
    let mut a = vec!["pi".into()];
    if !fork.is_empty() {
        a.extend(["--fork".into(), fork.into()]);
    } else {
        a.extend(["--session-id".into(), session.unwrap_or("").into()]);
    }
    a.extend([
        "--session-dir".into(),
        s(meta, "sessionDir").into(),
        "--mode".into(),
        "json".into(),
    ]);
    for key in ["provider", "model", "thinking"] {
        if !s(meta, key).is_empty() {
            a.extend([format!("--{key}"), s(meta, key).into()]);
        }
    }
    if s(meta, "mode") == "read-only" && !meta["worktree"].is_object() {
        a.extend(["--tools".into(), "read,grep,find,ls".into()]);
    }
    if !s(meta, "dir").is_empty() {
        let role = Path::new(s(meta, "dir")).join("colleague.md");
        a.extend([
            "--append-system-prompt".into(),
            role.to_string_lossy().into(),
        ]);
    }
    a.push("-p".into());
    for image in images {
        a.push(format!("@{}", image.as_str().unwrap_or("")));
    }
    a
}
/// Session files under a run's session directory: Pi writes them there, rpi in
/// one subdirectory per working directory.
pub fn session_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = vec![];
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(
                fs::read_dir(&path)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|x| x.path())
                    .filter(|x| x.extension().is_some_and(|e| e == "jsonl")),
            );
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            files.push(path);
        }
    }
    files.sort();
    files
}
pub fn session_file(dir: &str, id: &str) -> Option<PathBuf> {
    session_files(Path::new(dir)).into_iter().rfind(|x| {
        x.file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with(&format!("_{id}.jsonl")))
    })
}
pub fn leaked(answer: &str) -> bool {
    let tail = answer
        .trim()
        .chars()
        .rev()
        .take(600)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    if !tail.ends_with('}') {
        return false;
    }
    tail.match_indices("call:").any(|(pos, _)| {
        if tail[..pos]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            return false;
        }
        let mut chars = tail[pos + 5..].chars().peekable();
        let mut first = 0;
        while chars
            .peek()
            .is_some_and(|c| c.is_alphanumeric() || "_.-".contains(*c))
        {
            chars.next();
            first += 1;
        }
        if first == 0 {
            return false;
        }
        if chars.peek() == Some(&':') {
            chars.next();
            let mut second = 0;
            while chars
                .peek()
                .is_some_and(|c| c.is_alphanumeric() || "_-".contains(*c))
            {
                chars.next();
                second += 1;
            }
            if second == 0 {
                return false;
            }
        }
        chars.peek() == Some(&'{')
    })
}
pub fn run_agent(
    meta: &Value,
    run: &Path,
    attempt: i64,
    holder: Arc<AtomicI32>,
    grace_out: Arc<std::sync::Mutex<Option<f64>>>,
) -> Res<(String, String)> {
    if s(meta, "agent") != AGENT {
        return Err(format!(
            "{} runs are no longer supported; delegate runs Pi only",
            s(meta, "agent")
        ));
    }
    let uuid = format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        now_ns() as u32,
        std::process::id() as u16,
        0u16,
        0u16,
        (now_ns() >> 8) as u64 & 0xffffffffffff
    );
    let session = if !s(meta, "fork").is_empty() {
        None
    } else {
        Some(uuid)
    };
    let session_dir = Path::new(s(meta, "sessionDir"));
    let known = session_files(session_dir);
    write(Path::new(s(meta, "dir")).join("colleague.md"), COLLEAGUE)?;
    let mut args = command_pi(meta, session.as_deref());
    if !s(meta, "agentBin").is_empty() {
        args[0] = s(meta, "agentBin").to_string();
    }
    let prompt = OpenOptions::new()
        .read(true)
        .open(
            meta["agentPromptFile"]
                .as_str()
                .map(PathBuf::from)
                .unwrap_or_else(|| run.join("prompt.md")),
        )
        .map_err(|e| e.to_string())?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(run.join("stderr.log"))
        .map_err(|e| e.to_string())?;
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(run.join("events.jsonl"))
        .map_err(|e| e.to_string())?;
    if let Some(id) = &session {
        writeln!(
            log,
            "{}",
            json!({"e":"session","id":id,"attempt":attempt,"at":iso()})
        )
        .map_err(|e| e.to_string())?;
    }
    let mut c =
        crate::cleanup::scoped_command(run, &format!("agent-{attempt}"), &args[0], &args[1..]);
    c.current_dir(s(meta, "workdir"))
        .stdin(Stdio::from(prompt))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr));
    clean_env(&mut c, &meta["env"]);
    crate::deny::inject(&mut c, meta, run)?;
    c.env("DELEGATE_AGENT", s(meta, "agent"))
        .env("DELEGATE_RUN_DIR", s(meta, "dir"));
    c.env("PI_DELEGATE_ACTIVE", "1");
    group(&mut c);
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let pid = child.id() as i32;
    holder.store(pid, Ordering::SeqCst);
    write(run.join("agent.pid"), pid.to_string())?;
    let timed = Arc::new(AtomicBool::new(false));
    let watch = Arc::new((
        Mutex::new(WatchState {
            finished: false,
            last: Instant::now(),
            commands: 0,
            command_started: None,
            command_queued: 0.0,
            command: String::new(),
        }),
        Condvar::new(),
    ));
    let start = Instant::now();
    let soft = meta["timeoutSeconds"].as_f64().filter(|x| *x > 0.0);
    let g = setting("TIMEOUT_GRACE", "10")
        .parse::<f64>()
        .unwrap_or(10.0);
    let hard = soft.map(|x| x * (1.0 + g / 100.0));
    let stall_idle = stall_seconds(meta, "idle");
    let stall_command = stall_seconds(meta, "command");
    let stalled = Arc::new(Mutex::new(None::<Value>));
    let (watcher_state, t, go, stalled_out) = (
        watch.clone(),
        timed.clone(),
        grace_out.clone(),
        stalled.clone(),
    );
    let runpath = run.to_path_buf();
    let watcher = std::thread::spawn(move || {
        let (mutex, changed) = &*watcher_state;
        let mut state = mutex.lock().unwrap();
        loop {
            if state.finished {
                break;
            }
            let queued = lane::queued_seconds(&runpath);
            let spent = start.elapsed().as_secs_f64() - queued;
            let idle = state.last.elapsed().as_secs_f64();
            let command_idle = idle - (queued - state.command_queued).max(0.0);
            let command_age = state
                .command_started
                .map(|at| at.elapsed().as_secs_f64() - (queued - state.command_queued).max(0.0));
            if stall_idle.is_some_and(|limit| state.commands == 0 && idle >= limit) {
                let elapsed = idle.round() as u64;
                let reason = json!({"kind":"idle","seconds":elapsed,"limitSeconds":stall_idle.unwrap_or(0.0)});
                if let Ok(mut out) = stalled_out.lock() {
                    *out = Some(reason.clone());
                }
                if let Ok(mut log) = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(runpath.join("events.jsonl"))
                {
                    let _ = writeln!(
                        log,
                        "{}",
                        json!({"e":"stalled","kind":"idle","seconds":elapsed,"at":iso()})
                    );
                }
                t.store(false, Ordering::SeqCst);
                crate::cleanup::record(&runpath, pid);
                kill_group(pid, 5.0);
                break;
            }
            if stall_command
                .is_some_and(|limit| state.command_started.is_some() && command_idle >= limit)
            {
                let elapsed = command_age.unwrap_or(0.0).round() as u64;
                let reason = json!({"kind":"command","cmd":state.command,"seconds":elapsed,"limitSeconds":stall_command.unwrap_or(0.0)});
                if let Ok(mut out) = stalled_out.lock() {
                    *out = Some(reason.clone());
                }
                if let Ok(mut log) = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(runpath.join("events.jsonl"))
                {
                    let _ = writeln!(
                        log,
                        "{}",
                        json!({"e":"stalled","kind":"command","cmd":state.command,"seconds":elapsed,"at":iso()})
                    );
                }
                t.store(false, Ordering::SeqCst);
                crate::cleanup::record(&runpath, pid);
                kill_group(pid, 5.0);
                break;
            }
            if let Some(soft) = soft {
                if spent >= soft {
                    if spent < hard.unwrap_or(soft) && (state.commands > 0 || idle < 120.0) {
                        if let Ok(mut x) = go.lock() {
                            *x = Some(((spent - soft) * 10.0).round() / 10.0);
                        }
                    } else {
                        t.store(true, Ordering::SeqCst);
                        crate::cleanup::record(&runpath, pid);
                        kill_group(pid, 5.0);
                        break;
                    }
                }
            }
            let mut delay: f64 = 60.0;
            if let Some(soft) = soft {
                // Wake at the soft limit to record grace, then at the hard limit.
                let next = if spent < soft {
                    soft
                } else {
                    hard.unwrap_or(soft)
                };
                delay = delay.min((next - spent).max(0.01));
            }
            if let Some(limit) = stall_idle {
                if state.commands == 0 {
                    delay = delay.min((limit - idle).max(0.01));
                }
            }
            if let Some(limit) = stall_command {
                if state.command_started.is_some() {
                    delay = delay.min((limit - command_idle).max(0.01));
                }
            }
            state = changed
                .wait_timeout(state, Duration::from_secs_f64(delay.max(0.001)))
                .unwrap()
                .0;
        }
    });
    let mut turn = Value::Null;
    let mut answer = String::new();
    let mut settled = false;
    if let Some(stdout) = child.stdout.take() {
        let mut reader = BufReader::new(stdout);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
            let Ok(event) = serde_json::from_slice::<Value>(&raw) else {
                continue;
            };
            for mut item in filter_pi(&event) {
                let kind = s(&item, "e").to_string();
                item["attempt"] = json!(attempt);
                item["at"] = json!(iso());
                {
                    let (mutex, changed) = &*watch;
                    let mut state = mutex.lock().unwrap();
                    state.last = Instant::now();
                    if kind == "bash" {
                        state.commands += 1;
                        state.command_started = Some(Instant::now());
                        state.command_queued = lane::queued_seconds(run);
                        state.command = s(&item, "cmd").to_string();
                    } else if kind == "bash_done" {
                        state.commands = (state.commands - 1).max(0);
                        if state.commands == 0 {
                            state.command_started = None;
                        }
                        if !state.command.is_empty() {
                            item["cmd"] = json!(state.command.clone());
                        }
                    }
                    changed.notify_one();
                }
                let _ = writeln!(log, "{item}");
                let _ = log.flush();
                match kind.as_str() {
                    "turn" => {
                        turn = item;
                        answer.clear();
                    }
                    "result" | "message" => answer = s(&item, "text").into(),
                    "settled" => settled = true,
                    "turn_error" => turn = json!({"stopReason":"error"}),
                    _ => {}
                }
            }
        }
    }
    loop {
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            break;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            break;
        }
    }
    {
        let (mutex, changed) = &*watch;
        let mut state = mutex.lock().unwrap();
        state.finished = true;
        changed.notify_one();
    }
    let _ = watcher.join();
    holder.store(0, Ordering::SeqCst);
    let status = child.wait().map_err(|e| e.to_string())?;
    {
        let created = session_files(session_dir)
            .into_iter()
            .filter(|x| !known.contains(x))
            .collect::<Vec<_>>();
        if let Some(p) = created.last() {
            let stem = p.file_stem().unwrap_or_default().to_string_lossy();
            let id = stem.split_once('_').map(|(_, x)| x).unwrap_or(&stem);
            let _ = writeln!(
                log,
                "{}",
                json!({"e":"session","id":id,"attempt":attempt,"at":iso()})
            );
        }
    }
    let stalled_reason = stalled.lock().ok().and_then(|x| x.clone());
    if let Some(reason) = &stalled_reason {
        let _ = write_json(run.join("stall.json"), reason);
    }
    let verdict = if lane::stopped() {
        "stopped"
    } else if stalled_reason.is_some() {
        "stalled"
    } else if timed.load(Ordering::SeqCst) {
        "timeout"
    } else if !status.success() {
        use std::os::unix::process::ExitStatusExt;
        if status.code() == Some(137) || status.signal() == Some(9) {
            "killed"
        } else {
            "failed"
        }
    } else if s(&turn, "stopReason") == "stop" && settled {
        if answer.trim().is_empty() || leaked(&answer) {
            "malformed"
        } else {
            "ok"
        }
    } else {
        "failed"
    };
    Ok((verdict.into(), answer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(_agent: &str, meta: &Value, session: Option<&str>) -> Vec<String> {
        command_pi(meta, session)
    }

    #[test]
    fn tier_rules_pick_the_first_window_that_holds_the_minute() {
        let at = |weekday, minute| Clock { weekday, minute };
        let rules = json!([{"model": "ds", "hours": "00:30-08:30"}, {"model": "night", "hours": "22:00-00:30"}, "gpt"]);
        assert_eq!(resolve_tier(&rules, at(1, 30), &|_| 0), "ds");
        assert_eq!(resolve_tier(&rules, at(1, 8 * 60 + 29), &|_| 0), "ds");
        assert_eq!(resolve_tier(&rules, at(1, 8 * 60 + 30), &|_| 0), "gpt");
        assert_eq!(resolve_tier(&rules, at(1, 23 * 60), &|_| 0), "night");
        assert_eq!(resolve_tier(&rules, at(1, 0), &|_| 0), "night");
        assert_eq!(resolve_tier(&json!("plain"), at(1, 0), &|_| 0), "plain");
        assert_eq!(parse_hours("8:30-09:00"), None);
        assert_eq!(parse_hours("09:00-09:00"), None);
        assert_eq!(
            describe_tier(&rules).unwrap(),
            "00:30-08:30 ds, 22:00-00:30 night, else gpt"
        );
    }

    #[test]
    fn tier_rules_read_weekdays_and_several_windows() {
        let at = |weekday, minute| Clock { weekday, minute };
        // DeepSeek's peak: weekdays 09:00-12:00 and 14:00-18:00 Beijing time.
        let rules =
            json!([{"model": "gpt", "days": "mon-fri", "hours": "09:00-12:00,14:00-18:00"}, "ds"]);
        assert_eq!(resolve_tier(&rules, at(1, 9 * 60), &|_| 0), "gpt");
        assert_eq!(resolve_tier(&rules, at(5, 17 * 60 + 59), &|_| 0), "gpt");
        assert_eq!(resolve_tier(&rules, at(3, 12 * 60 + 30), &|_| 0), "ds");
        assert_eq!(resolve_tier(&rules, at(5, 18 * 60), &|_| 0), "ds");
        assert_eq!(resolve_tier(&rules, at(6, 10 * 60), &|_| 0), "ds");
        assert_eq!(resolve_tier(&rules, at(7, 15 * 60), &|_| 0), "ds");
        assert_eq!(
            resolve_tier(
                &json!([{"model": "weekend", "days": "sat,sun"}, "x"]),
                at(7, 0),
                &|_| 0
            ),
            "weekend"
        );
        assert_eq!(parse_days("fri-mon"), None);
        assert_eq!(parse_days("mon,fri-sun"), Some(vec![1, 5, 6, 7]));
        assert_eq!(parse_windows("09:00-12:00,9-10"), None);
        assert_eq!(
            describe_tier(&rules).unwrap(),
            "mon-fri 09:00-12:00,14:00-18:00 gpt, else ds"
        );
    }

    #[test]
    fn tier_rules_skip_a_model_at_its_max_active() {
        let at = |weekday, minute| Clock { weekday, minute };
        let rules = json!([
            {"model": "spot", "maxActive": 5},
            {"model": "gpt", "days": "mon-fri", "hours": "09:00-12:00,14:00-18:00"},
            "ds"
        ]);
        let running = |busy: usize| move |model: &str| if model == "spot" { busy } else { 9 };
        assert_eq!(resolve_tier(&rules, at(1, 10 * 60), &running(4)), "spot");
        assert_eq!(resolve_tier(&rules, at(1, 10 * 60), &running(5)), "gpt");
        assert_eq!(resolve_tier(&rules, at(6, 10 * 60), &running(5)), "ds");
        // A capped rule may also carry a window; both must hold.
        let night = json!([{"model": "spot", "hours": "20:00-08:00", "maxActive": 2}, "ds"]);
        assert_eq!(resolve_tier(&night, at(1, 21 * 60), &running(1)), "spot");
        assert_eq!(resolve_tier(&night, at(1, 12 * 60), &running(0)), "ds");
        assert_eq!(
            describe_tier(&rules).unwrap(),
            "<5 running spot, mon-fri 09:00-12:00,14:00-18:00 gpt, else ds"
        );
    }

    #[test]
    fn tier_models_keep_ids_with_colons_and_strip_only_thinking() {
        let models = vec![
            ("ollama/qwen2.5-coder:7b".to_string(), false),
            ("p/m".to_string(), true),
        ];
        assert!(check_tier_model("ollama/qwen2.5-coder:7b", &models).is_ok());
        assert!(check_tier_model("p/m:high", &models).is_ok());
        assert!(check_tier_model("p/m:huge", &models).is_err());
        assert!(check_tier_model("m", &[]).is_err());
    }

    #[test]
    fn stall_settings_have_defaults_and_zero_disables() {
        std::env::remove_var("DELEGATE_STALL_IDLE");
        std::env::remove_var("PI_DELEGATE_STALL_IDLE");
        assert_eq!(stall_seconds(&json!({}), "idle"), Some(1800.0));
        assert_eq!(stall_seconds(&json!({"stall":{"idle":"0"}}), "idle"), None);
        assert_eq!(
            stall_seconds(&json!({"stall":{"command":"2m"}}), "command"),
            Some(120.0)
        );
    }

    #[test]
    fn pi_command_preserves_argument_order() {
        let meta = json!({"workdir":"/repo","sessionDir":"/runs/session","mode":"read-only","images":["/a.png","/b.png"],"provider":"p","model":"m","thinking":"high"});
        assert_eq!(
            args("pi", &meta, Some("session-1")),
            [
                "pi",
                "--session-id",
                "session-1",
                "--session-dir",
                "/runs/session",
                "--mode",
                "json",
                "--provider",
                "p",
                "--model",
                "m",
                "--thinking",
                "high",
                "--tools",
                "read,grep,find,ls",
                "-p",
                "@/a.png",
                "@/b.png"
            ]
        );
        let fork = json!({"fork":"/runs/parent.jsonl","sessionDir":"/runs/session","mode":"read-only","worktree":{},"images":["/a.png"]});
        assert_eq!(
            args("pi", &fork, None),
            [
                "pi",
                "--fork",
                "/runs/parent.jsonl",
                "--session-dir",
                "/runs/session",
                "--mode",
                "json",
                "-p",
                "@/a.png"
            ]
        );
    }
}
