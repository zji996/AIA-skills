use crate::common::*;
use crate::lane;
use serde_json::{json, Value};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

pub fn nesting_error(agent: &str) -> Option<String> {
    let caller = setting("AGENT", "");
    let caller = if caller.is_empty() && env::var_os("PI_DELEGATE_ACTIVE").is_some() {
        "pi"
    } else {
        &caller
    };
    if caller == "pi" {
        Some("refusing nested delegation: a delegated Pi run cannot delegate further".into())
    } else if caller == "codex" && agent == "codex" {
        Some("refusing nested delegation: a delegated Codex run may delegate to Pi (--agent pi) but not to Codex".into())
    } else {
        None
    }
}
pub fn missing_tools(agent: &str) -> Res<()> {
    let found = env::var_os("PATH").and_then(|p| {
        env::split_paths(&p).find(|x| {
            fs::metadata(x.join(agent))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    });
    if found.is_some() {
        return Ok(());
    }
    if agent == "codex" {
        return Err("missing required tools: codex\n  codex: see https://github.com/openai/codex (npm i -g @openai/codex), then log in".into());
    }
    let mut here = script();
    let mut kit = None;
    while let Some(parent) = here.parent() {
        let p = parent.join("third_party/pi-kit/install.sh");
        if p.is_file() {
            kit = Some(p);
            break;
        }
        here = parent.to_path_buf();
    }
    let hint=kit.map(|p|format!("sh {} --additive",p.display())).unwrap_or_else(||"curl -fsSL https://git.aiatechco.com:31443/zji996/pi-kit/raw/branch/main/install.sh | sh -s -- --additive\n    (GitHub: https://raw.githubusercontent.com/zji996/pi-kit/main/install.sh)".into());
    Err(format!("missing required tools: pi\n  pi: {hint}"))
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
fn command(meta: &Value, session: Option<&str>) -> Vec<String> {
    let fork = s(meta, "fork");
    let images = meta["images"].as_array().cloned().unwrap_or_default();
    if s(meta, "agent") == "codex" {
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
    } else {
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
        if s(meta, "mode") == "read-only" {
            a.extend(["--tools".into(), "read,grep,find,ls".into()]);
        }
        a.push("-p".into());
        for image in images {
            a.push(format!("@{}", image.as_str().unwrap_or("")));
        }
        a
    }
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
    let codex = s(meta, "agent") == "codex";
    let uuid = format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        now_ns() as u32,
        std::process::id() as u16,
        0u16,
        0u16,
        (now_ns() >> 8) as u64 & 0xffffffffffff
    );
    let session = if codex || !s(meta, "fork").is_empty() {
        None
    } else {
        Some(uuid)
    };
    let session_dir = Path::new(s(meta, "sessionDir"));
    let known = fs::read_dir(session_dir)
        .ok()
        .map(|e| e.flatten().map(|x| x.path()).collect::<Vec<_>>())
        .unwrap_or_default();
    let args = command(meta, session.as_deref());
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
    let mut c = Command::new(&args[0]);
    c.args(&args[1..])
        .current_dir(s(meta, "workdir"))
        .stdin(Stdio::from(prompt))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr));
    clean_env(&mut c, &meta["env"]);
    c.env("DELEGATE_AGENT", s(meta, "agent"))
        .env("DELEGATE_PARENT_RUN", s(meta, "run"))
        .env("DELEGATE_RUN_DIR", s(meta, "dir"));
    if !codex {
        c.env("PI_DELEGATE_ACTIVE", "1");
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
            for mut item in if codex {
                filter_codex(&event)
            } else {
                filter_pi(&event)
            } {
                let kind = s(&item, "e").to_string();
                if codex && kind == "turn" {
                    item["model"] = if !s(meta, "model").is_empty() {
                        json!(s(meta, "model"))
                    } else {
                        json!(codex_model())
                    };
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
                        settled |= codex;
                        if !codex {
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
    if !codex {
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
