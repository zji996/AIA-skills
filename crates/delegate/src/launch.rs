use crate::agents::{
    self, agent_available, agent_identity, missing_tools, nesting_error, session_file,
    SessionSource,
};
use crate::changes::snapshot;
use crate::common::*;
use crate::runs::{self, active, agent_alive, all_runs, state};
use crate::worktree;
use serde_json::{json, Value};
use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Default, Clone)]
pub struct Options {
    pub words: Vec<String>,
    pub prompt: Option<String>,
    pub prompt_file: Option<String>,
    pub agent: String,
    pub tier: Option<String>,
    pub name: Option<String>,
    pub workdir: Option<String>,
    pub images: Vec<String>,
    pub protect: Vec<String>,
    pub read_only: bool,
    pub in_place: bool,
    pub worktree: bool,
    pub accept: Option<String>,
    pub accept_set: bool,
    pub hide_accept: bool,
    pub accept_timeout: String,
    pub timeout: Option<String>,
    pub retries: i64,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub parallel: bool,
    pub fresh: bool,
    pub sync: bool,
    pub wait: bool,
    pub max: Option<f64>,
    pub progress: bool,
    pub full: bool,
    pub run: Option<String>,
    pub after: Option<String>,
    pub in_run: Option<String>,
}
impl Options {
    pub fn new() -> Self {
        Self {
            accept_timeout: "10m".into(),
            retries: 1,
            ..Default::default()
        }
    }
}
pub fn parse_launch(args: &[String], reply: bool, collect: bool) -> Res<Options> {
    let mut o = Options::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (key, inline) = if a.starts_with("--") {
            a.split_once('=')
                .map_or((a.as_str(), None), |(k, v)| (k, Some(v)))
        } else {
            (a.as_str(), None)
        };
        if reply
            && [
                "--workdir",
                "--read-only",
                "--in-place",
                "--worktree",
                "--protect",
                "--retries",
                "--provider",
                "--model",
                "--thinking",
                "--allow-parallel-writes",
            ]
            .contains(&key)
        {
            return Err(format!("unrecognized arguments: {a}"));
        }
        let takes = [
            "--prompt",
            "--prompt-file",
            "--agent",
            "--tier",
            "--name",
            "--workdir",
            "--image",
            "--protect",
            "--accept",
            "--accept-timeout",
            "--timeout",
            "--retries",
            "--provider",
            "--model",
            "--thinking",
            "--max",
            "--after",
            "--in",
        ]
        .contains(&key);
        if takes {
            let v = if let Some(v) = inline {
                v.to_string()
            } else {
                i += 1;
                if i == args.len() {
                    return Err(format!("argument {a}: expected one argument"));
                }
                args[i].clone()
            };
            match key {
                "--prompt" => o.prompt = Some(v),
                "--prompt-file" => o.prompt_file = Some(v),
                "--agent" => o.agent = v,
                "--tier" => o.tier = Some(v),
                "--name" => o.name = Some(v),
                "--workdir" => o.workdir = Some(v),
                "--image" => o.images.push(v),
                "--protect" => o.protect.push(v),
                "--accept" => {
                    o.accept_set = true;
                    o.accept = Some(v)
                }
                "--accept-timeout" => o.accept_timeout = v,
                "--timeout" => o.timeout = Some(v),
                "--retries" => {
                    o.retries = v.parse().map_err(|_| "invalid --retries".to_string())?
                }
                "--provider" => o.provider = Some(v),
                "--model" => o.model = Some(v),
                "--thinking" => o.thinking = Some(v),
                "--max" => o.max = Some(seconds(&v)?),
                "--after" => o.after = Some(v),
                "--in" => o.in_run = Some(v),
                _ => {}
            }
        } else {
            match a.as_str() {
                "--read-only" => o.read_only = true,
                "--in-place" => o.in_place = true,
                "--worktree" => o.worktree = true,
                "--hide-accept" => o.hide_accept = true,
                "--no-accept" => {
                    o.accept_set = true;
                    o.accept = None;
                }
                "--allow-parallel-writes" => o.parallel = true,
                "--fresh" => o.fresh = true,
                "--sync" if reply => o.sync = true,
                "--wait" if reply => o.wait = true,
                "--progress" => o.progress = true,
                "--full" => o.full = true,
                "--" => {
                    if reply && o.run.is_none() {
                        if let Some(run) = args.get(i + 1) {
                            o.run = Some(run.clone());
                            o.words.extend_from_slice(&args[i + 2..]);
                        }
                    } else {
                        o.words.extend_from_slice(&args[i + 1..]);
                    }
                    break;
                }
                x if x.starts_with('-') => return Err(format!("unrecognized arguments: {x}")),
                _ => {
                    if reply && o.run.is_none() {
                        o.run = Some(a.clone());
                    } else {
                        o.words.push(a.clone());
                    }
                }
            }
        }
        i += 1;
    }
    if reply {
        if o.run.is_none() {
            return Err("reply requires run".into());
        }
        if o.workdir.is_some()
            || o.read_only
            || o.in_place
            || o.worktree
            || o.parallel
            || o.provider.is_some()
            || o.model.is_some()
            || o.thinking.is_some()
            || o.after.is_some()
            || o.in_run.is_some()
        {
            return Err("unrecognized reply option".into());
        }
    } else if o.fresh || o.sync || o.wait {
        return Err("--fresh, --sync and --wait are only for reply".into());
    }
    if o.in_run.is_some() && !o.read_only {
        return Err("--in requires --read-only".into());
    }
    if o.in_run.is_some() && o.in_place {
        return Err("--in and --in-place contradict each other".into());
    }
    if (!collect || (reply && !o.wait)) && (o.max.is_some() || o.progress || o.full) {
        return Err("unrecognized collecting option".into());
    }
    if !o.agent.is_empty() && agents::spec(&o.agent).is_none() {
        return Err("argument --agent: invalid choice".into());
    }
    if o.tier
        .as_deref()
        .is_some_and(|t| t != "cheap" && t != "strong")
    {
        return Err("argument --tier: invalid choice".into());
    }
    if !(0..=3).contains(&o.retries) {
        return Err("argument --retries: invalid choice".into());
    }
    seconds(&o.accept_timeout)?;
    if let Some(t) = &o.timeout {
        seconds(t)?;
    }
    Ok(o)
}
fn tier_agent(tier: &str) -> Res<String> {
    let agent = setting(
        if tier == "cheap" {
            "CHEAP_AGENT"
        } else {
            "STRONG_AGENT"
        },
        agents::default_for_tier(tier).name,
    );
    if agents::spec(&agent).is_none() {
        return Err(format!(
            "DELEGATE_{}_AGENT must be one of {}, got {agent:?}",
            tier.to_uppercase(),
            agents::choices(", ")
        ));
    }
    Ok(agent)
}
pub fn strong_agent() -> Res<String> {
    tier_agent("strong")
}
fn choose_agent(o: &mut Options) -> Res<()> {
    if !o.agent.is_empty() {
        if o.tier.is_some() {
            return Err("--agent and --tier contradict each other; give one".into());
        }
        return Ok(());
    }
    let tier = o
        .tier
        .clone()
        .unwrap_or_else(|| if o.read_only { "cheap" } else { "strong" }.into());
    o.agent = tier_agent(&tier)?;
    o.tier = Some(tier.clone());
    if tier == "cheap" {
        let strong = strong_agent()?;
        if !agent_available(&o.agent) && agent_available(&strong) {
            eprintln!(
                "delegate: {} is not installed; using the strong tier ({strong})",
                o.agent
            );
            o.agent = strong;
            o.tier = Some("strong".into());
        }
    }
    Ok(())
}
pub fn read_prompt(o: &mut Options) -> Res<String> {
    let prompt = if let Some(f) = &o.prompt_file {
        if f == "-" {
            let mut x = String::new();
            std::io::stdin()
                .read_to_string(&mut x)
                .map_err(|e| e.to_string())?;
            x
        } else {
            if !Path::new(f).is_file() {
                return Err(format!("prompt file does not exist: {f}"));
            }
            read(f)
        }
    } else {
        o.prompt
            .as_ref()
            .filter(|x| !x.is_empty())
            .cloned()
            .unwrap_or_else(|| o.words.join(" "))
    };
    if prompt.trim().is_empty() {
        return Err("empty prompt; pass --prompt-file, --prompt, or trailing text".into());
    }
    for x in &mut o.images {
        if !Path::new(x).is_file() {
            return Err(format!("image does not exist: {x}"));
        }
        *x = fs::canonicalize(&*x)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into();
    }
    Ok(prompt)
}
const READ_ONLY_ZH: &str = "只读任务：不要创建、修改或删除任何文件；结束后会核对工作目录，改动不会被采纳，并会报告给委派方。用 delegate 委派子任务产生的记录在 git 忽略的目录里，不算改动，无需改动其存放位置。";
const READ_ONLY_EN: &str = "Read-only task: do not create, modify or delete files; the working directory is checked afterwards, and any change is reported to the delegator and never adopted. Records of subtasks delegated with delegate live in git-ignored directories and do not count; leave their location as it is.";

pub fn has_read_only_contract(prompt: &str) -> bool {
    prompt.contains(READ_ONLY_ZH) || prompt.contains(READ_ONLY_EN)
}

pub fn contract(
    prompt: &str,
    accept: Option<&str>,
    read_only: bool,
    revoked: bool,
    protect: &[String],
) -> String {
    let zh = prompt.chars().any(|c| {
        (0x3040..=0x30ff).contains(&(c as u32)) || (0x4e00..=0x9fff).contains(&(c as u32))
    });
    let mut notes = vec![];
    if revoked {
        notes.push(if zh{"完成标准有变：之前给出的验收命令不再适用。"}else{"The definition of done has changed: the earlier acceptance command no longer applies."}.to_string());
    }
    if read_only {
        notes.push(if zh { READ_ONLY_ZH } else { READ_ONLY_EN }.to_string());
    }
    if let Some(a) = accept.filter(|x| !x.is_empty()) {
        let cmd = script().display().to_string();
        notes.push(if zh{format!("完成标准：你结束后，委派方会在工作目录中运行下面的命令，退出码为 0 即视为完成。\n\n```sh\n{a}\n```\n\n自己跑这条命令或其他耗时的检查时，前面加 `{cmd} lane`（如 `{cmd} lane {}`）：它与本机其他检查排队、一次只跑一个，排队时间不计入你的时限。",shell_quote(a))}else{format!("Definition of done: after you finish, the delegator runs this command in the working directory; exit code 0 counts as complete.\n\n```sh\n{a}\n```\n\nWhen you run this or another heavy check yourself, prefix it with `{cmd} lane` (e.g. `{cmd} lane {}`): it queues with the other checks on this machine, one at a time, and time spent queued does not count against your time limit.",shell_quote(a))});
    }
    if !protect.is_empty() {
        let paths = protect.join(", ");
        notes.push(if zh {
            format!("受保护路径：{paths}。不要创建、修改或删除这些路径；结束后会核对改动，触及受保护路径的任务会被拒绝。")
        } else {
            format!("Protected paths: {paths}. Do not create, modify, or delete these paths; changes to protected paths will reject the run.")
        });
    }
    if notes.is_empty() {
        prompt.into()
    } else {
        format!(
            "{}\n\n---\n{}\n",
            prompt.trim_end_matches('\n'),
            notes.join("\n\n")
        )
    }
}
fn normalize_protect(paths: &mut Vec<String>) -> Res<()> {
    for path in paths.iter_mut() {
        let directory = path.ends_with('/');
        let components = Path::new(path.as_str()).components().collect::<Vec<_>>();
        if components.is_empty()
            || components
                .iter()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(format!(
                "--protect requires a repository-relative file or directory: {path}"
            ));
        }
        let normalized = components
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        *path = normalized;
        if directory {
            path.push('/');
        }
    }
    paths.sort();
    paths.dedup();
    Ok(())
}
pub fn start(mut o: Options) -> Res<PathBuf> {
    if let Some(e) = nesting_error() {
        return Err(e);
    }
    choose_agent(&mut o)?;
    let timeout = o.timeout.clone().unwrap_or_else(|| {
        agents::spec(&o.agent)
            .expect("validated agent")
            .default_timeout
            .into()
    });
    o.timeout = Some(timeout);
    let prompt = read_prompt(&mut o)?;
    let after = o.after.as_deref().map(runs::resolve).transpose()?;
    let in_run = o.in_run.as_deref().map(runs::resolve).transpose()?;
    if let Some(upstream) = &in_run {
        if after.is_none() && active(&state(upstream)) {
            return Err("--in requires a finished run unless --after is given".into());
        }
        let prior = json(upstream.join("meta.json"));
        if !prior["worktree"].is_object() || s(&prior["worktree"], "path").is_empty() {
            return Err("--in requires a run with a worktree".into());
        }
    }
    let workdir = PathBuf::from(
        o.workdir
            .clone()
            .or_else(|| {
                in_run
                    .as_ref()
                    .map(|r| s(&json(r.join("meta.json"))["worktree"], "sourceWorkdir").to_string())
            })
            .unwrap_or_else(|| {
                env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into()
            }),
    );
    if !workdir.is_dir() {
        return Err(format!("workdir does not exist: {}", workdir.display()));
    }
    let workdir = workdir.canonicalize().map_err(|e| e.to_string())?;
    missing_tools(&o.agent)?;
    if o.in_place && !o.read_only {
        return Err("--in-place is for --read-only runs; write runs work in place unless --worktree is given".into());
    }
    if o.in_place && o.worktree {
        return Err("--in-place and --worktree contradict each other".into());
    }
    let repo = git_top(&workdir);
    if !o.protect.is_empty() && repo.is_none() {
        return Err("--protect needs a git repository".into());
    }
    normalize_protect(&mut o.protect)?;
    let mut extra = json!({"env":{}});
    if let Some(top) = &repo {
        let (env, cfg, default_accept) = worktree::config(top)?;
        extra["env"] = env;
        if !o.read_only && !o.accept_set {
            o.accept = default_accept;
        }
        if let Some(upstream) = &in_run {
            let prior = json(upstream.join("meta.json"));
            extra["worktree"] = json!({"source":s(&prior["worktree"],"path"),"sourceWorkdir":s(&prior,"workdir"),"config":cfg,"in":upstream});
        } else if o.worktree || (o.read_only && !o.in_place) {
            extra["worktree"] = json!({"source":top,"sourceWorkdir":workdir,"config":cfg});
        }
    }
    if o.worktree && repo.is_none() {
        return Err(format!(
            "--worktree needs a git repository: {}",
            workdir.display()
        ));
    }
    let mode = if o.read_only { "read-only" } else { "write" };
    if let Some(upstream) = &after {
        extra["after"] = json!(upstream);
    }
    launch(o, &prompt, &workdir, mode, &extra, None)
}
pub fn reply(mut o: Options) -> Res<PathBuf> {
    let parent = runs::latest(runs::resolve(o.run.as_deref().unwrap_or("last"))?);
    let meta = json(parent.join("meta.json"));
    let summary = json(parent.join("summary.json"));
    if active(&state(&parent)) {
        return Err(format!(
            "{} is still running; wait for it before replying",
            parent.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    if o.sync {
        let parent_name = parent.file_name().unwrap_or_default().to_string_lossy();
        if let Some(existing) = all_runs().into_iter().find(|run| {
            s(&json(run.join("meta.json")), "parent") == parent_name && state(run) != "malformed"
        }) {
            return Err(format!(
                "{parent_name} already has a reply ({}); wait for it and reply to that",
                existing.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
    }
    let requested = o.agent.clone();
    let requested_tier = o.tier.clone();
    if !requested.is_empty() && requested_tier.is_some() {
        return Err("--agent and --tier contradict each other; give one".into());
    }
    let agent = if let Some(tier) = requested_tier.as_deref() {
        tier_agent(tier)?
    } else if !requested.is_empty() {
        requested
    } else {
        s(&meta, "agent").to_string()
    };
    if agent != s(&meta, "agent") {
        o.fresh = true;
    }
    let session = if o.fresh {
        None
    } else {
        summary
            .get("session")
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    if !o.fresh
        && (session.is_none()
            || (agents::spec(s(&meta, "agent"))
                .is_some_and(|a| a.session == SessionSource::DelegateFile)
                && session_file(s(&meta, "sessionDir"), session.as_deref().unwrap_or(""))
                    .is_none()))
    {
        return Err(format!("{} has no saved session to continue (runs before delegate 4.1 kept none); use --fresh to start a new session in the same place",parent.file_name().unwrap_or_default().to_string_lossy()));
    }
    if meta["worktree"].is_object() && !Path::new(s(&meta["worktree"], "path")).exists() {
        return Err(format!(
            "the worktree of {} no longer exists: {}",
            parent.file_name().unwrap_or_default().to_string_lossy(),
            s(&meta["worktree"], "path")
        ));
    }
    if let Some(e) = nesting_error() {
        return Err(e);
    }
    missing_tools(&agent)?;
    o.agent = agent;
    o.tier = requested_tier.or_else(|| {
        if o.agent == s(&meta, "agent") {
            meta["tier"].as_str().map(str::to_string)
        } else {
            None
        }
    });
    if o.agent == s(&meta, "agent") {
        o.provider = meta["provider"].as_str().map(str::to_string);
        o.model = meta["model"].as_str().map(str::to_string);
        o.thinking = meta["thinking"].as_str().map(str::to_string);
    }
    o.retries = n(&meta, "retries");
    o.protect = meta["protect"]
        .as_array()
        .map(|paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if o.timeout.is_none() {
        o.timeout = Some(s(&meta, "timeout").into());
    }
    if !o.accept_set {
        o.accept = meta["accept"].as_str().map(str::to_string);
    }
    if o.accept.as_deref() == Some("") {
        o.accept = None;
    }
    let prompt = read_prompt(&mut o)?;
    if o.sync && !meta["worktree"].is_object() {
        return Err("--sync requires a worktree conversation".into());
    }
    let synced = if o.sync {
        Some(worktree::sync(&parent, &meta)?)
    } else {
        None
    };
    let prompt = if let Some(paths) = &synced {
        if prompt
            .chars()
            .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
        {
            format!(
                "{}\n\n主控在上一轮之后的改动已同步进工作目录：{}",
                prompt.trim_end(),
                if paths.is_empty() {
                    "（无）".to_string()
                } else {
                    paths.join("、")
                }
            )
        } else {
            format!("{}\n\nThe caller's changes since the previous round have been synced into the workdir: {}", prompt.trim_end(), if paths.is_empty() { "(none)".to_string() } else { paths.join(", ") })
        }
    } else {
        prompt
    };
    let mut extra =
        json!({"parent":meta,"session":session,"env":meta["env"],"worktree":meta["worktree"]});
    if let Some(paths) = synced {
        extra["sync"] = json!({"files":paths});
    }
    if extra["worktree"].is_null() {
        extra.as_object_mut().unwrap().remove("worktree");
    }
    launch(
        o,
        &prompt,
        Path::new(s(&meta, "workdir")),
        s(&meta, "mode"),
        &extra,
        Some(&parent),
    )
}
pub fn active_machine(slots: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(e) = fs::read_dir(slots) {
        for e in e.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "slot") {
                let run = PathBuf::from(read(&p).trim());
                if run.join("meta.json").is_file() && (active(&state(&run)) || agent_alive(&run)) {
                    if !run.join(".waiting").exists() {
                        out.push(run);
                    }
                } else {
                    let _ = fs::remove_file(p);
                }
            }
        }
    }
    out
}
pub fn machine_runs(slots: &Path) -> Vec<PathBuf> {
    let _guard = lock(&slots.join(".start.lock"), true, false).ok();
    let Ok(entries) = fs::read_dir(slots) else {
        return vec![];
    };
    entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "slot"))
        .map(|entry| PathBuf::from(read(entry.path()).trim()))
        .filter(|run| run.join("meta.json").is_file() && active(&state(run)))
        .collect()
}
pub fn capacity(agent: &str, active_runs: &[PathBuf]) -> Res<()> {
    let total = number("MAX_ACTIVE", 8)?;
    let codex = number("MAX_CODEX", 4)?;
    let codex_count = active_runs
        .iter()
        .filter(|r| agents::spec(s(&json(r.join("meta.json")), "agent")).is_some_and(|a| a.heavy))
        .count();
    let reason = if total > 0 && active_runs.len() >= total as usize {
        format!(
            "{} runs are active on this machine (DELEGATE_MAX_ACTIVE={total})",
            active_runs.len()
        )
    } else if agents::spec(agent).is_some_and(|a| a.heavy)
        && codex > 0
        && codex_count >= codex as usize
    {
        format!("{codex_count} Codex runs are active on this machine (DELEGATE_MAX_CODEX={codex})")
    } else {
        String::new()
    };
    if !reason.is_empty() {
        let listing = active_runs
            .iter()
            .map(|r| {
                format!(
                    "\n  {:>5}s {:<5} {}",
                    n(&runs::status(r), "elapsedSeconds"),
                    s(&json(r.join("meta.json")), "agent"),
                    r.display()
                )
            })
            .collect::<String>();
        return Err(format!("refusing to start: {reason}; collect results with wait before starting more, or stop runs no longer needed{listing}"));
    }
    let floor_setting = setting("MIN_AVAILABLE_MB", "4096");
    if floor_setting.is_empty() || !floor_setting.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "DELEGATE_MIN_AVAILABLE_MB must be a non-negative integer (0 = no check), got {floor_setting:?}"
        ));
    }
    let floor = floor_setting.parse::<u64>().map_err(|e| e.to_string())?;
    let available = read("/proc/meminfo")
        .lines()
        .find_map(|x| {
            x.strip_prefix("MemAvailable:")
                .and_then(|x| x.split_whitespace().next())
                .and_then(|x| x.parse::<u64>().ok())
        })
        .map(|x| x / 1024);
    if floor > 0 && available.is_some_and(|x| x < floor) {
        return Err(format!("refusing to start: only {} MB of memory available (DELEGATE_MIN_AVAILABLE_MB={floor}); collect results with wait, or stop runs no longer needed",available.unwrap_or(0)));
    }
    Ok(())
}
fn tree_source_warnings(extra: &Value) -> Vec<String> {
    let tree = &extra["worktree"];
    if !tree.is_object() || !tree["in"].is_null() {
        return Vec::new();
    }
    let source = Path::new(s(tree, "source"));
    let mut warnings = Vec::new();
    for key in ["copy", "link"] {
        if let Some(items) = tree["config"][key].as_array() {
            for item in items.iter().filter_map(Value::as_str) {
                let path = source.join(item);
                let empty = path.is_dir()
                    && fs::read_dir(&path).is_ok_and(|mut entries| entries.next().is_none());
                if empty {
                    let gitlink = git(source, &["ls-files", "-s", "--", item])
                        .is_ok_and(|out| out.starts_with(b"160000 "));
                    warnings.push(format!(
                        "worktree.{key} source is empty{}: {}",
                        if gitlink {
                            " (uninitialized submodule)"
                        } else {
                            ""
                        },
                        path.display()
                    ));
                } else if !path.exists() && !path.is_symlink() {
                    warnings.push(format!(
                        "worktree.{key} source missing, skipping: {}",
                        path.display()
                    ));
                }
            }
        }
    }
    warnings
}
pub fn launch(
    mut o: Options,
    prompt: &str,
    workdir: &Path,
    mode: &str,
    extra: &Value,
    parent_path: Option<&Path>,
) -> Res<PathBuf> {
    if o.name.as_deref() == Some("") {
        o.name = None;
    }
    let root = runs_root();
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let slots = state_dir();
    fs::create_dir_all(&slots).map_err(|e| e.to_string())?;
    let _machine = lock(&slots.join(".start.lock"), true, false).map_err(|e| e.to_string())?;
    let _local = lock(&root.join(".start.lock"), true, false).map_err(|e| e.to_string())?;
    let active_runs = active_machine(&slots);
    let deferred = extra["after"].is_string();
    if !deferred {
        capacity(&o.agent, &active_runs)?;
    }
    if !deferred && mode == "write" && !o.parallel && !extra["worktree"].is_object() {
        for run in all_runs() {
            let m = json(run.join("meta.json"));
            if s(&m, "mode") == "write"
                && s(&m, "workdir") == workdir.to_string_lossy()
                && (active(&state(&run)) || agent_alive(&run))
            {
                return Err(format!("write run {} is still active in {}; wait for it, use --read-only, or pass --allow-parallel-writes",run.file_name().unwrap_or_default().to_string_lossy(),workdir.display()));
            }
        }
    }
    if let Some(parent) = parent_path {
        let p = parent.file_name().unwrap_or_default().to_string_lossy();
        let replies = all_runs()
            .into_iter()
            .filter(|r| s(&json(r.join("meta.json")), "parent") == p && state(r) != "malformed")
            .collect::<Vec<_>>();
        if let Some(last) = replies.last() {
            return Err(format!(
                "{p} already has a reply ({}); wait for it and reply to that",
                last.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
    }
    if setting("RUNS", "").is_empty() && !root.join(".gitignore").exists() {
        write(root.join(".gitignore"), "*\n")?;
    }
    let name = o.name.clone().unwrap_or_else(|| {
        prompt
            .lines()
            .find(|x| !x.trim().is_empty())
            .unwrap_or("")
            .trim()
            .chars()
            .take(120)
            .collect()
    });
    let slug = o
        .name
        .as_deref()
        .unwrap_or("")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(40)
        .collect::<String>();
    let slug = if slug.is_empty() {
        format!("{:04x}", now_ns() as u16)
    } else {
        slug
    };
    let stamp = format_time(true, "%Y%m%d-%H%M%S");
    let mut run = root.join(format!("{stamp}-{slug}"));
    while run.exists() {
        run = root.join(format!("{stamp}-{slug}-{:04x}", now_ns() as u16));
    }
    fs::create_dir(&run).map_err(|e| e.to_string())?;
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    let warnings = tree_source_warnings(extra);
    if !warnings.is_empty() {
        for warning in &warnings {
            eprintln!("delegate: {warning}");
        }
        write_json(run.join("warnings.json"), &json!(warnings))?;
    }
    let parent = &extra["parent"];
    let fork = extra
        .get("session")
        .and_then(Value::as_str)
        .filter(|x| !x.is_empty());
    let changed =
        parent.is_object() && fork.is_some() && o.accept.as_deref() != parent["accept"].as_str();
    let actual = if parent.is_object() && fork.is_some() {
        contract(
            prompt,
            if changed && !o.hide_accept {
                o.accept.as_deref()
            } else {
                None
            },
            false,
            changed && (o.hide_accept || o.accept.is_none()),
            &o.protect,
        )
    } else {
        contract(
            prompt,
            if o.hide_accept {
                None
            } else {
                o.accept.as_deref()
            },
            mode == "read-only"
                && (agents::spec(&o.agent)
                    .expect("validated agent")
                    .read_only_contract
                    || extra["worktree"].is_object()),
            false,
            &o.protect,
        )
    };
    write(
        run.join("prompt.md"),
        if actual.ends_with('\n') {
            actual
        } else {
            format!("{actual}\n")
        },
    )?;
    let mut fork_value = fork.map(str::to_string);
    if let (true, Some(fork_id), SessionSource::DelegateFile) = (
        parent.is_object(),
        fork,
        agents::spec(&o.agent).expect("validated agent").session,
    ) {
        let source = session_file(s(parent, "sessionDir"), fork_id)
            .ok_or_else(|| "missing parent session".to_string())?;
        fs::create_dir(run.join("fork")).map_err(|e| e.to_string())?;
        let target = run
            .join("fork")
            .join(source.file_name().unwrap_or_default());
        fs::copy(source, &target).map_err(|e| e.to_string())?;
        fork_value = Some(target.to_string_lossy().into());
    }
    let mut tree = extra["worktree"].clone();
    let mut wd = workdir.to_path_buf();
    if tree.is_object() && s(&tree, "path").is_empty() {
        let path = cache_dir().join("worktrees").join(format!(
            "{}-{}",
            Path::new(s(&tree, "source"))
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            run.file_name().unwrap_or_default().to_string_lossy()
        ));
        tree["path"] = json!(path);
        let origin = if tree["in"].is_string() {
            Path::new(s(&tree, "sourceWorkdir"))
        } else {
            workdir
        };
        wd = path.join(
            origin
                .strip_prefix(s(&tree, "source"))
                .unwrap_or(Path::new("")),
        );
    }
    let top = if tree.is_object() {
        let p = PathBuf::from(s(&tree, "path"));
        if p.exists() {
            Some(p)
        } else {
            Some(PathBuf::from(s(&tree, "source")))
        }
    } else {
        git_top(&wd)
    };
    let mut exclude = vec![];
    if tree.is_object() {
        for key in ["copy", "link"] {
            if let Some(a) = tree["config"][key].as_array() {
                exclude.extend(a.iter().filter_map(Value::as_str).map(str::to_string));
            }
        }
    }
    let mut base = if tree["in"].is_string() {
        None
    } else {
        top.as_deref().and_then(|p| snapshot(p, &run, &exclude))
    };
    if tree.is_object() && tree["in"].is_null() && base.is_none() {
        let _ = fs::remove_dir_all(&run);
        return Err(format!(
            "cannot snapshot {} for the worktree",
            s(&tree, "source")
        ));
    }
    let new_tree = tree.is_object() && !Path::new(s(&tree, "path")).exists();
    if new_tree {
        if let Some(v) = &mut base {
            v["large"] = json!({});
            v["submodules"] = json!({});
        }
    }
    let top = if new_tree {
        PathBuf::from(s(&tree, "path"))
    } else {
        top.unwrap_or_default()
    };
    let applied_base = if parent.is_object() {
        let parent_dir = Path::new(s(parent, "dir"));
        let synced = json(parent_dir.join(".sync-base"));
        let state = if !s(&synced, "tree").is_empty() {
            synced
        } else {
            json(parent_dir.join(".applied"))
        };
        if !s(&state, "tree").is_empty() {
            state
        } else {
            parent["appliedBase"].clone()
        }
    } else {
        Value::Null
    };
    let mut meta = json!({"run":run.file_name().unwrap_or_default().to_string_lossy(),"dir":run,"workdir":wd,"mode":mode,"agent":o.agent,"tier":o.tier,"name":o.name.clone().unwrap_or_else(||if parent.is_object(){format!("reply to {}",s(parent,"name"))}else{name}),"caller":caller(),"callerSource":caller_source().map(|(_, source)| source),"provider":o.provider,"model":o.model,"thinking":o.thinking,"timeout":o.timeout,"timeoutSeconds":seconds(o.timeout.as_deref().unwrap_or("15m"))?,"accept":o.accept,"acceptTimeoutSeconds":seconds(&o.accept_timeout)? ,"retries":o.retries,"images":o.images,"protect":o.protect,"top":if top.as_os_str().is_empty(){Value::Null}else{json!(top)},"base":base,"snapshotExclude":exclude,"worktree":tree,"env":extra.get("env").cloned().unwrap_or(json!({})),"chainBase":if !s(parent,"chainBase").is_empty(){s(parent,"chainBase").to_string()}else{base.as_ref().map(|x|s(x,"tree").to_string()).unwrap_or_default()},"appliedBase":applied_base,"sessionDir":run.join("session"),"parent":if parent.is_object(){parent["run"].clone()}else{Value::Null},"fork":fork_value,"after":extra["after"],"parallel":o.parallel,"startedAt":iso(),"startedEpoch":epoch() as i64,"startedNs":now_ns() as u64});
    let (agent_bin, agent_version) = agent_identity(s(&meta, "agent"))?;
    meta["agentBin"] = json!(agent_bin);
    if let Some(version) = agent_version {
        meta["agentVersion"] = json!(version);
    }
    write_json(run.join("meta.json"), &meta)?;
    if !extra["sync"].is_null() {
        write_json(run.join("sync.json"), &extra["sync"])?;
    }
    if deferred {
        touch(run.join(".waiting"));
    }
    runs::prune();
    let hash = sha1_smol::Sha1::from(run.to_string_lossy().as_bytes())
        .digest()
        .to_string();
    write(
        slots.join(format!("{}.slot", &hash[..16])),
        format!("{}\n", run.display()),
    )?;
    let log = File::create(run.join("supervisor.log")).map_err(|e| e.to_string())?;
    let mut c = Command::new(script());
    c.arg("_supervise")
        .arg(&run)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log))
        .env_remove("DELEGATE_LANE_HELD");
    group(&mut c);
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let startup_timeout = if cfg!(debug_assertions) {
        std::env::var("DELEGATE_TEST_STARTUP_TIMEOUT")
            .ok()
            .and_then(|value| seconds(&value).ok())
            .unwrap_or(5.0)
    } else {
        5.0
    };
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(startup_timeout);
    while std::time::Instant::now() < deadline {
        if run.join("pid").is_file() {
            return Ok(run);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    end_group(child.id() as i32, 2.0);
    child.wait().map_err(|e| e.to_string())?;
    write_json(
        run.join("summary.json"),
        &json!({"state":"crashed","error":"supervisor did not start"}),
    )?;
    write(run.join("exit_code"), "1\n")?;
    Err(format!(
        "supervisor did not start; see {}",
        run.join("supervisor.log").display()
    ))
}
