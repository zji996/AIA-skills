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
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SessionSource {
    DelegateFile,
    EventStream,
}

/// Harness-facing protocol version; bump when docs/delegate-protocol.md §2 breaks.
pub const PROTOCOL: u32 = 1;

pub struct AgentSpec {
    pub name: &'static str,
    pub default_tier: Option<&'static str>,
    pub default_timeout: &'static str,
    pub heavy: bool,
    pub session: SessionSource,
    pub read_only_contract: bool,
    active_env: Option<&'static str>,
    install_hint: fn() -> String,
    build_command: fn(&Value, Option<&str>) -> Vec<String>,
    filter_events: fn(&Value) -> Vec<Value>,
    turn_model: Option<fn(&Value) -> Value>,
    settles_on_turn: bool,
    clears_answer_on_turn: bool,
}

pub static AGENTS: &[AgentSpec] = &[
    AgentSpec {
        name: "pi",
        default_tier: Some("cheap"),
        default_timeout: "15m",
        heavy: false,
        session: SessionSource::DelegateFile,
        read_only_contract: false,
        active_env: Some("PI_DELEGATE_ACTIVE"),
        install_hint: pi_install_hint,
        build_command: command_pi,
        filter_events: filter_pi,
        turn_model: None,
        settles_on_turn: false,
        clears_answer_on_turn: true,
    },
    AgentSpec {
        name: "codex",
        default_tier: Some("strong"),
        default_timeout: "30m",
        heavy: true,
        session: SessionSource::EventStream,
        read_only_contract: true,
        active_env: None,
        install_hint: codex_install_hint,
        build_command: command_codex,
        filter_events: filter_codex,
        turn_model: Some(codex_turn_model),
        settles_on_turn: true,
        clears_answer_on_turn: false,
    },
];

pub fn spec(name: &str) -> Option<&'static AgentSpec> {
    AGENTS.iter().find(|agent| agent.name == name)
}

pub fn default_for_tier(tier: &str) -> &'static AgentSpec {
    AGENTS
        .iter()
        .find(|agent| agent.default_tier == Some(tier))
        .expect("configured tier")
}

pub fn choices(separator: &str) -> String {
    AGENTS
        .iter()
        .map(|agent| agent.name)
        .collect::<Vec<_>>()
        .join(separator)
}

pub fn nesting_error() -> Option<String> {
    let caller = setting("AGENT", "");
    let caller = if caller.is_empty() {
        AGENTS
            .iter()
            .find(|agent| {
                agent
                    .active_env
                    .is_some_and(|key| env::var_os(key).is_some())
            })
            .map_or("", |agent| agent.name)
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
pub fn agent_bin(agent: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?).find_map(|dir| {
        let path = dir.join(agent);
        if fs::metadata(&path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) {
            path.canonicalize().ok()
        } else {
            None
        }
    })
}
/// Every executable named `agent` on PATH, resolved and in PATH order; the first one runs.
fn agent_bins(agent: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = vec![];
    for dir in env::var_os("PATH")
        .map(|p| env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
    {
        let path = dir.join(agent);
        if fs::metadata(&path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) {
            if let Ok(real) = path.canonicalize() {
                if !found.contains(&real) {
                    found.push(real);
                }
            }
        }
    }
    found
}
/// The harness-facing capability line (`delegate protocol`, docs/delegate-protocol.md).
pub fn protocol() -> Value {
    let tiers_of = |name: &str| {
        ["cheap", "strong"]
            .into_iter()
            .filter(|tier| {
                setting(
                    &format!("{}_AGENT", tier.to_uppercase()),
                    default_for_tier(tier).name,
                ) == name
            })
            .collect::<Vec<_>>()
    };
    let agents = AGENTS
        .iter()
        .map(|agent| {
            let bins = agent_bins(agent.name);
            let version = agent_identity(agent.name).ok().and_then(|(_, v)| v);
            json!({
                "name": agent.name,
                "tiers": tiers_of(agent.name),
                "available": !bins.is_empty(),
                "bin": bins.first(),
                "version": version,
                "shadowed": bins.iter().skip(1).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    let caller = caller_source().map(|(id, source)| json!({"id": id, "source": source}));
    json!({
        "protocol": PROTOCOL,
        "version": env!("CARGO_PKG_VERSION"),
        "caller": caller,
        "agents": agents,
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
    let agent = spec(agent).ok_or_else(|| format!("missing required tools: {agent}"))?;
    Err(format!(
        "missing required tools: {}\n  {}: {}",
        agent.name,
        agent.name,
        (agent.install_hint)()
    ))
}

fn codex_install_hint() -> String {
    "see https://github.com/openai/codex (npm i -g @openai/codex), then log in".into()
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
        "agent_settled" => vec![json!({"e":"settled"})],
        _ => vec![],
    }
}
fn filter_codex(v: &Value) -> Vec<Value> {
    let kind = s(v, "type");
    let item = &v["item"];
    let ty = s(item, "type");
    match kind {
        "thread.started" if !s(v, "thread_id").is_empty() => {
            vec![json!({"e":"session","id":s(v,"thread_id")})]
        }
        "item.started" if ty == "command_execution" => {
            vec![json!({"e":"bash","cmd":clip(s(item,"command"),180)})]
        }
        "item.completed" => match ty {
            "command_execution" => vec![json!({"e":"bash_done","ok":n(item,"exit_code")==0})],
            "file_change" => item["changes"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|x| json!({"e":"edit","path":s(x,"path")}))
                        .collect()
                })
                .unwrap_or_default(),
            "agent_message" => vec![json!({"e":"message","text":s(item,"text")})],
            "error" => vec![json!({"e":"warning","detail":clip(s(item,"message"),240)})],
            "mcp_tool_call" | "web_search" | "todo_list" => vec![
                json!({"e":"tool","tool":ty,"arg":clip(if !s(item,"query").is_empty(){s(item,"query")}else{s(item,"tool")},160)}),
            ],
            _ => vec![],
        },
        "turn.completed" => {
            let u = &v["usage"];
            vec![
                json!({"e":"turn","stopReason":"stop","usage":{"input":usage(u,"input_tokens"),"output":usage(u,"output_tokens"),"cacheRead":usage(u,"cached_input_tokens")}}),
            ]
        }
        "turn.failed" | "error" => vec![
            json!({"e":"turn_error","detail":clip(if !s(&v["error"],"message").is_empty(){s(&v["error"],"message")}else if !s(v,"message").is_empty(){s(v,"message")}else{kind},300)}),
        ],
        _ => vec![],
    }
}
fn codex_model() -> Option<String> {
    let path = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".codex"))
        .join("config.toml");
    for line in read(path).lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("model") {
            let v = v.trim_start();
            if let Some(v) = v.strip_prefix('=') {
                let v = v.trim();
                if v.starts_with('"') && v.ends_with('"') {
                    return Some(v.trim_matches('"').into());
                }
            }
        }
    }
    None
}
fn codex_turn_model(meta: &Value) -> Value {
    if !s(meta, "model").is_empty() {
        json!(s(meta, "model"))
    } else {
        json!(codex_model())
    }
}

fn command_codex(meta: &Value, _session: Option<&str>) -> Vec<String> {
    let fork = s(meta, "fork");
    let images = meta["images"].as_array().cloned().unwrap_or_default();
    let mut a = vec!["codex".into(), "exec".into()];
    if !fork.is_empty() {
        a.extend(["fork".into(), fork.into()]);
    }
    a.extend(["--json".into(), "--skip-git-repo-check".into()]);
    if fork.is_empty() {
        a.extend(["-C".into(), s(meta, "workdir").into()]);
    }
    a.push("--dangerously-bypass-approvals-and-sandbox".into());
    if !s(meta, "model").is_empty() {
        a.extend(["-m".into(), s(meta, "model").into()]);
    }
    for (key, label) in [
        ("thinking", "model_reasoning_effort"),
        ("provider", "model_provider"),
    ] {
        if !s(meta, key).is_empty() {
            a.extend(["-c".into(), format!("{label}=\"{}\"", s(meta, key))]);
        }
    }
    for image in images {
        a.push(format!("--image={}", image.as_str().unwrap_or("")));
    }
    a.push("-".into());
    a
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
    a.push("-p".into());
    for image in images {
        a.push(format!("@{}", image.as_str().unwrap_or("")));
    }
    a
}
pub fn session_file(dir: &str, id: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    let mut files = entries
        .flatten()
        .map(|x| x.path())
        .filter(|x| {
            x.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(&format!("_{id}.jsonl")))
        })
        .collect::<Vec<_>>();
    files.sort();
    files.pop()
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
    let agent = spec(s(meta, "agent")).ok_or_else(|| "unknown agent".to_string())?;
    let uuid = format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        now_ns() as u32,
        std::process::id() as u16,
        0u16,
        0u16,
        (now_ns() >> 8) as u64 & 0xffffffffffff
    );
    let session = if agent.session == SessionSource::EventStream || !s(meta, "fork").is_empty() {
        None
    } else {
        Some(uuid)
    };
    let session_dir = Path::new(s(meta, "sessionDir"));
    let known = fs::read_dir(session_dir)
        .ok()
        .map(|e| e.flatten().map(|x| x.path()).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut args = (agent.build_command)(meta, session.as_deref());
    if !s(meta, "agentBin").is_empty() {
        args[0] = s(meta, "agentBin").to_string();
    }
    let prompt = OpenOptions::new()
        .read(true)
        .open(run.join("prompt.md"))
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
    c.env("DELEGATE_AGENT", s(meta, "agent"))
        .env("DELEGATE_RUN_DIR", s(meta, "dir"));
    if let Some(env_name) = agent.active_env {
        c.env(env_name, "1");
    }
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
        }),
        Condvar::new(),
    ));
    let start = Instant::now();
    let soft = meta["timeoutSeconds"].as_f64().unwrap_or(900.0);
    let g = setting("TIMEOUT_GRACE", "50")
        .parse::<f64>()
        .unwrap_or(50.0);
    let hard = soft * (1.0 + g / 100.0);
    let (watcher_state, t, go) = (watch.clone(), timed.clone(), grace_out.clone());
    let runpath = run.to_path_buf();
    let watcher = std::thread::spawn(move || {
        let (mutex, changed) = &*watcher_state;
        let mut state = mutex.lock().unwrap();
        loop {
            if state.finished {
                break;
            }
            let spent = start.elapsed().as_secs_f64() - lane::queued_seconds(&runpath);
            let idle = state.last.elapsed().as_secs_f64();
            let delay = if spent < soft {
                soft - spent
            } else if spent < hard && (state.commands > 0 || idle < 120.0) {
                if let Ok(mut x) = go.lock() {
                    *x = Some(((spent - soft) * 10.0).round() / 10.0);
                }
                if state.commands > 0 {
                    hard - spent
                } else {
                    (hard - spent).min(120.0 - idle)
                }
            } else {
                t.store(true, Ordering::SeqCst);
                crate::cleanup::record(&runpath, pid);
                kill_group(pid, 5.0);
                break;
            };
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
            for mut item in (agent.filter_events)(&event) {
                let kind = s(&item, "e").to_string();
                if kind == "turn" {
                    if let Some(turn_model) = agent.turn_model {
                        item["model"] = turn_model(meta);
                    }
                }
                item["attempt"] = json!(attempt);
                item["at"] = json!(iso());
                {
                    let (mutex, changed) = &*watch;
                    let mut state = mutex.lock().unwrap();
                    state.last = Instant::now();
                    if kind == "bash" {
                        state.commands += 1;
                    } else if kind == "bash_done" {
                        state.commands = (state.commands - 1).max(0);
                    }
                    changed.notify_one();
                }
                let _ = writeln!(log, "{item}");
                let _ = log.flush();
                match kind.as_str() {
                    "turn" => {
                        turn = item;
                        settled |= agent.settles_on_turn;
                        if agent.clears_answer_on_turn {
                            answer.clear();
                        }
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
    if agent.session == SessionSource::DelegateFile {
        let mut created = fs::read_dir(session_dir)
            .ok()
            .map(|e| {
                e.flatten()
                    .map(|x| x.path())
                    .filter(|x| !known.contains(x))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        created.sort();
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
    let verdict = if lane::stopped() {
        "stopped"
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
    use std::collections::HashSet;

    fn args(agent: &str, meta: &Value, session: Option<&str>) -> Vec<String> {
        (spec(agent).unwrap().build_command)(meta, session)
    }

    #[test]
    fn agent_names_and_tier_defaults_are_unique() {
        let mut names = HashSet::new();
        for agent in AGENTS {
            assert!(names.insert(agent.name), "duplicate agent: {}", agent.name);
        }
        for tier in ["cheap", "strong"] {
            assert_eq!(
                AGENTS
                    .iter()
                    .filter(|agent| agent.default_tier == Some(tier))
                    .count(),
                1
            );
        }
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

    #[test]
    fn codex_command_preserves_argument_order() {
        let meta = json!({"workdir":"/repo","images":["/a.png","/b.png"],"provider":"p","model":"m","thinking":"high"});
        assert_eq!(
            args("codex", &meta, None),
            [
                "codex",
                "exec",
                "--json",
                "--skip-git-repo-check",
                "-C",
                "/repo",
                "--dangerously-bypass-approvals-and-sandbox",
                "-m",
                "m",
                "-c",
                "model_reasoning_effort=\"high\"",
                "-c",
                "model_provider=\"p\"",
                "--image=/a.png",
                "--image=/b.png",
                "-"
            ]
        );
        let fork = json!({"fork":"thread-1","workdir":"/repo"});
        assert_eq!(
            args("codex", &fork, None),
            [
                "codex",
                "exec",
                "fork",
                "thread-1",
                "--json",
                "--skip-git-repo-check",
                "--dangerously-bypass-approvals-and-sandbox",
                "-"
            ]
        );
    }
}
