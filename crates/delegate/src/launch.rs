use crate::agents::{self, agent_identity, missing_tools, nesting_error, session_file};
use crate::common::*;
use crate::runs::{self, active, agent_alive, all_runs, state};
use crate::{models, worktree};
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
    pub tier_inferred: bool,
    pub model_name: Option<String>,
    pub overflow: Option<String>,
    pub thinking_cli: Option<String>,
    pub class: Option<String>,
    pub agent_pinned: bool,
    /// The model came from a tier rather than `--model` or the run being continued,
    /// so `launch` may pick again once it counts the machine's runs (`maxActive`).
    pub tier_picked: bool,
    pub name: Option<String>,
    pub workdir: Option<String>,
    pub images: Vec<String>,
    pub share: Vec<String>,
    pub protect: Vec<String>,
    pub allow: Vec<String>,
    pub when_done: bool,
    pub protect_reasons: std::collections::BTreeMap<String, String>,
    pub read_only: bool,
    pub shell: bool,
    pub in_place: bool,
    pub worktree: bool,
    pub accept: Option<String>,
    pub accept_set: bool,
    pub accept_also: Vec<String>,
    pub hide_accept: bool,
    pub accept_timeout: Option<String>,
    pub evidence: Option<String>,
    pub evidence_set: bool,
    pub evidence_timeout: Option<String>,
    pub timeout: Option<String>,
    pub retries: i64,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub review_model: Option<String>,
    pub review_full: bool,
    pub thinking: Option<String>,
    pub parallel: bool,
    pub fresh: bool,
    pub sync: bool,
    pub sync_agent: bool,
    pub no_sync: bool,
    pub wait: bool,
    pub max: Option<f64>,
    pub progress: bool,
    pub full: bool,
    pub run: Option<String>,
    pub in_run: Option<String>,
    pub minor: bool,
    pub over_limit: Option<String>,
    pub rework: Option<Value>,
    pub max_answer: Option<u64>,
    pub work: Option<String>,
    pub no_remote: bool,
    pub no_brief_check: bool,
    pub no_scout: bool,
    pub brief_checked: bool,
}
impl Options {
    pub fn new() -> Self {
        Self {
            retries: 1,
            ..Default::default()
        }
    }

    fn append_accept(&mut self) {
        if !self.accept_also.is_empty() {
            let commands = self
                .accept
                .iter()
                .chain(self.accept_also.iter())
                .filter(|command| !command.is_empty())
                .map(String::as_str)
                .collect::<Vec<_>>();
            self.accept = Some(commands.join(" && ")).filter(|command| !command.is_empty());
        }
    }
}
fn launch_valid_flags(reply: bool, collect: bool) -> Vec<&'static str> {
    let mut flags = vec![
        "--prompt",
        "--prompt-file",
        "--tier",
        "--name",
        "--image",
        "--allow",
        "--accept",
        "--accept-also",
        "--no-accept",
        "--hide-accept",
        "--accept-timeout",
        "--evidence",
        "--no-evidence",
        "--evidence-timeout",
        "--no-remote",
        "--no-brief-check",
        "--timeout",
        "--max-answer",
        "--model",
        "--review-model",
        "--review-full",
        "--thinking",
        "--overflow",
        "--work",
        "--json",
    ];
    if reply {
        flags.extend_from_slice(&[
            "--fresh",
            "--sync",
            "--sync=agent",
            "--no-sync",
            "--wait",
            "--when-done",
            "--minor",
            "--over-limit",
        ]);
    } else {
        flags.extend_from_slice(&[
            "--workdir",
            "--share",
            "--read-only",
            "--shell",
            "--in-place",
            "--worktree",
            "--in",
            "--protect",
            "--protect-reason",
            "--retries",
            "--provider",
            "--class",
            "--no-scout",
            "--allow-parallel-writes",
        ]);
    }
    if collect {
        flags.extend_from_slice(&["--max", "--progress", "--full"]);
    }
    flags
}

pub fn parse_launch(args: &[String], reply: bool, collect: bool) -> Res<Options> {
    let cmd_name = if reply {
        "reply"
    } else if collect {
        "run"
    } else {
        "start"
    };
    let mut o = Options::new();
    let mut no_accept = false;
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
                "--protect-reason",
                "--retries",
                "--provider",
                "--allow-parallel-writes",
            ]
            .contains(&key)
        {
            return Err(unrecognized_arg(a, &launch_valid_flags(reply, collect), cmd_name));
        }
        if key == "--protect-reason" {
            if inline.is_some() || i + 2 >= args.len() {
                return Err("--protect-reason expects PATH REASON as two arguments".into());
            }
            let mut paths = vec![args[i + 1].clone()];
            normalize_protect(&mut paths)?;
            let path = paths.remove(0);
            let reason = args[i + 2].clone();
            if reason.trim().is_empty() {
                return Err("--protect-reason requires a non-empty reason".into());
            }
            if o.protect_reasons
                .get(&path)
                .is_some_and(|old| old != &reason)
            {
                return Err(format!("conflicting protection reasons for {path}"));
            }
            o.protect.push(path.clone());
            o.protect_reasons.insert(path, reason);
            i += 3;
            continue;
        }
        let takes = [
            "--prompt",
            "--prompt-file",
            "--agent",
            "--tier",
            "--name",
            "--workdir",
            "--image",
            "--share",
            "--protect",
            "--allow",
            "--accept",
            "--accept-also",
            "--accept-timeout",
            "--evidence",
            "--evidence-timeout",
            "--timeout",
            "--max-answer",
            "--retries",
            "--provider",
            "--model",
            "--review-model",
            "--thinking",
            "--class",
            "--overflow",
            "--max",
            "--after",
            "--in",
            "--over-limit",
            "--work",
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
                "--agent" => {
                    return Err(format!(
                        "--agent was removed in 6.0: delegate runs Pi only; pick its model with --tier or --model (got --agent {v})"
                    ))
                }
                "--tier" => o.tier = Some(v),
                "--overflow" => {
                    if !["queue", "next"].contains(&v.as_str()) {
                        return Err("--overflow must be next|queue".into());
                    }
                    o.overflow = Some(v);
                },
                "--name" => o.name = Some(v),
                "--workdir" => o.workdir = Some(v),
                "--image" => o.images.push(v),
                "--share" if !reply => o.share.push(v),
                "--protect" => o.protect.push(v),
                "--allow" => o.allow.push(v),
                "--accept" => {
                    o.accept_set = true;
                    o.accept = Some(v)
                }
                "--accept-also" => o.accept_also.push(v),
                "--accept-timeout" => o.accept_timeout = Some(v),
                "--evidence" => {
                    o.evidence_set = true;
                    o.evidence = Some(v);
                }
                "--evidence-timeout" => o.evidence_timeout = Some(v),
                "--timeout" => o.timeout = Some(v),
                "--max-answer" => {
                    o.max_answer = Some(
                        v.parse::<u64>()
                            .ok()
                            .filter(|n| *n > 0)
                            .ok_or("--max-answer must be a positive integer")?,
                    )
                }
                "--retries" => {
                    o.retries = v.parse().map_err(|_| "invalid --retries".to_string())?
                }
                "--provider" => o.provider = Some(v),
                "--model" => o.model = Some(v),
                "--review-model" => { models::validate_model(&v)?; o.review_model = Some(v); }
                "--class" => { crate::fit::validate_class(&v)?; o.class = Some(v); },
                "--thinking" => { models::validate_thinking(&v)?; o.thinking = Some(v.clone()); o.thinking_cli = Some(v); },
                "--max" => o.max = Some(seconds(&v)?),
                "--after" => {
                    return Err("--after was removed in 6.22.0: wait for the upstream run and start the next step directly".into())
                }
                "--in" => o.in_run = Some(v),
                "--over-limit" if reply => o.over_limit = Some(v),
                "--work" => o.work = Some(v),
                _ => {}
            }
        } else {
            match a.as_str() {
                "--read-only" => o.read_only = true,
                "--shell" => o.shell = true,
                "--in-place" => o.in_place = true,
                "--worktree" => o.worktree = true,
                "--hide-accept" => o.hide_accept = true,
                "--no-accept" => {
                    no_accept = true;
                    o.accept_set = true;
                    o.accept = None;
                }
                "--no-evidence" => {
                    o.evidence_set = true;
                    o.evidence = None;
                }
                "--no-remote" => o.no_remote = true,
                "--no-brief-check" => o.no_brief_check = true,
                "--no-scout" => o.no_scout = true,
                "--review-full" => o.review_full = true,
                "--allow-parallel-writes" => o.parallel = true,
                "--fresh" => o.fresh = true,
                "--sync" if reply => o.sync = true,
                "--sync=agent" if reply => {
                    o.sync = true;
                    o.sync_agent = true;
                }
                "--no-sync" if reply => o.no_sync = true,
                "--wait" if reply => o.wait = true,
                "--when-done" if reply => o.when_done = true,
                "--json" => crate::output::enable_json(),
                "--minor" if reply => o.minor = true,
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
                x if x.starts_with('-') => {
                    return Err(unrecognized_arg(
                        x,
                        &launch_valid_flags(reply, collect),
                        cmd_name,
                    ));
                }
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
    if no_accept && !o.accept_also.is_empty() {
        return Err("--no-accept and --accept-also contradict each other".into());
    }
    if o.sync && o.no_sync {
        return Err("--sync and --no-sync are exclusive".into());
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
            || o.in_run.is_some()
        {
            return Err("unrecognized reply option".into());
        }
    } else if o.fresh || o.sync || o.no_sync || o.wait {
        return Err("--fresh, --sync, --no-sync and --wait are only for reply".into());
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
    if o.tier
        .as_deref()
        .is_some_and(|t| !agents::ALL_TIERS.contains(&t))
    {
        return Err("argument --tier: invalid choice".into());
    }
    if let Some(model) = &o.model {
        models::validate_model(model)?;
    }
    if !(0..=3).contains(&o.retries) {
        return Err("argument --retries: invalid choice".into());
    }
    if let Some(t) = &o.accept_timeout {
        seconds(t)?;
    }
    if let Some(t) = &o.evidence_timeout {
        seconds(t)?;
    }
    if let Some(t) = &o.timeout {
        seconds(t)?;
    }
    Ok(o)
}
pub const WRITE_BOUNDARY: &str = "只在当前工作目录内改文件；提示里出现的其他绝对路径只读";
const READS_ONLY: &str = "scout and review tiers only read; writes require --tier write or --model";
fn choose_tier(o: &mut Options) -> Res<()> {
    o.agent = agents::AGENT.into();
    if o.tier
        .as_deref()
        .is_some_and(|t| ["scout", "review"].contains(&t))
    {
        o.read_only = true;
    }
    if o.shell && !o.read_only {
        return Err("--shell requires --read-only or --tier scout|review".into());
    }
    if o.model.is_some() {
        o.agent_pinned = true;
        return Ok(());
    }
    if o.tier.is_none() {
        o.tier = Some(models::infer(o.read_only).into());
        o.tier_inferred = true;
    }
    if !o.read_only
        && o.tier
            .as_deref()
            .is_some_and(|t| ["scout", "review"].contains(&t))
    {
        return Err(READS_ONLY.into());
    }
    Ok(())
}

fn require_models(config: &Value, listed: &[models::PiModel]) -> Res<Value> {
    let mut result = json!({});
    for tier in agents::TIERS {
        match models::pick(config, tier, None, listed, agents::local_clock(), &|_| 0) {
            Ok(pick) => result[tier] = json!(pick.model),
            Err(error) => {
                return Err(format!(
                    "model tiers not configured or invalid: {tier}: {error}\n{}",
                    agents::tiers_guide(config)
                ))
            }
        }
    }
    Ok(result)
}

/// Thinking override from CLI `--thinking`, falling back to `--class` default thinking
/// (`build|design|scout|review` -> `medium`, `hard` -> `high`) when no explicit `--thinking` was given.
fn effective_thinking_override(o: &Options) -> Option<&str> {
    o.thinking_cli
        .as_deref()
        .or_else(|| o.class.as_deref().and_then(crate::fit::default_thinking))
}

/// Resolve explicit identity independently of purpose, then choose thinking.
fn apply_tier_model(o: &mut Options, config: &Value) -> Res<Value> {
    let listed = agents::pi_models();
    let models = require_models(config, &listed)?;
    let thinking_override = effective_thinking_override(o).map(str::to_string);
    if o.agent_pinned {
        let name = o.model.as_deref().unwrap_or("");
        let model = models::resolve_model(name, config, &listed)?;
        o.model_name = Some(name.into());
        o.thinking = models::thinking(thinking_override.as_deref(), &json!(name), config)?;
        o.model = Some(model);
    } else {
        let pick = models::pick(
            config,
            o.tier.as_deref().unwrap_or("review"),
            thinking_override.as_deref(),
            &listed,
            agents::local_clock(),
            &|_| 0,
        )?;
        o.model_name = Some(pick.name);
        o.model = Some(pick.model);
        o.thinking = pick.thinking;
        o.tier_picked = true;
    }
    Ok(models)
}

/// Under the start lock, count maxActive by resolved id and snapshot candidates for retry.
/// `listed` is pre-fetched outside `.start.lock` so no subprocess runs while holding the lock.
fn pick_tier_under_cap(
    o: &mut Options,
    extra: &mut Value,
    active_runs: &[PathBuf],
    listed: &[models::PiModel],
) -> Res<()> {
    let busy = active_runs
        .iter()
        .map(|run| s(&json(run.join("meta.json")), "model").to_string())
        .collect::<Vec<_>>();
    let running = |model: &str| busy.iter().filter(|m| *m == model).count();
    let config = &extra["tierConfig"];
    let tier = o.tier.as_deref().unwrap_or("review");
    let thinking_override = effective_thinking_override(o).map(str::to_string);
    let pick = models::pick(
        config,
        tier,
        thinking_override.as_deref(),
        listed,
        agents::local_clock(),
        &running,
    )?;
    if o.model.as_deref() != Some(&pick.model) {
        eprintln!(
            "delegate: {tier} 档首选已到本机 maxActive 上限，改用 {}",
            pick.model
        );
    }
    o.model_name = Some(pick.name);
    o.model = Some(pick.model.clone());
    o.thinking = pick.thinking;
    extra["tierModels"] = agents::ALL_TIERS
        .iter()
        .map(|t| {
            let model = models::pick(config, t, None, listed, agents::local_clock(), &running)
                .map(|p| p.model)
                .unwrap_or_default();
            (t.to_string(), json!(model))
        })
        .collect::<serde_json::Map<_, _>>()
        .into();
    extra["tierCandidateIndex"] = json!(pick.index);
    extra["tierCandidates"] = json!(models::candidates(
        &extra["tierConfig"],
        tier,
        thinking_override.as_deref(),
        listed
    )?
    .iter()
    .map(models::Pick::value)
    .collect::<Vec<_>>());
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

pub fn contract(
    prompt: &str,
    accept: Option<&str>,
    read_only: bool,
    revoked: bool,
    protect: &[String],
    reasons: &std::collections::BTreeMap<String, String>,
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
        let paths = protect
            .iter()
            .map(|path| {
                reasons
                    .get(path)
                    .map_or_else(|| path.clone(), |reason| format!("{path}: {reason}"))
            })
            .collect::<Vec<_>>()
            .join("\n");
        notes.push(if zh {
            format!("受保护路径：{paths}。不要创建、修改或删除这些路径；结束后会核对改动，触及受保护路径的任务会被拒绝。")
        } else {
            format!("Protected paths: {paths}. Do not create, modify, or delete these paths; changes to protected paths will reject the run.")
        });
        notes.push(if zh {
            "若正确完成任务必须修改受保护路径，停止该实现路线，报告路径、必要修改和原因，等待主控处理；不得为避开保护而迁移、复制逻辑或削弱测试。"
        } else {
            "If completing the task correctly requires changing a protected path, stop that implementation route, report the path, necessary changes and reason, and wait for the delegator. Do not move or copy logic or weaken tests to bypass protection."
        }.to_string());
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
pub(crate) fn normalize_protect(paths: &mut Vec<String>) -> Res<()> {
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
/// A name is typed again in later commands: stop at the first clause, not at 40 characters of prose.
fn prompt_name(prompt: &str) -> String {
    let line = prompt
        .lines()
        .find(|s| !s.trim().is_empty())
        .unwrap_or("task")
        .trim()
        .trim_start_matches(['#', '*', '-', ' ']);
    let clause = line
        .split([
            '：', '，', '。', '；', '（', '(', ':', ',', ';', '、', '—', '`', '！', '？', '!', '?',
        ])
        .map(str::trim)
        // "目标：…" and "Goal: …" label the sentence; the name is what follows.
        .find(|part| {
            !part.is_empty()
                && !["目标", "任务", "背景", "goal", "task", "objective"]
                    .contains(&part.to_lowercase().as_str())
        })
        .unwrap_or(line);
    let mut name = String::new();
    for word in clause.split_whitespace() {
        let next = name.chars().count() + word.chars().count() + usize::from(!name.is_empty());
        if next > 24 {
            break;
        }
        if !name.is_empty() {
            name.push(' ');
        }
        name.push_str(word);
    }
    if name.is_empty() {
        name = clause.chars().take(24).collect();
    }
    if name.is_empty() {
        "task".into()
    } else {
        name
    }
}

fn effective_defaults(o: &mut Options, config: &crate::config::Config) -> Value {
    let mut applied = json!({});
    let defaults = &config.value["defaults"];
    for key in [
        "worktree",
        "protect",
        "acceptAlso",
        "timeout",
        "evidence",
        "keepCommits",
    ] {
        let Some(v) = defaults.get(key) else { continue };
        let used = match key {
            "worktree" if !o.read_only && !o.in_place && !o.worktree => {
                o.worktree = v.as_bool().unwrap();
                true
            }
            "protect" if o.protect.is_empty() && !o.read_only => {
                o.protect = v
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                true
            }
            "acceptAlso"
                if !o.read_only
                    && o.accept_also.is_empty()
                    && !(o.accept_set && o.accept.is_none()) =>
            {
                o.accept_also = v
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                true
            }
            "timeout" if o.timeout.is_none() => {
                o.timeout = v.as_str().map(str::to_string);
                true
            }
            "evidence" if !o.read_only && !o.evidence_set => {
                o.evidence = v.as_str().map(str::to_string);
                true
            }
            _ => false,
        };
        if used {
            applied[key] = json!({"value":v,"source":config.origins["defaults"][key]});
        }
    }
    if !o.read_only && o.accept_timeout.is_none() {
        if let Some(v) = config.value.get("acceptTimeout") {
            o.accept_timeout = v.as_str().map(str::to_string);
            applied["acceptTimeout"] =
                json!({"value": v, "source": config.origins["acceptTimeout"]});
        }
    }
    applied
}

fn standing_text(config: &crate::config::Config, read_only: bool, deny: &Value) -> (String, Value) {
    let mut text = Vec::new();
    let mut sources = json!({});
    for key in ["all", if read_only { "readOnly" } else { "write" }] {
        let line = crate::config::text_lines(&config.value["standing"][key]);
        if !line.trim().is_empty() {
            text.push(line);
            sources[key] = config.origins["standing"][key].clone();
        }
    }
    let rules = deny
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let cmd = r["argv"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(shell_quote)
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                "{cmd}{}（{}）",
                if b(r, "exact") {
                    " [exact]"
                } else {
                    " [prefix]"
                },
                s(r, "hint").replace(['\n', '\r'], " ")
            )
        })
        .collect::<Vec<_>>();
    if !rules.is_empty() {
        text.push(format!("这些命令被拦截：{}", rules.join("；")));
        sources["agentDeny"] = config.origins["agentDeny"].clone();
    }
    (text.join("\n"), sources)
}

fn area_prefix_matches(candidate: &str, prefix_raw: &str) -> bool {
    let pref = prefix_raw.trim().trim_start_matches("./");
    if pref.is_empty() {
        return false;
    }
    if pref.contains('*') || pref.contains('?') {
        return worktree::matches_rule(candidate, &[pref.to_string()]);
    }
    let clean_cand = candidate.trim_start_matches("./").trim_end_matches('/');
    let clean_pref = pref.trim_end_matches('/');
    clean_cand == clean_pref
        || clean_cand
            .strip_prefix(clean_pref)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn collect_area_prefixes(spec: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(p) = spec.get("paths") {
        match p {
            Value::String(s) => out.push(s.clone()),
            Value::Array(arr) => {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        out.push(s.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(s) = spec.get("prefix").and_then(Value::as_str) {
        out.push(s.to_string());
    }
    out
}

fn read_l0_three_lines(repo: &Path, l0_raw: &str) -> Option<Vec<String>> {
    let l0_path = {
        let p = Path::new(l0_raw.trim());
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            repo.join(p)
        }
    };
    let text = fs::read_to_string(&l0_path).ok()?;
    let lines: Vec<String> = text
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty())
        .take(3)
        .map(str::to_string)
        .collect();
    (!lines.is_empty()).then_some(lines)
}

fn work_item_path_hints(item: &crate::work::WorkItem, repo: &Path) -> Vec<String> {
    let mut hints = std::collections::BTreeSet::new();
    for art in &item.artifacts {
        let clean = art.path.trim().trim_start_matches("./");
        if !clean.is_empty() {
            hints.insert(clean.to_string());
        }
    }
    for tok in crate::brief_check::extract_repo_path_tokens(&item.title, repo)
        .into_iter()
        .chain(crate::brief_check::extract_repo_path_tokens(&item.done_when, repo))
    {
        hints.insert(tok);
    }
    for f in &item.followups {
        for tok in crate::brief_check::extract_repo_path_tokens(&f.text, repo) {
            hints.insert(tok);
        }
    }
    if !item.brief.trim().is_empty() {
        let bpath = {
            let p = Path::new(item.brief.trim());
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                repo.join(p)
            }
        };
        if let Ok(btext) = fs::read_to_string(&bpath) {
            for tok in crate::brief_check::extract_repo_path_tokens(&btext, repo) {
                hints.insert(tok);
            }
        }
    }
    for rid in &item.runs {
        if let Some(rdir_str) = item.run_dirs.get(rid) {
            let rdir = Path::new(rdir_str);
            for p in worktree::run_files(rdir) {
                hints.insert(p);
            }
        }
    }
    hints.into_iter().collect()
}

/// Match repository `.delegate.json` `areas` against brief, `--work` paths, and `workdir`.
/// Runs OUTSIDE `state/.state.lock` so reading the L0 pointer file never touches the lock.
fn match_area_for_start(
    areas: &Value,
    repo: &Path,
    workdir: &Path,
    prompt: &str,
    work_item: Option<&crate::work::WorkItem>,
) -> Option<Value> {
    let map = areas.as_object()?;
    if map.is_empty() {
        return None;
    }
    let mut candidates = std::collections::BTreeSet::new();
    for tok in crate::brief_check::extract_repo_path_tokens(prompt, repo) {
        candidates.insert(tok);
    }
    if let Some(item) = work_item {
        for tok in work_item_path_hints(item, repo) {
            candidates.insert(tok);
        }
    }
    if let Ok(rel_wd) = workdir.strip_prefix(repo) {
        let rel_s = rel_wd.to_string_lossy().trim_matches('/').to_string();
        if !rel_s.is_empty() {
            candidates.insert(rel_s);
        }
    }
    if candidates.is_empty() {
        return None;
    }

    let mut area_names: Vec<&String> = map.keys().collect();
    area_names.sort();

    // Deterministic tie-breaking: longest matched prefix > distinct matched paths > area name.
    let mut best: Option<(usize, usize, String, String, Vec<String>)> = None;
    for name in area_names {
        let spec = &map[name];
        let prefixes = collect_area_prefixes(spec);
        let l0 = s(spec, "l0").trim();
        if prefixes.is_empty() || l0.is_empty() {
            continue;
        }
        let mut matched = Vec::new();
        let mut max_pref_len = 0usize;
        for cand in &candidates {
            let mut hit_len = 0usize;
            for pref in &prefixes {
                if area_prefix_matches(cand, pref) {
                    hit_len = hit_len.max(pref.trim().trim_start_matches("./").trim_end_matches('/').len());
                }
            }
            if hit_len > 0 {
                matched.push(cand.clone());
                max_pref_len = max_pref_len.max(hit_len);
            }
        }
        if matched.is_empty() {
            continue;
        }
        let replace = match &best {
            None => true,
            Some((best_len, best_cnt, best_name, _, _)) => {
                (max_pref_len, matched.len(), std::cmp::Reverse(name.as_str()))
                    > (*best_len, *best_cnt, std::cmp::Reverse(best_name.as_str()))
            }
        };
        if replace {
            best = Some((max_pref_len, matched.len(), name.clone(), l0.to_string(), matched));
        }
    }

    let (_, _, area, l0, matched) = best?;
    let Some(l0_lines) = read_l0_three_lines(repo, &l0) else {
        eprintln!(
            "delegate: areas.{area}: L0 指针文件缺失或为空（{l0}），跳过 A/B 分组"
        );
        return None;
    };
    Some(json!({
        "area": area,
        "l0": l0,
        "l0Lines": l0_lines,
        "matchedPaths": matched.into_iter().take(10).collect::<Vec<_>>(),
    }))
}

/// Atomically advance per-(repo, area) dispatch counter inside `state/.state.lock` and assign
/// deterministic A/B group by parity: odd `seq` -> `"B"` (treatment, inject L0 3 lines),
/// even `seq` -> `"A"` (control, unmodified brief).
fn assign_area_ab_locked(state: &Path, repo_key: &str, candidate: &Value) -> Value {
    let area = s(candidate, "area");
    let counter_path = state.join("area-ab.json");
    let mut doc = json(&counter_path);
    if !doc.is_object() {
        doc = json!({});
    }
    let repo_bucket = if repo_key.is_empty() {
        "_default"
    } else {
        repo_key
    };
    if !doc[repo_bucket].is_object() {
        doc[repo_bucket] = json!({});
    }
    let prev = doc[repo_bucket][area].as_u64().unwrap_or(0);
    let seq = prev.saturating_add(1);
    doc[repo_bucket][area] = json!(seq);
    let _ = write_json(&counter_path, &doc);
    let group = if seq % 2 == 1 { "B" } else { "A" };
    let injected = group == "B";
    json!({
        "area": area,
        "group": group,
        "seq": seq,
        "l0": candidate["l0"].clone(),
        "injected": injected,
        "matchedPaths": candidate["matchedPaths"].clone(),
        "l0Lines": candidate["l0Lines"].clone(),
    })
}

pub(crate) fn protected_paths_in_prompt(
    prompt: &str,
    repo: Option<&Path>,
    protect: &[String],
    allow: &[String],
) -> Vec<String> {
    if protect.is_empty() {
        return Vec::new();
    }
    let repo_prefix = repo
        .map(|p| p.to_string_lossy().trim_end_matches('/').to_string())
        .filter(|p| !p.is_empty())
        .map(|p| format!("{p}/"));
    let mut found = std::collections::BTreeSet::new();
    let bytes = prompt.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if !(b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'.' | b'+')) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric()
                || matches!(bytes[i], b'/' | b'_' | b'-' | b'.' | b'+'))
        {
            i += 1;
        }
        // Skip URLs such as https://example.com/apps/foo.
        if start >= 1 && bytes[start - 1] == b':' && bytes[start..i].starts_with(b"//") {
            continue;
        }
        let mut token = &prompt[start..i];
        while let Some(stripped) = token.strip_prefix("./") {
            token = stripped;
        }
        token = token.trim_end_matches('.');
        if token.is_empty() {
            continue;
        }
        if token.starts_with('/') {
            if let Some(prefix) = &repo_prefix {
                if let Some(rel) = token.strip_prefix(prefix.as_str()) {
                    token = rel;
                } else {
                    continue;
                }
            } else {
                continue;
            }
        }
        let dir = token.ends_with('/');
        let parts = Path::new(token).components().collect::<Vec<_>>();
        if parts.is_empty()
            || parts
                .iter()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            continue;
        }
        let mut norm = parts
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if dir {
            norm.push('/');
        }
        if worktree::matches_rule(&norm, protect) && !worktree::matches_rule(&norm, allow) {
            found.insert(norm);
        }
    }
    found.into_iter().collect()
}

fn note_prompt_protected_paths(paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    let flags = paths
        .iter()
        .map(|p| format!("--allow {}", shell_quote(p)))
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!(
        "delegate: note: prompt mentions protected path(s) without --allow: {} (if edits are intended, add: {flags}) / 提示：正文提到受保护路径 {}（如需修改请加：{flags}）",
        paths.join(", "),
        paths.join("、"),
    );
}

fn validate_allow(allow: &mut Vec<String>, repo: Option<&Path>, protect: &[String]) -> Res<()> {
    for path in allow.iter_mut() {
        if repo.is_some_and(|top| top.join(&*path).is_dir()) && !path.ends_with('/') {
            path.push('/');
        }
    }
    normalize_protect(allow)?;
    for path in allow.iter() {
        if !worktree::matches_rule(path, protect) {
            return Err(format!("--allow must be inside a protected path: {path}"));
        }
    }
    Ok(())
}

struct ReconstructedInSnapshot {
    origin_repo: PathBuf,
    tree_sha: String,
    base_commit: Option<String>,
    archive_dir: Option<PathBuf>,
    summary: String,
}

struct TempRebuildDir {
    path: PathBuf,
}

impl TempRebuildDir {
    fn create() -> Res<Self> {
        let dir = env::temp_dir().join(format!(
            "delegate-in-rebuild-{}-{:016x}",
            std::process::id(),
            now_ns() as u64
        ));
        fs::create_dir_all(&dir).map_err(|e| format!("create temp dir {}: {e}", dir.display()))?;
        Ok(Self { path: dir })
    }
}

impl Drop for TempRebuildDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn is_upstream_worktree_live(upstream: &Path, prior: &Value) -> bool {
    let wt_str = s(&prior["worktree"], "path");
    if wt_str.is_empty() {
        return false;
    }
    let wt_path = Path::new(wt_str);
    if !wt_path.is_dir() || !wt_path.join(".git").exists() {
        return false;
    }
    if upstream.join(".applied-cleaned").exists() || crate::archive::is_record(upstream) {
        return false;
    }
    if crate::pool::is_slot(wt_path) || b(&prior["worktree"], "pooled") {
        return crate::pool::slot_belongs_to_run(wt_path, upstream, prior);
    }
    true
}

fn git_object_type(repo: &Path, sha: &str) -> Option<String> {
    if sha.is_empty() {
        return None;
    }
    let out = git_command()
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "-t", sha])
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

fn resolve_upstream_archive_dir(upstream: &Path, prior: &Value) -> Option<PathBuf> {
    if crate::archive::is_record(upstream) {
        if let Some(parent) = upstream.parent() {
            if parent.is_dir() {
                return Some(parent.to_path_buf());
            }
        }
    }
    let arch_json = json(upstream.join("archive.json"));
    let recorded = s(&arch_json, "path");
    if !recorded.is_empty() {
        let p = PathBuf::from(recorded);
        if p.is_dir() {
            return Some(p);
        }
    }
    let run_id = {
        let r = s(prior, "run");
        if !r.is_empty() {
            r.to_string()
        } else {
            upstream
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }
    };
    if !run_id.is_empty() {
        let p = cache_dir().join("archive").join(run_id);
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

fn find_upstream_record_file(
    upstream: &Path,
    archive_dir: Option<&Path>,
    name: &str,
) -> Option<PathBuf> {
    let direct = upstream.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    if let Some(arch) = archive_dir {
        let rec = arch.join("record").join(name);
        if rec.is_file() {
            return Some(rec);
        }
    }
    None
}

fn fail_closed_in_err(run_id: &str, wt_path: &str, reason: &str) -> String {
    let wt_note = if wt_path.is_empty() {
        "未记录 worktree 路径".to_string()
    } else {
        format!("{wt_path} 已清理或已释放")
    };
    format!(
        "--in {run_id}: 原 worktree 不可用（{wt_note}），且无法从归档重建只读快照（{reason}）；已拒绝启动，避免审查员静默读到主干。\n\
         下一步：用 `delegate show {run_id}` 或 `delegate diff {run_id}` 查看归档记录；若确需审查当前主干，去掉 `--in {run_id}` 后重跑。"
    )
}

fn apply_patch_onto_base(
    origin_git_dir: &Path,
    base_sha: &str,
    patch_bytes: &[u8],
) -> Result<String, String> {
    let tmp = TempRebuildDir::create()?;
    let wt = tmp.path.join("wt");
    fs::create_dir_all(&wt).map_err(|e| format!("create rebuild worktree: {e}"))?;
    let idx = tmp.path.join("index");

    let run_git = |args: &[&str], stdin_data: Option<&[u8]>| -> Result<Vec<u8>, String> {
        let mut cmd = git_command();
        cmd.arg("--git-dir")
            .arg(origin_git_dir)
            .arg("--work-tree")
            .arg(&wt)
            .env("GIT_INDEX_FILE", &idx)
            .args(args);
        if stdin_data.is_some() {
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn().map_err(|e| e.to_string())?;
            if let Some(bytes) = stdin_data {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = std::io::Write::write_all(&mut stdin, bytes);
                }
            }
            let out = child.wait_with_output().map_err(|e| e.to_string())?;
            if out.status.success() {
                Ok(out.stdout)
            } else {
                Err(clip(String::from_utf8_lossy(&out.stderr).trim(), 300))
            }
        } else {
            let out = cmd.output().map_err(|e| e.to_string())?;
            if out.status.success() {
                Ok(out.stdout)
            } else {
                Err(clip(String::from_utf8_lossy(&out.stderr).trim(), 300))
            }
        }
    };

    run_git(&["read-tree", base_sha], None)
        .map_err(|e| format!("read-tree {base_sha}: {e}"))?;
    run_git(&["checkout-index", "-a", "-f"], None)
        .map_err(|e| format!("checkout-index {base_sha}: {e}"))?;
    if !patch_bytes.is_empty() {
        run_git(&["apply", "--index", "--binary"], Some(patch_bytes))
            .map_err(|e| format!("git apply 失败: {e}"))?;
    }
    let tree_bytes = run_git(&["write-tree"], None)
        .map_err(|e| format!("write-tree 失败: {e}"))?;
    let tree_sha = String::from_utf8_lossy(&tree_bytes).trim().to_string();
    if tree_sha.is_empty() {
        return Err("write-tree 返回空 SHA".into());
    }
    Ok(tree_sha)
}

/// Fail-closed reconstruction of an upstream run's read-only snapshot when its original
/// worktree has been cleaned, archived, or released from a pool slot.
fn reconstruct_in_snapshot(
    upstream: &Path,
    prior: &Value,
    fallback_workdir: &Path,
) -> Res<ReconstructedInSnapshot> {
    let run_id = {
        let r = s(prior, "run");
        if !r.is_empty() {
            r.to_string()
        } else {
            upstream
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }
    };
    let wt_path = s(&prior["worktree"], "path");

    let origin_repo = {
        let src = s(&prior["worktree"], "source");
        if !src.is_empty() && Path::new(src).is_dir() && git_top_fast(Path::new(src)).is_some() {
            git_top_fast(Path::new(src)).unwrap_or_else(|| PathBuf::from(src))
        } else if let Some(top) = git_top_fast(fallback_workdir) {
            top
        } else {
            return Err(fail_closed_in_err(
                &run_id,
                wt_path,
                "找不到源 git 仓库目录",
            ));
        }
    };
    let origin_git_dir = git_text(
        &origin_repo,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    )
    .map(PathBuf::from)
    .map_err(|e| fail_closed_in_err(&run_id, wt_path, &format!("源仓库 git-dir 不可读: {e}")))?;

    let archive_dir = resolve_upstream_archive_dir(upstream, prior);
    let changes_doc = find_upstream_record_file(upstream, archive_dir.as_deref(), "changes.json")
        .map(|p| json(&p))
        .unwrap_or(Value::Null);
    let applied_doc = find_upstream_record_file(upstream, archive_dir.as_deref(), ".applied")
        .map(|p| json(&p))
        .unwrap_or(Value::Null);
    let (patch_path, patch_label) =
        if let Some(p) = find_upstream_record_file(upstream, archive_dir.as_deref(), "applied.patch") {
            (Some(p), "applied.patch")
        } else if let Some(p) =
            find_upstream_record_file(upstream, archive_dir.as_deref(), "changes.patch")
        {
            (Some(p), "changes.patch")
        } else {
            (None, "")
        };

    let wt_base_commit =
        find_upstream_record_file(upstream, archive_dir.as_deref(), "worktree-base-commit")
            .map(|p| read(&p).trim().to_string())
            .unwrap_or_default();

    let raw_bases: Vec<(&str, String)> = vec![
        ("changes.base", s(&changes_doc, "base").to_string()),
        ("meta.base.tree", s(&prior["base"], "tree").to_string()),
        ("meta.chainBase", s(prior, "chainBase").to_string()),
        ("meta.netBase", s(prior, "netBase").to_string()),
        ("worktree-base-commit", wt_base_commit),
        ("meta.base.head", s(&prior["base"], "head").to_string()),
    ];
    let mut seen_bases = std::collections::HashSet::new();
    let mut valid_bases: Vec<(&str, String, String)> = Vec::new();
    let mut first_base_commit: Option<String> = None;
    for (label, sha) in raw_bases {
        if sha.is_empty() || !seen_bases.insert(sha.clone()) {
            continue;
        }
        if let Some(ty) = git_object_type(&origin_repo, &sha) {
            if matches!(ty.as_str(), "tree" | "commit") {
                if ty == "commit" && first_base_commit.is_none() {
                    first_base_commit = Some(sha.clone());
                }
                valid_bases.push((label, sha, ty));
            }
        }
    }

    let after_tree = {
        let c_after = s(&changes_doc, "after");
        let a_after = s(&applied_doc, "tree");
        let cand = if !c_after.is_empty() { c_after } else { a_after };
        if !cand.is_empty() && git_object_type(&origin_repo, cand).as_deref() == Some("tree") {
            Some(cand.to_string())
        } else {
            None
        }
    };

    let explicit_zero_changes = changes_doc.is_object()
        && changes_doc["changes"]
            .as_array()
            .is_some_and(Vec::is_empty);

    // 1. Primary reconstruction: apply archived `applied.patch` / `changes.patch` onto base commit/tree.
    if let Some(pp) = &patch_path {
        let patch_bytes = fs::read(pp)
            .map_err(|e| fail_closed_in_err(&run_id, wt_path, &format!("读取 {patch_label} 失败: {e}")))?;
        if valid_bases.is_empty() {
            if let Some(after) = after_tree {
                return Ok(ReconstructedInSnapshot {
                    origin_repo,
                    tree_sha: after.clone(),
                    base_commit: first_base_commit,
                    archive_dir,
                    summary: format!("归档 after tree ({})", &after[..after.len().min(7)]),
                });
            }
            return Err(fail_closed_in_err(
                &run_id,
                wt_path,
                &format!("存在 {patch_label} 但原基线提交/树对象在源仓库中已不可达"),
            ));
        }
        let mut last_err = String::new();
        for (base_label, base_sha, obj_type) in &valid_bases {
            match apply_patch_onto_base(&origin_git_dir, base_sha, &patch_bytes) {
                Ok(tree_sha) => {
                    let base_commit = if obj_type == "commit" {
                        Some(base_sha.clone())
                    } else {
                        first_base_commit.clone()
                    };
                    return Ok(ReconstructedInSnapshot {
                        origin_repo,
                        tree_sha,
                        base_commit,
                        archive_dir,
                        summary: format!(
                            "{patch_label} + {base_label} ({})",
                            &base_sha[..base_sha.len().min(7)]
                        ),
                    });
                }
                Err(e) => {
                    last_err = format!("{base_label}({}): {e}", &base_sha[..base_sha.len().min(7)]);
                }
            }
        }
        return Err(fail_closed_in_err(
            &run_id,
            wt_path,
            &format!("{patch_label} 无法应用到基线提交（{last_err}）"),
        ));
    }

    // 2. Zero-change run with an intact base object.
    if explicit_zero_changes {
        if let Some((base_label, base_sha, obj_type)) = valid_bases.first() {
            let tree_sha = if obj_type == "tree" {
                base_sha.clone()
            } else {
                git_text(&origin_repo, &["rev-parse", &format!("{base_sha}^{{tree}}")])
                    .map_err(|e| fail_closed_in_err(&run_id, wt_path, &format!("解析基线树失败: {e}")))?
            };
            return Ok(ReconstructedInSnapshot {
                origin_repo,
                tree_sha,
                base_commit: if obj_type == "commit" {
                    Some(base_sha.clone())
                } else {
                    first_base_commit
                },
                archive_dir,
                summary: format!("零改动基线 {base_label} ({})", &base_sha[..base_sha.len().min(7)]),
            });
        }
    }

    // 3. Fallback if patch file is absent but `after` tree object is still intact in origin_repo.
    if let Some(after) = after_tree {
        return Ok(ReconstructedInSnapshot {
            origin_repo,
            tree_sha: after.clone(),
            base_commit: first_base_commit,
            archive_dir,
            summary: format!("归档 after tree ({})", &after[..after.len().min(7)]),
        });
    }

    Err(fail_closed_in_err(
        &run_id,
        wt_path,
        "归档缺少 applied.patch / changes.patch，且基线或 after 树对象不可达",
    ))
}

fn launch_scout_for_write(
    write_o: &Options,
    raw_brief: &str,
    workdir: &Path,
    repo: &Path,
    config: &crate::config::Config,
) -> Res<PathBuf> {
    let mut scout_o = Options::new();
    scout_o.tier = Some("scout".into());
    scout_o.read_only = true;
    scout_o.workdir = Some(workdir.to_string_lossy().into_owned());
    scout_o.no_brief_check = true;
    scout_o.no_scout = true;
    scout_o.no_remote = write_o.no_remote;
    scout_o.share = write_o.share.clone();
    let base_name = write_o
        .name
        .clone()
        .unwrap_or_else(|| prompt_name(raw_brief));
    scout_o.name = Some(format!("探路-{}", clip(&base_name, 18)));
    choose_tier(&mut scout_o)?;
    missing_tools(&scout_o.agent)?;
    let tier_models = apply_tier_model(&mut scout_o, &config.value)?;
    let defaults = effective_defaults(&mut scout_o, config);
    let capacities = crate::config::capacities(&config.value)?;
    let cfg = worktree::work_config(&config.value, &repo.join(".delegate.json"))?;
    let mut extra = json!({
        "env": config.value["env"],
        "agentDeny": config.value["agentDeny"],
        "configSources": config.sources,
        "configCapacity": capacities,
        "tierModels": tier_models,
        "tierConfig": {
            "tiers": config.value["tiers"],
            "models": config.value["models"],
            "thinking": config.value["thinking"],
        },
        "sourceBuild": config.value["sourceBuild"],
        "sourceBuildRoot": repo,
        "defaults": defaults,
        "resultChars": config.value["resultChars"].as_u64().unwrap_or(20000),
        "cleanupKeepExecutables": config.value["cleanupKeepExecutables"],
        "share": scout_o.share,
        "acceptBlind": config.value["acceptBlind"],
        "controllerReviewPaths": config.value["controllerReviewPaths"],
        "riskRules": config.value["riskRules"],
        "resources": config.value["resources"],
        "measure": config.value["measure"],
        "measureCpus": config.value["measureCpus"],
        "stall": config.value["stall"],
        "checkIn": config.value["checkIn"],
        "agy": config.value["agy"],
        "worktree": {"source": repo, "sourceWorkdir": workdir, "config": cfg},
    });
    let remote_off =
        scout_o.no_remote || std::env::var("DELEGATE_REMOTE").ok().as_deref() == Some("0");
    extra["remoteOff"] = json!(remote_off);
    extra["remote"] = if remote_off {
        json!(false)
    } else {
        config.value["remote"].clone()
    };
    let (standing, sources) = standing_text(config, true, &extra["agentDeny"]);
    extra["standing"] = json!(standing);
    extra["standingSources"] = sources;
    let scout_prompt = crate::scout_first::build_scout_prompt(raw_brief);
    launch(scout_o, &scout_prompt, workdir, "read-only", &extra, None)
}

pub fn start(mut o: Options) -> Res<PathBuf> {
    if let Some(e) = nesting_error() {
        return Err(e);
    }
    let mut work_item: Option<crate::work::WorkItem> = None;
    if let Some(raw_w) = o.work.clone() {
        let item = crate::work::validate_work_id(&raw_w)?;
        o.work = Some(item.id.clone());
        work_item = Some(item);
    }
    choose_tier(&mut o)?;
    let prompt = read_prompt(&mut o)?;
    let in_run = o.in_run.as_deref().map(runs::resolve).transpose()?;
    if let Some(upstream) = &in_run {
        if active(&state(upstream)) {
            return Err("--in requires a finished run".into());
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
                in_run.as_ref().and_then(|r| {
                    let prior = json(r.join("meta.json"));
                    let sw = s(&prior["worktree"], "sourceWorkdir");
                    if !sw.is_empty() && Path::new(sw).is_dir() {
                        Some(sw.to_string())
                    } else {
                        let src = s(&prior["worktree"], "source");
                        (!src.is_empty() && Path::new(src).is_dir()).then(|| src.to_string())
                    }
                })
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
    let reconstructed_in = if let Some(upstream) = &in_run {
        let prior = json(upstream.join("meta.json"));
        if is_upstream_worktree_live(upstream, &prior) {
            None
        } else {
            let rec = reconstruct_in_snapshot(upstream, &prior, &workdir)?;
            let rid = {
                let r = s(&prior, "run");
                if !r.is_empty() {
                    r.to_string()
                } else {
                    upstream
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                }
            };
            eprintln!(
                "delegate: --in {rid} 的原 worktree 已清理，已从归档（{}）重建只读快照",
                rec.summary
            );
            Some(rec)
        }
    } else {
        None
    };
    missing_tools(&o.agent)?;
    if o.in_place && o.worktree {
        return Err("--in-place and --worktree contradict each other".into());
    }
    let repo = git_top_fast(&workdir);
    if o.shell && (repo.is_none() || o.in_place) {
        return Err("--shell requires a git worktree snapshot (not --in-place)".into());
    }
    for item in &mut o.share {
        let path = std::path::absolute(&*item).map_err(|e| e.to_string())?;
        if !path.exists() {
            return Err(format!("--share path does not exist: {}", path.display()));
        }
        let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
        if repo.as_ref().is_some_and(|top| {
            let top_canon = top.canonicalize().unwrap_or_else(|_| top.clone());
            path.starts_with(top) || canon.starts_with(&top_canon)
        }) && (worktree::has_node_modules(&path) || worktree::has_node_modules(&canon))
        {
            return Err(format!(
                "--share must not link node_modules inside the repository ({}); package managers follow the symlink and overwrite the source checkout",
                path.display()
            ));
        }
        *item = path.to_string_lossy().into_owned();
    }
    o.share.sort();
    o.share.dedup();
    let config = crate::config::load(repo.as_deref())?;
    let tier_models = apply_tier_model(&mut o, &config.value)?;
    let defaults = effective_defaults(&mut o, &config);
    let mut defaults = defaults;
    if let Some(value) = config.value["defaults"].get("keepCommits") {
        defaults["keepCommits"] = value.clone();
    }
    if !o.protect.is_empty() && repo.is_none() {
        return Err("--protect needs a git repository".into());
    }
    normalize_protect(&mut o.protect)?;
    validate_allow(&mut o.allow, repo.as_deref(), &o.protect)?;
    if !o.allow.is_empty() {
        eprintln!("delegate: open for this task / 本次开放：{}", o.allow.join(", "));
    }
    let prompt_protected = if o.read_only {
        Vec::new()
    } else {
        protected_paths_in_prompt(&prompt, repo.as_deref(), &o.protect, &o.allow)
    };
    note_prompt_protected_paths(&prompt_protected);
    if !o.no_brief_check && crate::config::brief_check_enabled(&config.value) {
        let check_root = repo.as_deref().unwrap_or(&workdir);
        crate::brief_check::run_and_report(&prompt, check_root);
    }
    let capacities = crate::config::capacities(&config.value)?;
    if !config.ignored.is_empty() {
        eprintln!(
            "delegate: {}: ignoring repository-only fields: {}",
            crate::config::user_path().display(),
            config.ignored.join(", ")
        );
    }
    let mut extra = json!({"env":config.value["env"],"agentDeny":config.value["agentDeny"],"configSources":config.sources,"configCapacity":capacities,"tierModels":tier_models,"tierConfig":{"tiers":config.value["tiers"],"models":config.value["models"],"thinking":config.value["thinking"]}});
    extra["sourceBuild"] = config.value["sourceBuild"].clone();
    extra["sourceBuildRoot"] = json!(repo);
    extra["defaults"] = defaults;
    extra["resultChars"] = json!(config.value["resultChars"].as_u64().unwrap_or(20000));
    extra["cleanupKeepExecutables"] = config.value["cleanupKeepExecutables"].clone();
    extra["share"] = json!(o.share);
    extra["acceptBlind"] = config.value["acceptBlind"].clone();
    extra["controllerReviewPaths"] = config.value["controllerReviewPaths"].clone();
    extra["riskRules"] = config.value["riskRules"].clone();
    if !o.read_only && !o.accept_set && config.value["acceptRules"].is_array() {
        extra["acceptRules"] = config.value["acceptRules"].clone();
    }
    if !o.read_only && !o.accept_also.is_empty() {
        extra["acceptAlso"] = json!(o.accept_also);
    }
    if let Some(class) = &o.class {
        extra["class"] = json!(class);
    }
    if !o.read_only {
        if let Some(rc) = crate::review::resolve_launch_config(
            &config.value,
            repo.as_deref(),
            o.review_model.as_deref(),
            config.origins["reviewModel"].as_str(),
        )? {
            extra["reviewConfig"] = rc;
        }
    }
    let remote_off =
        o.no_remote || std::env::var("DELEGATE_REMOTE").ok().as_deref() == Some("0");
    extra["remoteOff"] = json!(remote_off);
    extra["remote"] = if remote_off {
        json!(false)
    } else {
        config.value["remote"].clone()
    };
    extra["resources"] = config.value["resources"].clone();
    extra["measure"] = config.value["measure"].clone();
    extra["measureCpus"] = config.value["measureCpus"].clone();
    extra["stall"] = config.value["stall"].clone();
    extra["checkIn"] = config.value["checkIn"].clone();
    extra["agy"] = config.value["agy"].clone();
    let (standing, sources) = standing_text(&config, o.read_only, &extra["agentDeny"]);
    extra["standing"] = json!(standing);
    extra["standingSources"] = sources;
    if !o.read_only {
        let default_accept = config.value["accept"].as_str().filter(|c| !c.is_empty());
        if !o.accept_set {
            o.accept = config.value["accept"].as_str().map(str::to_string);
        } else if let Some(default) = default_accept {
            if o.accept
                .as_deref()
                .is_some_and(|command| command != default)
            {
                eprintln!("delegate: --accept replaces default accept {default:?}; use --accept-also COMMAND to keep it and append task tests / --accept 替换了默认验收；任务测试用 --accept-also 追加并保留默认验收");
            }
        }
    }
    o.append_accept();
    let deny_accept_conflicts =
        crate::deny::conflicts(o.accept.as_deref(), &prompt, &extra["agentDeny"]);
    for command in &deny_accept_conflicts {
        eprintln!("验收命令 {command} 会被 agentDeny 拦下，同事无法运行；改为主控合入后自己跑，或换成单包命令");
    }
    extra["denyAcceptConflicts"] = json!(deny_accept_conflicts);
    if !o.read_only && !o.evidence_set && o.evidence.is_none() {
        o.evidence = config.value["evidence"].as_str().map(str::to_string);
    }
    if let Some(top) = &repo {
        let cfg = worktree::work_config(&config.value, &top.join(".delegate.json"))?;
        if !o.read_only && !o.protect.is_empty() {
            let (paths, command) = worktree::generated(top)?;
            let overlap = paths
                .iter()
                .filter(|path| {
                    !worktree::matches_rule(path, &o.allow) && o.protect.iter().any(|rule| {
                        worktree::matches_rule(
                            path.trim_end_matches('/'),
                            std::slice::from_ref(rule),
                        ) || worktree::matches_rule(
                            rule.trim_end_matches('/'),
                            std::slice::from_ref(*path),
                        ) || rule == *path
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
            if !overlap.is_empty() && !command.trim().is_empty() {
                eprintln!("delegate: protected generated paths {} may only be changed by generated.command", overlap.join(", "));
                extra["protectGenerated"] = json!({"paths":overlap,"command":command,"inputs":worktree::generated_inputs(top)?});
            }
        }
        if let Some(upstream) = &in_run {
            let prior = json(upstream.join("meta.json"));
            if let Some(rec) = &reconstructed_in {
                let mut wt = json!({
                    "source": rec.origin_repo,
                    "sourceWorkdir": workdir,
                    "config": cfg,
                    "in": upstream,
                    "reconstructedTree": rec.tree_sha,
                    "reconstructedSummary": rec.summary,
                });
                if let Some(base_commit) = &rec.base_commit {
                    wt["reconstructedBase"] = json!(base_commit);
                }
                if let Some(arch) = &rec.archive_dir {
                    wt["reconstructedArchive"] = json!(arch);
                }
                extra["worktree"] = wt;
            } else {
                extra["worktree"] = json!({
                    "source": s(&prior["worktree"], "path"),
                    "sourceWorkdir": s(&prior, "workdir"),
                    "config": cfg,
                    "in": upstream,
                });
            }
        } else if o.worktree || (o.read_only && !o.in_place) {
            extra["worktree"] = json!({"source":top,"sourceWorkdir":workdir,"config":cfg});
        } else if !o.read_only && !o.in_place {
            // In place, this run would edit the tree other runs are applied into.
            let busy = worktree::active_writes_on(&top.to_string_lossy(), None);
            if !busy.is_empty() {
                let names = busy
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>();
                eprintln!(
                    "delegate: write task {} running on this source; this run uses a worktree so the source stays free for apply / 同源已有写入任务，本任务改用 worktree",
                    names.join(", ")
                );
                o.worktree = true;
                extra["worktree"] = json!({"source":top,"sourceWorkdir":workdir,"config":cfg});
            }
        }
    }
    if o.worktree && repo.is_none() {
        return Err(format!(
            "--worktree needs a git repository: {}",
            workdir.display()
        ));
    }
    let mode = if o.read_only { "read-only" } else { "write" };
    if let Some(top) = &repo {
        if let Some(cand) = match_area_for_start(
            &config.value["areas"],
            top,
            &workdir,
            &prompt,
            work_item.as_ref(),
        ) {
            extra["areaCandidate"] = cand;
        }
    }
    if !prompt_protected.is_empty() {
        extra["promptProtectedPaths"] = json!(prompt_protected);
    }
    let mut scout_echo: Option<String> = None;
    if !o.read_only && o.worktree {
        if o.no_scout {
            scout_echo = Some("探路：跳过（--no-scout）".to_string());
        } else if !crate::config::scout_first_enabled(&config.value) {
            scout_echo = Some("探路：跳过（scoutFirst: false）".to_string());
        } else if let Some(top) = repo.as_deref() {
            match launch_scout_for_write(&o, &prompt, &workdir, top, &config) {
                Ok(scout_dir) => {
                    let scout_id = scout_dir
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    extra["scoutFirst"] = json!(true);
                    extra["scoutRun"] = json!(&scout_id);
                    extra["scoutRunDir"] = json!(&scout_dir);
                    extra["rawBrief"] = json!(&prompt);
                    scout_echo = Some(format!("探路：先探路（{scout_id}）"));
                }
                Err(err) => {
                    eprintln!("delegate: 探路启动失败（{err}），跳过探路直接派写手");
                    extra["scoutLaunchError"] = json!(err);
                    scout_echo = Some("探路：跳过（探路启动失败）".to_string());
                }
            }
        }
    }
    let run = launch(o, &prompt, &workdir, mode, &extra, None)?;
    if let Some(line) = scout_echo {
        eprintln!("{line}");
    }
    Ok(run)
}
/// Lines a `--minor` round may change before it counts as a rework after all.
const MINOR_LINES: u64 = 60;
/// Characters a `--minor` instruction may have.
const MINOR_CHARS: usize = 600;

/// Rework rounds already spent in the write conversation ending at `run`: every
/// rework reply that changed files after a round that had delivered changes, plus
/// minor replies that grew past [`MINOR_LINES`]. Answering a question, or a first
/// build after a round that changed nothing, is not rework. Also returns the
/// conversation's name (its first run's).
fn rework_used(run: &Path) -> (u64, String) {
    let mut used = 0;
    let mut current = run.to_path_buf();
    loop {
        let meta = json(current.join("meta.json"));
        let sum = json(current.join("summary.json"));
        let changes = &sum["changes"];
        let lines =
            changes["added"].as_u64().unwrap_or(0) + changes["deleted"].as_u64().unwrap_or(0);
        match (s(&meta, "continuation"), s(&meta["rework"], "kind")) {
            _ if b(&sum, "infrastructureFailure")
                || s(&sum, "state") == "blocked_mainline"
                || runs::state(&current) == "blocked_mainline" => {}
            (continuation, _)
                if runs::interrupted(continuation) || continuation == "blocked_mainline" => {}
            // A round that changed nothing (a question, a failed start) is not rework,
            // and neither is the round after one: that is the first build.
            (_, "rework")
                if changes["files"].as_u64().unwrap_or(0) > 0 && delivered(&meta, &current) =>
            {
                used += 1
            }
            (_, "minor") if lines > MINOR_LINES => used += 1,
            _ => {}
        }
        let parent = s(&meta, "parent");
        if parent.is_empty() {
            return (used, s(&meta, "name").to_string());
        }
        current = current.with_file_name(parent);
    }
}

pub fn rework_count(run: &Path) -> u64 {
    let meta = json(run.join("meta.json"));
    let walked = rework_used(run).0;
    let recorded = meta["rework"]["used"].as_u64().unwrap_or(0);
    walked.max(recorded)
}

/// Whether the round this reply answered had delivered work: it changed files
/// and did not stop to ask.
fn delivered(meta: &Value, run: &Path) -> bool {
    let parent = s(meta, "parent");
    if parent.is_empty() {
        return false;
    }
    let parent_dir = run.with_file_name(parent);
    let sum = json(parent_dir.join("summary.json"));
    sum["changes"]["files"].as_u64().unwrap_or(0) > 0
        && !b(&sum, "question")
        && !b(&sum, "infrastructureFailure")
        && s(&sum, "state") != "blocked_mainline"
        && runs::state(&parent_dir) != "blocked_mainline"
}

/// Rework budget for write conversations (`.delegate.json` `maxRework`, default 1):
/// past it, the caller finishes the work; `--minor` covers short fixes and
/// `--over-limit REASON` is an explicit, recorded exception.
fn rework_gate(
    parent: &Path,
    meta: &Value,
    o: &Options,
    prompt: &str,
    config: &Value,
) -> Res<Option<Value>> {
    if s(meta, "mode") != "write" {
        if o.minor || o.over_limit.is_some() {
            return Err("--minor and --over-limit apply to write conversations".into());
        }
        return Ok(None);
    }
    let limit = match config.get("maxRework") {
        Some(Value::Null) => None,
        Some(value) => value.as_u64(),
        None => Some(1),
    };
    let (used, name) = rework_used(parent);
    let parent_state = runs::state(parent);
    let parent_sum = json(parent.join("summary.json"));
    if runs::interrupted(&parent_state)
        || parent_state == "blocked_mainline"
        || s(&parent_sum, "state") == "blocked_mainline"
        || b(&parent_sum, "infrastructureFailure")
    {
        return Ok(Some(
            json!({"kind":"continuation","used":used,"limit":limit}),
        ));
    }
    if o.minor {
        let chars = prompt.chars().count();
        if chars > MINOR_CHARS {
            return Err(format!(
                "--minor is for short fixes: the instruction has {chars} characters (limit {MINOR_CHARS}); drop --minor to spend a rework round"
            ));
        }
        return Ok(Some(json!({"kind":"minor","used":used,"limit":limit})));
    }
    if b(&parent_sum, "question") || parent_sum["changes"]["files"].as_u64().unwrap_or(0) == 0 {
        // Answering a question or starting the build after a round with no changes.
        return Ok(Some(json!({"kind":"answer","used":used,"limit":limit})));
    }
    if let (Some(limit), None) = (limit, &o.over_limit) {
        if used >= limit {
            return Err(format!(
                "rework limit reached ({used}/{limit}) for {name}: finish it yourself (diff {name} --total, apply {name}, then fix in your tree), use reply --minor for a fix under {MINOR_CHARS} characters, or --over-limit '<reason>' to record an exception\n返工次数已用完（{used}/{limit}）：请主控接手（diff/apply 后自己改）；短修正用 --minor（说明不超过 {MINOR_CHARS} 字，改动超过 {MINOR_LINES} 行会补记一次返工）"
            ));
        }
    }
    let mut record = json!({"kind":"rework","used":used + 1,"limit":limit});
    if let Some(reason) = &o.over_limit {
        record["overLimit"] = json!(reason);
    }
    Ok(Some(record))
}

pub fn reply(mut o: Options) -> Res<PathBuf> {
    let timeout_overridden = o.timeout.is_some();
    let evidence_overridden = o.evidence_set;
    let accept_overridden = o.accept_set;
    let accept_timeout_overridden = o.accept_timeout.is_some();
    let parent = runs::latest(runs::resolve(o.run.as_deref().unwrap_or("last"))?);
    let meta = json(parent.join("meta.json"));
    if o.work.is_none() {
        let parent_work = s(&meta, "workId");
        if !parent_work.is_empty() {
            o.work = Some(parent_work.to_string());
        }
    } else if let Some(raw_w) = o.work.clone() {
        let item = crate::work::validate_work_id(&raw_w)?;
        o.work = Some(item.id);
    }
    o.share = strings(&meta["share"]);
    if active(&state(&parent)) {
        return Err(format!(
            "{} is still running; append directly with `delegate reply` or wait before launching a new round",
            parent.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    let summary = json(parent.join("summary.json"));
    let after_apply = parent.join(".applied-cleaned").exists()
        || (crate::archive::is_record(&parent) && parent.join(".applied").exists());
    let scout_question = b(&meta, "scoutQuestion");
    let compose_after_apply = after_apply && !o.fresh;
    if after_apply {
        o.fresh = true;
        eprintln!("delegate: 原会话已合入，已开新会话续做");
    } else if scout_question {
        o.fresh = true;
    }
    let parent_name = crate::notes::run_id(&parent, &meta);
    let existing_child = all_runs().into_iter().find(|run| {
        s(&json(run.join("meta.json")), "parent") == parent_name
            && runs::continues_conversation(run)
    });
    if o.sync {
        if let Some(existing) = &existing_child {
            return Err(format!(
                "{parent_name} already has a reply ({}); wait for it and reply to that",
                existing.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
    }
    let requested_tier = o.tier.clone();
    let requested_model = o.model.clone();
    let requested_thinking = o.thinking_cli.clone();
    o.read_only = s(&meta, "mode") == "read-only";
    o.shell |= b(&meta, "shell");
    if o.shell && (!o.read_only || !meta["worktree"].is_object()) {
        return Err("--shell requires a read-only worktree snapshot".into());
    }
    if !o.read_only
        && requested_tier
            .as_deref()
            .is_some_and(|t| ["scout", "review"].contains(&t))
    {
        return Err(format!("{READS_ONLY}; this conversation writes"));
    }
    let legacy = ![agents::AGENT, crate::agy::BIN].contains(&s(&meta, "agent"));
    if legacy && !o.fresh {
        o.fresh = true;
        eprintln!(
            "delegate: {} 的会话无法由 Pi 或 agy 续接，已开新会话",
            s(&meta, "agent")
        );
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
            || agents::continuation_target(&meta, session.as_deref().unwrap_or("")).is_none())
    {
        return Err(format!("{} has no saved session to continue (runs before delegate 4.1 kept none); use --fresh to start a new session in the same place",parent.file_name().unwrap_or_default().to_string_lossy()));
    }
    if !after_apply
        && !scout_question
        && meta["worktree"].is_object()
        && !Path::new(s(&meta["worktree"], "path")).exists()
    {
        return Err(format!(
            "the worktree of {} no longer exists: {}",
            parent.file_name().unwrap_or_default().to_string_lossy(),
            s(&meta["worktree"], "path")
        ));
    }
    if let Some(e) = nesting_error() {
        return Err(e);
    }
    o.agent = agents::AGENT.into();
    missing_tools(&o.agent)?;
    let switching = requested_tier.is_some() || requested_model.is_some();
    o.agent_pinned = requested_model.is_some() || (!switching && runs::agent_pinned(&meta));
    if requested_model.is_none() {
        o.tier = requested_tier
            .clone()
            .or_else(|| meta["tier"].as_str().map(str::to_string));
    }
    if !switching && !legacy {
        o.provider = meta["provider"].as_str().map(str::to_string);
        o.model = meta["model"].as_str().map(str::to_string);
        o.model_name = meta["modelName"].as_str().map(str::to_string);
        o.thinking = meta["thinking"].as_str().map(str::to_string);
        o.tier_inferred = b(&meta, "tierInferred");
    }
    o.retries = n(&meta, "retries");
    let mut added_allow = std::mem::take(&mut o.allow);
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
    o.protect_reasons = meta["protectReasons"]
        .as_object()
        .map(|reasons| {
            reasons
                .iter()
                .filter_map(|(path, reason)| {
                    reason
                        .as_str()
                        .map(|reason| (path.clone(), reason.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let allow_top = Some(s(&meta["worktree"], "source"))
        .filter(|x| !x.is_empty())
        .or_else(|| Some(s(&meta, "top")).filter(|x| !x.is_empty()))
        .map(Path::new);
    validate_allow(&mut added_allow, allow_top, &o.protect)?;
    let mut merged_allow = strings(&meta["allow"]);
    merged_allow.extend(added_allow.iter().cloned());
    merged_allow.sort();
    merged_allow.dedup();
    o.allow = merged_allow;
    if !added_allow.is_empty() {
        eprintln!("delegate: open for this task / 本次开放：{}", o.allow.join(", "));
    }
    if o.timeout.is_none() && !s(&meta, "timeout").is_empty() {
        o.timeout = Some(s(&meta, "timeout").into());
    }
    if !o.accept_set {
        o.accept = meta["accept"].as_str().map(str::to_string);
    }
    o.append_accept();
    if o.accept.as_deref() == Some("") {
        o.accept = None;
    }
    if o.accept_timeout.is_none() {
        o.accept_timeout = meta["acceptTimeoutSeconds"]
            .as_f64()
            .filter(|seconds| *seconds > 0.0)
            .map(|seconds| format!("{seconds}s"));
    }
    if !o.evidence_set {
        o.evidence = meta["evidenceCommand"].as_str().map(str::to_string);
    }
    if o.evidence_timeout.is_none() {
        o.evidence_timeout = meta["evidenceTimeoutSeconds"]
            .as_f64()
            .map(|seconds| format!("{seconds}s"));
    }
    if o.max_answer.is_none() {
        o.max_answer = meta["maxAnswer"].as_u64();
    }
    let raw_prompt = read_prompt(&mut o)?;
    let prompt_protected = if o.read_only {
        Vec::new()
    } else {
        protected_paths_in_prompt(&raw_prompt, allow_top, &o.protect, &o.allow)
    };
    note_prompt_protected_paths(&prompt_protected);
    let prompt = if compose_after_apply {
        format!(
            "原任务说明：\n{}\n\n上次答复：\n{}\n\n这次的追加说明：\n{}",
            read(parent.join("prompt.md")).trim(),
            read(parent.join("result.md")).trim(),
            raw_prompt
        )
    } else {
        raw_prompt.clone()
    };
    let top = Some(s(&meta["worktree"], "source"))
        .filter(|x| !x.is_empty())
        .unwrap_or(s(&meta, "top"));
    let config = crate::config::load(Some(Path::new(top)))?;
    if !o.no_brief_check
        && !o.brief_checked
        && crate::config::brief_check_enabled(&config.value)
    {
        let check_root = [
            s(&meta["worktree"], "source"),
            s(&meta, "top"),
            s(&meta, "workdir"),
        ]
        .into_iter()
        .find(|p| !p.is_empty() && Path::new(p).is_dir())
        .map(Path::new);
        if let Some(root) = check_root {
            crate::brief_check::run_and_report(&raw_prompt, root);
        }
    }
    let tier_models = if switching || legacy {
        if requested_model.is_none() {
            o.model = None;
        }
        choose_tier(&mut o)?;
        apply_tier_model(&mut o, &config.value)?
    } else {
        let listed = if s(&meta, "agentBin").is_empty() {
            agents::pi_models()
        } else {
            agents::pi_models_for(Path::new(s(&meta, "agentBin")))
        };
        require_models(&config.value, &listed)?
    };
    if !switching {
        // Ordinary reply keeps the captured level; only env/CLI/--class may override it.
        let env = setting("THINKING", "");
        if !env.is_empty() {
            models::validate_thinking(&env)?;
            o.thinking = Some(env);
        } else if requested_thinking.is_some() {
            o.thinking = requested_thinking;
        } else if let Some(level) = o.class.as_deref().and_then(crate::fit::default_thinking) {
            o.thinking = Some(level.into());
        }
    }
    let deny_accept_conflicts =
        crate::deny::conflicts(o.accept.as_deref(), &prompt, &meta["agentDeny"]);
    for command in &deny_accept_conflicts {
        eprintln!("验收命令 {command} 会被 agentDeny 拦下，同事无法运行；改为主控合入后自己跑，或换成单包命令");
    }
    let capacities = crate::config::capacities(&config.value)?;
    o.rework = if after_apply {
        None
    } else {
        rework_gate(&parent, &meta, &o, &prompt, &config.value)?
    };
    if o.sync && !meta["worktree"].is_object() {
        return Err("--sync requires a worktree conversation".into());
    }
    let sync_mode = if o.sync_agent {
        Some(worktree::SyncMode::Agent)
    } else if o.sync {
        Some(worktree::SyncMode::Strict)
    } else if !o.no_sync
        && !after_apply
        && existing_child.is_none()
        && meta["worktree"].is_object()
        && s(&meta, "mode") == "write"
        && Path::new(s(&meta["worktree"], "path")).is_dir()
    {
        Some(worktree::SyncMode::Auto)
    } else {
        None
    };
    // Explicit `--sync` (`Strict`) and `--sync=agent` (`Agent`) run synchronously in foreground
    // because callers rely on immediate conflict / non-text conflict rejection; default `Auto`
    // sync moves to `_supervise` so interactive `reply` returns in <50ms.
    let (synced, deferred_sync_mode) = match sync_mode {
        Some(mode @ (worktree::SyncMode::Strict | worktree::SyncMode::Agent)) => (
            Some((mode, worktree::sync_with_mode(&parent, &meta, mode)?)),
            None,
        ),
        Some(worktree::SyncMode::Auto) => (None, Some(worktree::SyncMode::Auto)),
        None => (None, None),
    };
    let mut prompt = prompt;
    if let Some((mode, report)) = &synced {
        let (notices, updated_prompt) = worktree::format_sync_feedback(*mode, report, &prompt);
        for notice in notices {
            eprintln!("{notice}");
        }
        prompt = updated_prompt;
    }
    // A conversation continues only on the colleague that holds it (Pi or agy).
    let session = match session {
        Some(_)
            if crate::agy::colleague(o.model.as_deref().unwrap_or(""))
                != crate::agy::colleague(s(&meta, "model")) =>
        {
            eprintln!("delegate: 换到另一个同事（Pi / agy），会话无法续接，已开新会话");
            None
        }
        other => other,
    };
    let mut extra =
        json!({"parent":meta,"session":session,"env":meta["env"],"worktree":meta["worktree"]});
    if let Some(dmode) = deferred_sync_mode {
        extra["pendingSync"] = json!({
            "mode": match dmode {
                worktree::SyncMode::Auto => "auto",
                worktree::SyncMode::Agent => "agent",
                worktree::SyncMode::Strict => "strict",
            },
            "parentDir": parent.to_string_lossy(),
        });
    }
    extra["denyAcceptConflicts"] = json!(deny_accept_conflicts);
    if after_apply {
        extra["newAfterApply"] = json!(true);
        if meta["worktree"].is_object() {
            extra["worktree"] = json!({"source":top,"sourceWorkdir":s(&meta["worktree"],"sourceWorkdir"),
                "config":worktree::work_config(&config.value, &Path::new(top).join(".delegate.json"))?});
        }
    } else if scout_question {
        if meta["worktree"].is_object() {
            extra["worktree"] = json!({
                "source": top,
                "sourceWorkdir": s(&meta["worktree"], "sourceWorkdir"),
                "config": worktree::work_config(&config.value, &Path::new(top).join(".delegate.json"))?,
            });
        }
        let raw_brief = {
            let p = parent.join("raw-brief.md");
            if p.is_file() {
                read(&p)
            } else {
                s(&meta, "rawBrief").to_string()
            }
        };
        let scout_note = {
            let n = s(&meta, "scoutNote");
            if !n.is_empty() {
                n
            } else {
                s(&meta, "scoutL0")
            }
        };
        extra["scoutReplyCompose"] = json!({
            "rawBrief": raw_brief,
            "note": scout_note,
            "controllerReply": raw_prompt,
            "sourceMap": parent.join("scout-map.md"),
        });
        extra["scoutReply"] = json!(true);
        if meta["scout"].is_object() {
            extra["scoutCarried"] = meta["scout"].clone();
        }
        if !s(&meta, "scoutRun").is_empty() {
            extra["scoutRun"] = meta["scoutRun"].clone();
        }
    }
    extra["protectGenerated"] = meta["protectGenerated"].clone();
    if !added_allow.is_empty() && extra["protectGenerated"].is_object() {
        let kept: Vec<String> = strings(&extra["protectGenerated"]["paths"])
            .into_iter()
            .filter(|path| !worktree::matches_rule(path, &o.allow))
            .collect();
        if kept.is_empty() {
            extra.as_object_mut().unwrap().remove("protectGenerated");
        } else {
            extra["protectGenerated"]["paths"] = json!(kept);
        }
    }
    if !prompt_protected.is_empty() {
        extra["promptProtectedPaths"] = json!(prompt_protected);
    }
    extra["agentDeny"] = meta["agentDeny"].clone();
    extra["defaults"] = meta["defaults"].clone();
    if let Some(defaults) = extra["defaults"].as_object_mut() {
        if timeout_overridden {
            defaults.remove("timeout");
        }
        if evidence_overridden {
            defaults.remove("evidence");
        }
        if accept_overridden {
            defaults.remove("acceptAlso");
        }
        if accept_timeout_overridden {
            defaults.remove("acceptTimeout");
        }
    }
    extra["resultChars"] = json!(config.value["resultChars"].as_u64().unwrap_or(20000));
    extra["cleanupKeepExecutables"] = config.value["cleanupKeepExecutables"].clone();
    extra["share"] = meta["share"].clone();
    extra["acceptBlind"] = meta["acceptBlind"].clone();
    extra["controllerReviewPaths"] = if config.value["controllerReviewPaths"].is_array() {
        config.value["controllerReviewPaths"].clone()
    } else {
        meta["controllerReviewPaths"].clone()
    };
    extra["riskRules"] = if !config.value["riskRules"].is_null() {
        config.value["riskRules"].clone()
    } else {
        meta["riskRules"].clone()
    };
    if !o.read_only && !accept_overridden {
        extra["acceptRules"] = if config.value["acceptRules"].is_array() {
            config.value["acceptRules"].clone()
        } else {
            meta["acceptRules"].clone()
        };
        if !o.accept_also.is_empty() {
            extra["acceptAlso"] = json!(o.accept_also);
        } else if meta["acceptAlso"].is_array() {
            extra["acceptAlso"] = meta["acceptAlso"].clone();
        }
    }
    if !o.read_only {
        let inherited_cli_review_model = if o.review_model.is_none()
            && s(&meta["reviewConfig"], "modelSource") == "cli"
        {
            let prev = s(&meta["reviewConfig"], "modelName");
            (!prev.is_empty()).then_some(prev)
        } else {
            None
        };
        let cli_rev_model = o.review_model.as_deref().or(inherited_cli_review_model);
        if let Some(rc) = crate::review::resolve_launch_config(
            &config.value,
            (!top.is_empty()).then_some(Path::new(top)),
            cli_rev_model,
            config.origins["reviewModel"].as_str(),
        )? {
            extra["reviewConfig"] = rc;
        } else if meta["reviewConfig"].is_object() {
            extra["reviewConfig"] = meta["reviewConfig"].clone();
        }
    }
    let remote_off = if o.no_remote {
        true
    } else {
        match std::env::var("DELEGATE_REMOTE") {
            Ok(value) => value == "0",
            Err(_) => b(&meta, "remoteOff"),
        }
    };
    extra["remoteOff"] = json!(remote_off);
    extra["remote"] = if remote_off {
        json!(false)
    } else {
        config.value["remote"].clone()
    };
    extra["lanePriority"] = json(parent.join("lane-priority.json"));
    extra["resources"] = meta["resources"].clone();
    extra["measure"] = meta["measure"].clone();
    extra["measureCpus"] = meta["measureCpus"].clone();
    extra["stall"] = meta["stall"].clone();
    extra["checkIn"] = meta["checkIn"].clone();
    extra["agy"] = if !o.fresh && meta["agy"].is_object() {
        meta["agy"].clone()
    } else {
        config.value["agy"].clone()
    };
    if !o.fresh && session.is_some() && crate::agy::is_agy(o.model.as_deref().unwrap_or("")) {
        let pinned = s(&meta, "agyAccount");
        let inherited = if !pinned.is_empty() {
            pinned
        } else {
            s(&summary, "agyAccount")
        };
        if !inherited.is_empty() {
            extra["agyAccount"] = json!(inherited);
        }
    }
    if o.fresh {
        extra["agentDeny"] = config.value["agentDeny"].clone();
        let (text, sources) = standing_text(
            &config,
            s(&meta, "mode") == "read-only",
            &extra["agentDeny"],
        );
        extra["standing"] = json!(text);
        extra["standingSources"] = sources;
    } else {
        extra["standing"] = meta["standing"].clone();
        extra["standingSources"] = meta["standingSources"].clone();
        extra["standingInherited"] = json!(true);
    }
    let sources = ["user", "repo"]
        .into_iter()
        .filter(|source| {
            config.sources.contains(source)
                || meta["configSources"]
                    .as_array()
                    .is_some_and(|sources| sources.iter().any(|s| s.as_str() == Some(source)))
        })
        .collect::<Vec<_>>();
    extra["configSources"] = json!(sources);
    extra["configCapacity"] = capacities;
    extra["tierModels"] = tier_models;
    extra["tierConfig"] = json!({"tiers": config.value["tiers"],"models":config.value["models"],"thinking":config.value["thinking"]});
    extra["tierCandidates"] = meta["tierCandidates"].clone();
    extra["tierCandidateIndex"] = meta["tierCandidateIndex"].clone();
    let thinking_env = setting("THINKING", "");
    let override_level = if thinking_env.is_empty() {
        o.thinking_cli.as_deref()
    } else {
        Some(thinking_env.as_str())
    };
    if let (Some(level), Some(candidates)) =
        (override_level, extra["tierCandidates"].as_array_mut())
    {
        for candidate in candidates {
            candidate["thinking"] = json!(level);
        }
    }
    extra["sourceBuild"] = config.value["sourceBuild"].clone();
    extra["sourceBuildRoot"] = meta.get("sourceBuildRoot").cloned().unwrap_or(json!(top));
    let parent_state = runs::state(&parent);
    if runs::interrupted(&parent_state) && s(&meta, "mode") == "write" {
        extra["continuation"] = json!(parent_state);
    }
    if let Some((mode, report)) = synced {
        if report.synced {
            extra["syncedBase"] = json!({
                "tree": report.base_tree,
                "head": report.base_head,
                "commit": report.base_commit,
                "dirty": report.source_dirty,
            });
            let mut sync_doc = json!({
                "files": report.paths,
                "conflicts": report.conflicts,
                "agent": o.sync_agent,
                "auto": mode == worktree::SyncMode::Auto,
            });
            if !report.gitlink_mainline.is_empty() {
                sync_doc["gitlinkMainline"] = json!(report.gitlink_mainline);
            }
            if !report.regenerate_paths.is_empty() {
                sync_doc["regeneratePaths"] = json!(report.regenerate_paths);
                if !report.generate_command.is_empty() {
                    sync_doc["generateCommand"] = json!(report.generate_command);
                }
            }
            extra["sync"] = sync_doc;
        } else if mode != worktree::SyncMode::Auto {
            extra["sync"] = json!({
                "files": report.paths,
                "conflicts": report.conflicts,
                "agent": o.sync_agent,
            });
        } else {
            let previous = json(parent.join("sync.json"));
            if b(&previous, "agent") {
                extra["sync"] = previous;
            }
        }
    } else {
        let previous = json(parent.join("sync.json"));
        if b(&previous, "agent") {
            extra["sync"] = previous;
        }
    }
    if extra["worktree"].is_null() {
        extra.as_object_mut().unwrap().remove("worktree");
    }
    let child = launch(
        o,
        &prompt,
        Path::new(s(&meta, "workdir")),
        s(&meta, "mode"),
        &extra,
        Some(&parent),
    )?;
    if !after_apply && parent.join("sign.json").is_file() {
        let _ = fs::copy(parent.join("sign.json"), child.join("sign.json"));
    }
    if parent.join("meta.json").is_file() && !crate::archive::is_record(&parent) {
        let mut parent_meta = json(parent.join("meta.json"));
        parent_meta["repliedAt"] = json!(iso());
        let _ = write_json(parent.join("meta.json"), &parent_meta);
    }
    let rc = rework_count(&child);
    crate::history::record_reply(&meta, &child, rc);
    crate::board::refresh_for_run(&child);
    Ok(child)
}
pub fn active_machine(slots: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(e) = fs::read_dir(slots) {
        for e in e.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "slot") {
                let run = PathBuf::from(read(&p).trim());
                if run.join("meta.json").is_file() && (active(&state(&run)) || agent_alive(&run)) {
                    if !run.join(".waiting").exists() && !run.join(".scouting").exists() {
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
/// Read-only listing of machine-active runs from `.slot` files.
/// Never blocks on `.start.lock` so read-only callers (`status`, `work`, TUI) cannot deadlock
/// with concurrent `start` calls.
pub fn machine_runs(slots: &Path) -> Vec<PathBuf> {
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
pub fn repo_key(path: &Path) -> String {
    let common = git_common_dir_fast(path).unwrap_or_else(|| path.to_path_buf());
    fs::canonicalize(&common)
        .or_else(|_| std::path::absolute(&common))
        .unwrap_or(common)
        .to_string_lossy()
        .into_owned()
}
pub fn repo_key_for_meta(meta: &Value) -> String {
    if let Some(key) = meta["repoKey"].as_str().filter(|key| !key.is_empty()) {
        return key.to_string();
    }
    let source = s(&meta["worktree"], "source");
    repo_key(Path::new(if source.is_empty() {
        s(meta, "workdir")
    } else {
        source
    }))
}
/// Invalid settings are errors; exhausted capacity is a queue reason.
pub fn capacity(active_runs: &[PathBuf], config: &Value, repo: &str) -> Res<Option<String>> {
    let total = crate::config::capacity(config, "maxActive", "MAX_ACTIVE", 32)?;
    let repo_total = crate::config::capacity(config, "repoMaxActive", "REPO_MAX_ACTIVE", 0)?;
    let floor_setting = setting("MIN_AVAILABLE_MB", "4096");
    if floor_setting.is_empty() || !floor_setting.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("DELEGATE_MIN_AVAILABLE_MB must be a non-negative integer (0 = no check), got {floor_setting:?}"));
    }
    let floor = floor_setting.parse::<u64>().map_err(|e| e.to_string())?;
    let repo_count = active_runs
        .iter()
        .filter(|run| repo_key_for_meta(&json(run.join("meta.json"))) == repo)
        .count();
    if total > 0 && active_runs.len() >= total as usize {
        return Ok(Some(format!(
            "总路数 / machine capacity (DELEGATE_MAX_ACTIVE={total})"
        )));
    }
    if repo_total > 0 && repo_count >= repo_total as usize {
        return Ok(Some(format!(
            "仓库路数 / repository capacity (DELEGATE_REPO_MAX_ACTIVE={repo_total})"
        )));
    }
    let available = read("/proc/meminfo")
        .lines()
        .find_map(|line| {
            line.strip_prefix("MemAvailable:")?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        })
        .map(|kb| kb / 1024);
    if floor > 0 && available.is_some_and(|mb| mb < floor) {
        return Ok(Some(format!(
            "可用内存 / available memory (DELEGATE_MIN_AVAILABLE_MB={floor})"
        )));
    }
    Ok(None)
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
/// Queued tasks claim pool slots only after admission, under the start lock.
pub fn allocate_tree(tree: &mut Value, wd: &mut PathBuf, workdir: &Path, run: &Path, mode: &str) {
    if tree.is_object() && s(tree, "path").is_empty() {
        let slots = tree["config"]["pool"].as_u64().unwrap_or(0);
        let slot = (mode == "write" && slots > 0 && tree["in"].is_null())
            .then(|| crate::pool::claim(Path::new(s(tree, "source")), run, slots))
            .flatten();
        if mode == "write" && slots > 0 && tree["in"].is_null() && slot.is_none() {
            eprintln!("delegate: all {slots} pool slots are held; this task gets its own worktree");
        }
        let path = slot.clone().unwrap_or_else(|| {
            cache_dir().join("worktrees").join(format!(
                "{}-{}",
                Path::new(s(tree, "source"))
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                run.file_name().unwrap_or_default().to_string_lossy()
            ))
        });
        if slot.is_some() {
            tree["pooled"] = json!(true);
            // Only the run that claimed the slot resets it; a reply continuing in the slot
            // copies this record and must find its conversation's files untouched.
            tree["claimedBy"] = json!(run.file_name().unwrap_or_default().to_string_lossy());
        }
        tree["path"] = json!(path);
        let origin = if tree["in"].is_string() && s(tree, "reconstructedTree").is_empty() {
            Path::new(s(tree, "sourceWorkdir"))
        } else {
            workdir
        };
        *wd = path.join(
            origin
                .strip_prefix(s(tree, "source"))
                .unwrap_or(Path::new("")),
        );
    }
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
    if o.name.is_none() && parent_path.is_none() {
        o.name = Some(prompt_name(prompt));
    }
    let root = runs_root();
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let slots = state_dir();
    fs::create_dir_all(&slots).map_err(|e| e.to_string())?;
    if setting("RUNS", "").is_empty() && !root.join(".gitignore").exists() {
        write(root.join(".gitignore"), "*\n")?;
    }

    // Pre-lock work: git rev-parse, config prep, timeout validation, model listing, and
    // agent identity probing all run OUTSIDE `.start.lock` so concurrent starts never
    // serialize slow subprocesses behind the admission lock.
    let source_str = s(&extra["worktree"], "source");
    let repo = repo_key(if source_str.is_empty() {
        workdir
    } else {
        Path::new(source_str)
    });
    let mut extra = extra.clone();
    if let Some(overflow) = &o.overflow {
        if let Some(tiers) = extra["tierConfig"]["tiers"].as_object_mut() {
            for rules in tiers.values_mut().filter_map(Value::as_array_mut) {
                for rule in rules.iter_mut().filter(|rule| rule.is_object()) {
                    rule["overflow"] = json!(overflow);
                }
            }
        }
    }
    let listed_pi_models = if !o.images.is_empty() || (o.tier_picked && !o.agent_pinned) {
        agents::pi_models()
    } else {
        Vec::new()
    };
    let timeout_seconds = o
        .timeout
        .as_deref()
        .map(seconds)
        .transpose()?
        .unwrap_or(0.0);
    let accept_timeout_seconds = o
        .accept_timeout
        .as_deref()
        .map(seconds)
        .transpose()?;
    let evidence_timeout_seconds = o
        .evidence
        .as_deref()
        .filter(|command| !command.is_empty())
        .map(|_| seconds(o.evidence_timeout.as_deref().unwrap_or("30m")))
        .transpose()?;
    let fallback_name = o.name.clone().unwrap_or_else(|| {
        prompt
            .lines()
            .find(|x| !x.trim().is_empty())
            .unwrap_or("")
            .trim()
            .chars()
            .take(120)
            .collect()
    });
    let display_name = o.name.clone().unwrap_or_else(|| {
        if extra["parent"].is_object() {
            format!("reply to {}", s(&extra["parent"], "name"))
        } else {
            fallback_name.clone()
        }
    });
    let slug = o
        .name
        .as_deref()
        .unwrap_or("")
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || "._-".contains(c) {
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
    let pre_colleague = crate::agy::colleague(o.model.as_deref().unwrap_or("")).to_string();
    let pre_fork = extra
        .get("session")
        .and_then(Value::as_str)
        .filter(|x| !x.is_empty());
    let pre_parent_bin = s(&extra["parent"], "agentBin");
    let pre_identity = if !(extra["parent"].is_object() && pre_fork.is_some() && !pre_parent_bin.is_empty()) {
        agent_identity(&pre_colleague).ok()
    } else {
        None
    };

    let scout_first = b(&extra, "scoutFirst");
    // Critical section: ONLY capacity/admission checks + run directory & slot registration
    // under the single short-lived `<state>/.state.lock` (<5ms, zero git/subprocess/sleep).
    let (run, slot_file, deferred, reason, queued_ns, model_cap, started_at, started_epoch, started_ns, queued_at, q_ahead, area_experiment) = {
        let _state_lock = crate::locks::acquire_state(&slots).map_err(|e| e.to_string())?;

        let active_runs = active_machine(&slots);
        if !scout_first {
            capacity(&active_runs, &extra["configCapacity"], &repo)?;
        }
        if o.tier_picked && !o.agent_pinned {
            pick_tier_under_cap(&mut o, &mut extra, &active_runs, &listed_pi_models)?;
        }
        if !o.images.is_empty() {
            let model = o.model.as_deref().unwrap_or("");
            models::guard_images(model, &listed_pi_models)?;
        }
        let model_cap = models::model_cap(&extra["tierConfig"], o.model.as_deref().unwrap_or(""))?;
        let admission = json!({
            "configCapacity": extra["configCapacity"],
            "repoKey": repo,
            "model": o.model,
            "modelCap": model_cap,
            "parent": extra["parent"]["run"],
            "queuedNs": now_ns() as u64
        });
        let reason = if scout_first {
            None
        } else {
            crate::queue::reason(&admission, &active_runs)?
        };
        let deferred = reason.is_some();
        if !deferred && mode == "write" && !o.parallel && !extra["worktree"].is_object() {
            for existing in all_runs() {
                let m = json(existing.join("meta.json"));
                if s(&m, "mode") == "write"
                    && s(&m, "workdir") == workdir.to_string_lossy()
                    && (active(&state(&existing)) || agent_alive(&existing))
                {
                    return Err(format!(
                        "write run {} is still active in {}; wait for it, use --read-only, or pass --allow-parallel-writes",
                        existing.file_name().unwrap_or_default().to_string_lossy(),
                        workdir.display()
                    ));
                }
            }
        }
        if let Some(parent) = parent_path {
            let p = parent.file_name().unwrap_or_default().to_string_lossy();
            let replies = all_runs()
                .into_iter()
                .filter(|r| {
                    s(&json(r.join("meta.json")), "parent") == p && runs::continues_conversation(r)
                })
                .collect::<Vec<_>>();
            if let Some(last) = replies.last() {
                return Err(format!(
                    "{p} already has a reply ({}); wait for it and reply to that",
                    last.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
        }

        let stamp = format_time(true, "%Y%m%d-%H%M%S");
        let mut run = root.join(format!("{stamp}-{slug}"));
        while run.exists()
            || cache_dir()
                .join("archive")
                .join(run.file_name().unwrap_or_default())
                .exists()
        {
            run = root.join(format!("{stamp}-{slug}-{:04x}", now_ns() as u16));
        }
        fs::create_dir(&run).map_err(|e| e.to_string())?;
        fs::set_permissions(&run, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        write(run.join("launch.pid"), format!("{}\n", std::process::id()))?;

        let started_at = iso();
        let started_epoch = epoch() as i64;
        let started_ns = now_ns() as u64;
        let area_experiment = if extra["areaCandidate"].is_object() {
            Some(assign_area_ab_locked(&slots, &repo, &extra["areaCandidate"]))
        } else {
            None
        };
        let mut provisional = json!({
            "run": run.file_name().unwrap_or_default().to_string_lossy(),
            "dir": run,
            "workdir": workdir,
            "mode": mode,
            "agent": crate::agy::colleague(o.model.as_deref().unwrap_or("")),
            "tier": o.tier,
            "name": display_name,
            "model": o.model,
            "modelCap": model_cap,
            "repoKey": repo,
            "configCapacity": extra["configCapacity"],
            "worktree": extra["worktree"],
            "parent": if extra["parent"].is_object() { extra["parent"]["run"].clone() } else { Value::Null },
            "parallel": o.parallel,
            "startedAt": started_at,
            "startedEpoch": started_epoch,
            "startedNs": started_ns,
            "queuedNs": admission["queuedNs"].clone(),
        });
        if let Some(exp) = &area_experiment {
            provisional["area"] = exp["area"].clone();
            provisional["areaGroup"] = exp["group"].clone();
        }
        let (queued_at, q_ahead, queued_ns) = if scout_first {
            touch(run.join(".scouting"));
            provisional["scoutFirst"] = json!(true);
            provisional["scoutRun"] = extra["scoutRun"].clone();
            write_json(run.join("meta.json"), &provisional)?;
            (String::new(), 0usize, admission["queuedNs"].clone())
        } else if deferred {
            touch(run.join(".waiting"));
            crate::queue::enter(&run, &mut provisional)?;
            let ahead = crate::queue::ahead(&provisional);
            let q_reason = reason.as_deref().unwrap_or("总路数");
            crate::queue::update(&run, q_reason, ahead)?;
            (
                s(&provisional, "queuedAt").to_string(),
                ahead,
                provisional["queuedNs"].clone(),
            )
        } else {
            write_json(run.join("meta.json"), &provisional)?;
            (String::new(), 0usize, admission["queuedNs"].clone())
        };
        let hash = sha1_smol::Sha1::from(run.to_string_lossy().as_bytes())
            .digest()
            .to_string();
        let slot_file = slots.join(format!("{}.slot", &hash[..16]));
        write(&slot_file, format!("{}\n", run.display()))?;
        (
            run,
            slot_file,
            deferred,
            reason,
            queued_ns,
            model_cap,
            started_at,
            started_epoch,
            started_ns,
            queued_at,
            q_ahead,
            area_experiment,
        )
    };

    struct LaunchRollback {
        run: PathBuf,
        slot_file: PathBuf,
        claimed_slot: Option<PathBuf>,
        armed: bool,
    }
    impl Drop for LaunchRollback {
        fn drop(&mut self) {
            let _ = fs::remove_file(self.run.join("launch.pid"));
            if self.armed {
                if let Some(slot) = &self.claimed_slot {
                    crate::pool::release(slot, &self.run);
                }
                let _ = fs::remove_file(&self.slot_file);
                let _ = fs::remove_dir_all(&self.run);
            }
        }
    }
    let mut rollback = LaunchRollback {
        run: run.clone(),
        slot_file,
        claimed_slot: None,
        armed: true,
    };
    let extra = &extra;
    let name = fallback_name;
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
    let remaining = o.protect.iter().filter(|rule| !worktree::matches_rule(rule, &o.allow)).cloned().collect::<Vec<_>>();
    let mut scout_reply_map_end: Option<usize> = None;
    let scout_reply_composed: Option<String> = if extra["scoutReplyCompose"].is_object() {
        let rc = &extra["scoutReplyCompose"];
        let raw_brief = s(rc, "rawBrief");
        let note = {
            let n = s(rc, "note");
            if !n.is_empty() {
                n
            } else {
                s(rc, "l0")
            }
        };
        let ctrl_reply = s(rc, "controllerReply");
        let src_map = PathBuf::from(s(rc, "sourceMap"));
        let dest_map = run.join("scout-map.md");
        if src_map.is_file() {
            let _ = fs::copy(&src_map, &dest_map);
        }
        let _ = write(run.join("raw-brief.md"), raw_brief);
        let (composed, map_end) =
            crate::scout_first::build_writer_brief_with_end(raw_brief, note, Some(ctrl_reply));
        scout_reply_map_end = Some(map_end);
        Some(composed)
    } else {
        if scout_first {
            let raw = s(extra, "rawBrief");
            let raw = if raw.is_empty() { prompt } else { raw };
            let _ = write(run.join("raw-brief.md"), raw);
        }
        None
    };
    let base_prompt = scout_reply_composed.as_deref().unwrap_or(prompt);
    let prompt_with_area: Option<String> = area_experiment.as_ref().and_then(|exp| {
        if !b(exp, "injected") {
            return None;
        }
        let l0_lines = strings(&exp["l0Lines"]);
        if l0_lines.is_empty() {
            return None;
        }
        Some(format!("{}\n\n{}", base_prompt.trim_end(), l0_lines.join("\n")))
    });
    let effective_prompt = prompt_with_area.as_deref().unwrap_or(base_prompt);
    let mut actual = if parent.is_object() && fork.is_some() {
        contract(
            effective_prompt,
            if changed && !o.hide_accept {
                o.accept.as_deref()
            } else {
                None
            },
            false,
            changed && (o.hide_accept || o.accept.is_none()),
            &remaining,
            &o.protect_reasons,
        )
    } else {
        contract(
            effective_prompt,
            if o.hide_accept {
                None
            } else {
                o.accept.as_deref()
            },
            mode == "read-only" && extra["worktree"].is_object(),
            false,
            &remaining,
            &o.protect_reasons,
        )
    };
    if !o.allow.is_empty() {
        actual.push_str(&format!("\n本次开放 / Open for this task only: {}. These paths are removed from the protected set for this task; this overrides any broader protection above. Other protected paths remain closed.\n", o.allow.join(", ")));
    }
    if mode == "write" {
        actual = format!("{}\n\n{WRITE_BOUNDARY}\n", actual.trim_end_matches('\n'));
        let source = s(&extra["worktree"], "source");
        let source = if source.is_empty() {
            s(extra, "sourceBuildRoot")
        } else {
            source
        };
        if !source.is_empty() && prompt.contains(source) {
            eprintln!("delegate: note: prompt contains the source repository absolute path; other absolute paths are read-only / 提示包含源仓库绝对路径，勿在该路径写入");
        }
    }
    if extra["protectGenerated"].is_object() {
        actual.push_str(&format!("\n\n生成物保护例外 / Generated protection exception: {} 只允许通过生成命令改动 / may only be changed by the generator: `{}`. You may run this command; completion reruns it and rejects manually edited generated output. This exception overrides the protected-path restriction only for these generated paths.\n", extra["protectGenerated"]["paths"], s(&extra["protectGenerated"], "command")));
    }
    if let Some(limit) = o.max_answer {
        actual.push_str(&format!(
            "\n\n答复不超过 {limit} 字 / Answer in no more than {limit} characters.\n"
        ));
    }
    if !o.share.is_empty() {
        actual.push_str(&format!("\n\n可读输入：{}（只读）\n", o.share.join("、")));
    }
    if let Some(conflicts) = extra["denyAcceptConflicts"]
        .as_array()
        .filter(|x| !x.is_empty())
    {
        let commands = conflicts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("；");
        actual.push_str(&format!("\n\n验收命令冲突说明：以下命令命中 agentDeny，由主控负责运行：{commands}。同事无需运行这些命令。\n"));
    }
    let standing = s(extra, "standing");
    let inherited = b(extra, "standingInherited");
    if !inherited && !standing.is_empty() {
        actual.push_str(&format!("\n\n{standing}\n"));
    }
    eprintln!(
        "delegate: standing 附加 {} 行，来源 {}{}",
        if inherited {
            0
        } else {
            standing.lines().count()
        },
        extra["standingSources"],
        if inherited { "（已有会话）" } else { "" }
    );
    let model = o.model.as_deref().unwrap_or("（Pi 默认）");
    let alias_input = o.model_name.as_deref().unwrap_or(model);
    let resolved_alias = models::selected_alias(alias_input, &extra["tierConfig"]);
    let model_cost = models::model_cost(alias_input, &extra["tierConfig"]);
    let model_display = match model_cost {
        Some(cost) if resolved_alias != model => format!("{resolved_alias}〔{cost}〕（{model}）"),
        Some(cost) => format!("{model}〔{cost}〕"),
        None => model.to_string(),
    };
    let inferred_name = match model_cost {
        Some(cost) => format!("{resolved_alias}〔{cost}〕"),
        None => o.model_name.as_deref().unwrap_or(model).to_string(),
    };
    let rule = if o.tier_picked {
        models::tier_rules(&extra["tierConfig"], o.tier.as_deref().unwrap_or("review"))
            .into_iter()
            .nth(extra["tierCandidateIndex"].as_u64().unwrap_or(0) as usize)
            .unwrap_or(Value::Null)
    } else {
        json!(o.model_name)
    };
    let source: String = if !setting("THINKING", "").is_empty() {
        "环境".into()
    } else if o.thinking_cli.is_some() {
        "CLI".into()
    } else if let Some(class) = o
        .class
        .as_deref()
        .filter(|c| crate::fit::default_thinking(c).is_some())
    {
        format!("类别 {class}")
    } else if parent_path.is_some() && !o.tier_picked && !o.agent_pinned {
        "原会话".into()
    } else {
        models::thinking_source(None, &rule, &extra["tierConfig"]).into()
    };
    let level = format!(
        "{}〔{}〕",
        o.thinking.as_deref().unwrap_or("Pi 默认"),
        source
    );
    if o.tier_inferred && o.tier_picked {
        if o.class.is_some() {
            eprintln!(
                "{}",
                models::inference_notice(
                    o.tier.as_deref().unwrap_or("review"),
                    &inferred_name,
                    model,
                    Some(&level)
                )
            );
        } else {
            eprintln!(
                "{}",
                models::inference_notice(
                    o.tier.as_deref().unwrap_or("review"),
                    &inferred_name,
                    model,
                    o.thinking.as_deref()
                ) + &format!("〔{source}〕")
            );
        }
    } else if parent_path.is_some() && !o.tier_picked && !o.agent_pinned {
        eprintln!("delegate: 沿用原会话的模型 {model_display}，thinking {level}");
    } else if o.agent_pinned {
        eprintln!("delegate: 固定模型 {model_display}，thinking {level}（--model）");
    } else {
        eprintln!(
            "delegate: {} 档，模型 {model_display}，thinking {level}",
            o.tier.as_deref().unwrap_or("")
        );
    }

    if let Some(class) = &o.class {
        match crate::fit::notices(
            class,
            o.model_name.as_deref().unwrap_or(model),
            &extra["tierConfig"],
        ) {
            Ok(notices) => {
                for notice in notices {
                    eprintln!("delegate: {notice}");
                }
            }
            Err(error) => {
                eprintln!("delegate: 适配卡未读取（{error}）；请检查 delegate fit --json")
            }
        }
    }
    if extra["defaults"].as_object().is_some_and(|d| !d.is_empty()) {
        eprintln!("delegate: 生效默认值及来源 {}", extra["defaults"]);
    }
    if b(extra, "remoteOff") {
        eprintln!("delegate: 远程：关");
    }
    write(
        run.join("prompt.md"),
        if actual.ends_with('\n') {
            actual
        } else {
            format!("{actual}\n")
        },
    )?;
    let mut fork_value = fork.map(str::to_string);
    let mut pending_fork = Value::Null;
    if let (true, Some(fork_id), true) = (
        parent.is_object(),
        fork,
        crate::agy::is_agy(s(parent, "model")),
    ) {
        // agy keeps the conversation itself; the reply continues it by id.
        fork_value = Some(fork_id.to_string());
    } else if let (true, Some(fork_id)) = (parent.is_object(), fork) {
        let source = session_file(s(parent, "sessionDir"), fork_id)
            .ok_or_else(|| "missing parent session".to_string())?;
        let target = run
            .join("fork")
            .join(source.file_name().unwrap_or_default());
        pending_fork = json!({
            "source": source,
            "target": target,
        });
        fork_value = Some(target.to_string_lossy().into());
    }
    let mut tree = extra["worktree"].clone();
    let mut wd = workdir.to_path_buf();
    if !deferred && !scout_first {
        allocate_tree(&mut tree, &mut wd, workdir, &run, mode);
    }
    let run_id = run.file_name().unwrap_or_default().to_string_lossy();
    let claimed_slot = b(&tree, "pooled") && s(&tree, "claimedBy") == run_id;
    if claimed_slot {
        let p = PathBuf::from(s(&tree, "path"));
        if !p.as_os_str().is_empty() {
            rollback.claimed_slot = Some(p);
        }
    }
    let new_tree =
        tree.is_object() && (claimed_slot || !Path::new(s(&tree, "path")).exists());
    let top = if tree.is_object() {
        let p = PathBuf::from(s(&tree, "path"));
        if !p.as_os_str().is_empty() {
            p
        } else {
            PathBuf::from(s(&tree, "source"))
        }
    } else {
        git_top_fast(&wd).unwrap_or_default()
    };
    let mut exclude = vec![];
    if tree.is_object() {
        for key in ["copy", "link"] {
            if let Some(a) = tree["config"][key].as_array() {
                exclude.extend(a.iter().filter_map(Value::as_str).map(str::to_string));
            }
        }
        let source = Path::new(s(&tree, "source"));
        for item in &o.share {
            if let Ok(relative) = Path::new(item).strip_prefix(source) {
                if !relative.as_os_str().is_empty() {
                    exclude.push(relative.to_string_lossy().into_owned());
                }
            }
        }
    }
    let applied_base = if parent.is_object() && !b(extra, "newAfterApply") {
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
    let chain_base = if !s(&extra["syncedBase"], "tree").is_empty() {
        s(&extra["syncedBase"], "tree").to_string()
    } else if !b(extra, "newAfterApply") && !s(parent, "chainBase").is_empty() {
        s(parent, "chainBase").to_string()
    } else {
        String::new()
    };
    let mut meta = json!({"run":run.file_name().unwrap_or_default().to_string_lossy(),"dir":run,"workdir":wd,"mode":mode,"agent":crate::agy::colleague(o.model.as_deref().unwrap_or("")),"tier":o.tier,"name":o.name.clone().unwrap_or_else(||if parent.is_object(){format!("reply to {}",s(parent,"name"))}else{name}),"caller":caller(),"callerSource":caller_source().map(|(_, source)| source),"provider":o.provider,"model":o.model,"thinking":o.thinking,"timeout":o.timeout,"timeoutSeconds":timeout_seconds,"accept":o.accept,"acceptTimeoutSeconds":accept_timeout_seconds,"retries":o.retries,"images":o.images,"protect":o.protect,"top":if top.as_os_str().is_empty(){Value::Null}else{json!(top)},"base":Value::Null,"snapshotExclude":exclude,"worktree":tree,"env":extra.get("env").cloned().unwrap_or(json!({})),"chainBase":chain_base,"appliedBase":applied_base,"sessionDir":run.join("session"),"parent":if parent.is_object(){parent["run"].clone()}else{Value::Null},"fork":fork_value,"parallel":o.parallel,"startedAt":started_at,"startedEpoch":started_epoch,"startedNs":started_ns,"phase":"preparing","pendingPrepare":true,"newTree":new_tree});
    if pending_fork.is_object() {
        meta["pendingFork"] = pending_fork;
    }
    if extra["pendingSync"].is_object() {
        meta["pendingSync"] = extra["pendingSync"].clone();
    }
    if b(extra, "newAfterApply") {
        meta["newAfterApply"] = json!(true);
    }
    // A forked session continues on the executable that wrote it: Pi and rpi
    // keep different session formats.
    let parent_bin = s(parent, "agentBin");
    let (agent_bin, agent_version) = if !meta["fork"].is_null() && !parent_bin.is_empty() {
        if !Path::new(parent_bin).is_file() {
            return Err(format!(
                "this conversation ran on {parent_bin}, which is gone; continue with --fresh"
            ));
        }
        (
            PathBuf::from(parent_bin),
            parent["agentVersion"].as_str().map(str::to_string),
        )
    } else if s(&meta, "agent") == pre_colleague {
        if let Some(id) = pre_identity {
            id
        } else {
            agent_identity(s(&meta, "agent"))?
        }
    } else {
        agent_identity(s(&meta, "agent"))?
    };
    meta["modelCap"] = model_cap;
    meta["queuedNs"] = queued_ns;
    if !queued_at.is_empty() {
        meta["queuedAt"] = json!(queued_at);
    }
    meta["agentPinned"] = json!(o.agent_pinned);
    if let Some(command) = o.evidence.filter(|command| !command.is_empty()) {
        meta["evidenceCommand"] = json!(command);
        meta["evidenceTimeoutSeconds"] = json!(evidence_timeout_seconds.unwrap_or(1800.0));
    }
    meta["allow"] = json!(o.allow);
    meta["shell"] = json!(o.shell);
    if meta["worktree"].is_object() {
        let source = PathBuf::from(s(&meta["worktree"], "source"));
        let fresh_wt = b(extra, "newAfterApply") || b(extra, "scoutReply");
        if !s(&extra["syncedBase"], "head").is_empty() {
            meta["netBase"] = extra["syncedBase"]["head"].clone();
        } else if !fresh_wt && parent.is_object() && !s(parent, "netBase").is_empty() {
            meta["netBase"] = parent["netBase"].clone();
            if b(parent, "netBaseRebased") {
                meta["netBaseRebased"] = json!(true);
                if !s(parent, "netBaseTree").is_empty() {
                    meta["netBaseTree"] = parent["netBaseTree"].clone();
                }
            }
        } else if !scout_first {
            if let Some(head) = read_git_head(&source) {
                meta["netBase"] = json!(head);
            }
        }
    }
    meta["agentBin"] = json!(agent_bin);
    meta["configSources"] = extra.get("configSources").cloned().unwrap_or(json!([]));
    meta["configCapacity"] = extra["configCapacity"].clone();
    meta["tierModels"] = extra["tierModels"].clone();
    meta["tierCandidates"] = extra["tierCandidates"].clone();
    meta["tierCandidateIndex"] = extra["tierCandidateIndex"].clone();
    meta["tierInferred"] = json!(o.tier_inferred);
    meta["modelName"] = json!(o.model_name);
    for key in [
        "standing",
        "standingSources",
        "defaults",
        "cleanupKeepExecutables",
        "share",
        "acceptBlind",
        "acceptRules",
        "acceptAlso",
        "controllerReviewPaths",
        "riskRules",
        "reviewConfig",
        "resources",
        "remote",
        "remoteOff",
        "lanePriority",
        "measure",
        "measureCpus",
        "stall",
        "checkIn",
        "agy",
    ] {
        meta[key] = extra[key].clone();
    }
    if o.review_full {
        meta["reviewFull"] = json!(true);
    }
    if !s(extra, "agyAccount").is_empty() {
        meta["agyAccount"] = extra["agyAccount"].clone();
    }
    meta["resultDisplayChars"] = extra["resultChars"].clone();
    if let Some(limit) = o.max_answer {
        meta["maxAnswer"] = json!(limit);
    }
    meta["sourceBuild"] = extra["sourceBuild"].clone();
    meta["sourceBuildRoot"] = extra["sourceBuildRoot"].clone();
    meta["repoKey"] = json!(repo);
    if let Some(rules) = extra.get("agentDeny").filter(|rules| !rules.is_null()) {
        meta["agentDeny"] = rules.clone();
    }
    if !extra["continuation"].is_null() {
        meta["continuation"] = extra["continuation"].clone();
    }
    if extra["protectGenerated"].is_object() {
        meta["protectGenerated"] = extra["protectGenerated"].clone();
    }
    if let Some(version) = agent_version {
        meta["agentVersion"] = json!(version);
    }
    if !o.protect_reasons.is_empty() {
        meta["protectReasons"] = json!(o.protect_reasons);
    }
    if extra["promptProtectedPaths"]
        .as_array()
        .is_some_and(|items| !items.is_empty())
    {
        meta["promptProtectedPaths"] = extra["promptProtectedPaths"].clone();
    }
    if let Some(rework) = &o.rework {
        meta["rework"] = rework.clone();
    }
    if let Some(class) = &o.class {
        meta["class"] = json!(class);
    } else if !s(parent, "class").is_empty() {
        meta["class"] = parent["class"].clone();
    }
    if let Some(wid) = &o.work {
        meta["workId"] = json!(wid);
    }
    if let Some(exp) = &area_experiment {
        meta["area"] = exp["area"].clone();
        meta["areaGroup"] = exp["group"].clone();
        meta["areaExperiment"] = json!({
            "area": exp["area"],
            "group": exp["group"],
            "seq": exp["seq"],
            "l0": exp["l0"],
            "injected": exp["injected"],
            "matchedPaths": exp["matchedPaths"],
        });
    }
    let effective_prompt_split = effective_prompt.trim_end_matches('\n').len();
    if scout_first {
        meta["phase"] = json!("scouting");
        meta["scoutFirst"] = json!(true);
        meta["scoutRun"] = extra["scoutRun"].clone();
        meta["scoutRunDir"] = extra["scoutRunDir"].clone();
        meta["scoutPromptSplit"] = json!(effective_prompt_split);
    }
    if b(extra, "scoutReply") {
        meta["scoutReply"] = json!(true);
    }
    if let Some(map_end) = scout_reply_map_end {
        meta["scoutMapEnd"] = json!(map_end);
    }
    if !s(extra, "scoutLaunchError").is_empty() {
        meta["scoutLaunchError"] = extra["scoutLaunchError"].clone();
    }
    if extra["scoutCarried"].is_object() {
        let mut carried = extra["scoutCarried"].clone();
        if run.join("scout-map.md").is_file() {
            let abs = run
                .join("scout-map.md")
                .canonicalize()
                .unwrap_or_else(|_| run.join("scout-map.md"));
            carried["mapPath"] = json!(abs);
        }
        meta["scout"] = carried;
        let carried_run = s(&meta["scout"], "run").to_string();
        if !carried_run.is_empty() {
            meta["scoutRun"] = json!(carried_run);
        } else if !s(extra, "scoutRun").is_empty() {
            meta["scoutRun"] = extra["scoutRun"].clone();
        }
    }
    let user_prompt_len = if scout_reply_composed.is_some() || prompt_with_area.is_some() {
        effective_prompt_split
    } else {
        prompt.trim_end_matches('\n').len()
    };
    meta["userPromptLen"] = json!(user_prompt_len);
    write_json(run.join("meta.json"), &meta)?;
    if !s(&extra["syncedBase"], "commit").is_empty() {
        let commit = s(&extra["syncedBase"], "commit");
        write(run.join("worktree-base-commit"), commit)?;
    }
    if !extra["sync"].is_null() {
        write_json(run.join("sync.json"), &extra["sync"])?;
    }
    touch_caller_session();
    // Single short-lived `.state.lock` commit for work table, repo registry, event, and index (<3ms).
    {
        let _state_lock = crate::locks::acquire_state(&slots).map_err(|e| e.to_string())?;
        if let Some(wid) = &o.work {
            crate::work::on_run_started(wid, &run, &meta);
        }
        crate::notes::register_repo();
        if deferred {
            let q_reason = reason.as_deref().unwrap_or("总路数");
            crate::events::emit(
                &crate::events::run_id_for(&run, &meta),
                &crate::events::repo_for_meta(&meta),
                "queued",
                json!({
                    "scope": "capacity",
                    "name": s(&meta, "name"),
                    "reason": q_reason,
                    "ahead": q_ahead,
                }),
            );
        } else {
            crate::events::emit(
                &crate::events::run_id_for(&run, &meta),
                &crate::events::repo_for_meta(&meta),
                "started",
                crate::events::started_payload(&meta, false),
            );
        }
        let initial_state = if scout_first {
            "scouting"
        } else if deferred {
            "waiting"
        } else {
            "starting"
        };
        let initial_status = json!({
            "run": meta["run"],
            "name": meta["name"],
            "state": initial_state,
            "agent": meta["agent"],
            "mode": meta["mode"],
            "dir": run.to_string_lossy(),
        });
        crate::index::upsert_run_locked(
            &slots,
            &run,
            &crate::events::repo_for_meta(&meta),
            initial_status,
        );
    }
    if !deferred && !scout_first {
        crate::eta::record_pred_started(&run, &meta);
    }
    let explicit_startup_timeout = if cfg!(debug_assertions) {
        std::env::var("DELEGATE_TEST_STARTUP_TIMEOUT")
            .ok()
            .and_then(|value| seconds(&value).ok())
    } else {
        None
    };
    touch(run.join("events.jsonl"));
    let log = File::create(run.join("supervisor.log")).map_err(|e| e.to_string())?;
    let mut c = Command::new(script());
    c.arg("_supervise")
        .arg(&run)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log))
        .env_remove("DELEGATE_LANE_HELD");
    group(&mut c);
    let mut child = match c.spawn() {
        Ok(child) => child,
        Err(error) => {
            rollback.armed = false;
            finish_run(
                &run,
                json!({"state":"crashed","error":error.to_string()}),
                1,
            )?;
            return Err(error.to_string());
        }
    };
    if let Some(startup_timeout) = explicit_startup_timeout {
        let deadline = std::time::Instant::now() + Duration::from_secs_f64(startup_timeout);
        while std::time::Instant::now() < deadline {
            if run.join("pid").is_file() {
                rollback.armed = false;
                drop(rollback);
                return Ok(run);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        rollback.armed = false;
        end_group(child.id() as i32, 2.0);
        child.wait().map_err(|e| e.to_string())?;
        finish_run(
            &run,
            json!({"state":"crashed","error":"supervisor did not start"}),
            1,
        )?;
        return Err(format!(
            "supervisor did not start; see {}",
            run.join("supervisor.log").display()
        ));
    }
    // Write `pid` immediately so `status`, `stop`, and `wait` know the supervisor PID right away
    // without blocking `start`/`reply` on a polling loop (<50ms return).
    let _ = write(run.join("pid"), format!("{}\n", child.id()));
    rollback.armed = false;
    drop(rollback);
    Ok(run)
}

#[cfg(test)]
mod purpose_tests {
    use super::*;
    #[test]
    fn inference_is_explicit_and_model_tier_are_independent() {
        let mut o = Options::new();
        o.read_only = true;
        choose_tier(&mut o).unwrap();
        assert_eq!(o.tier.as_deref(), Some("review"));
        assert!(o.tier_inferred);
        let mut o = Options::new();
        choose_tier(&mut o).unwrap();
        assert_eq!(o.tier.as_deref(), Some("write"));
        for tier in ["scout", "review"] {
            let mut o = Options::new();
            o.tier = Some(tier.into());
            choose_tier(&mut o).unwrap();
            assert!(o.read_only);
        }
        let mut o = Options::new();
        o.model = Some("sol".into());
        choose_tier(&mut o).unwrap();
        assert!(o.agent_pinned);
        assert!(!o.tier_inferred);
        assert!(o.tier.is_none());
        let parse = |args: &[&str], reply| {
            parse_launch(
                &args.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                reply,
                false,
            )
        };
        assert!(parse(&["--tier", "review", "--model", "sol", "task"], false).is_ok());
        assert!(parse(&["--model", "sol:high", "task"], false).is_err());
        assert!(parse(
            &["last", "--model", "sol", "--thinking", "low", "more"],
            true
        )
        .is_ok());
        for tier in ["cheap", "strong", "default", "vision"] {
            assert!(parse(&["--tier", tier, "task"], false).is_err());
        }
        assert!(parse(&["--after", "prev", "task"], false).is_err());
        assert!(parse(&["last", "--allow", "apps/web/", "go"], true).is_ok());
        assert!(parse(&["--no-remote", "task"], false).unwrap().no_remote);
        assert!(parse(&["last", "--no-remote", "go"], true).unwrap().no_remote);
        assert!(parse(&["--no-brief-check", "task"], false)
            .unwrap()
            .no_brief_check);
        assert!(parse(&["last", "--no-brief-check", "go"], true)
            .unwrap()
            .no_brief_check);
    }

    #[test]
    fn prompt_protected_path_extraction_handles_prose_and_allow() {
        let protect = vec!["apps/".to_string(), "Cargo.toml".to_string()];
        let found = protected_paths_in_prompt(
            "请修改apps/web/src/App.tsx:42和 `apps/api/`，参考 https://example.com/apps/x 与 Cargo.toml。",
            None,
            &protect,
            &["apps/api/".to_string()],
        );
        assert_eq!(found, vec!["Cargo.toml", "apps/web/src/App.tsx"]);
    }

    #[test]
    fn areas_matching_prefers_longest_prefix_respects_boundaries_and_alternates_ab() {
        let dir = std::env::temp_dir().join(format!("delegate-area-ab-{}", now_ns()));
        let repo = dir.join("repo");
        let state = dir.join("state");
        fs::create_dir_all(repo.join(".local/notes/areas")).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(
            repo.join(".local/notes/areas/delegate.l0.md"),
            "L0-line-1\n\nL0-line-2\nL0-line-3\nL0-line-4-ignored\n",
        )
        .unwrap();
        fs::write(
            repo.join(".local/notes/areas/tui.l0.md"),
            "TUI-1\nTUI-2\n",
        )
        .unwrap();

        let areas = json!({
            "delegate": {"paths": ["crates/delegate"], "l0": ".local/notes/areas/delegate.l0.md"},
            "tui": {"paths": ["crates/delegate-tui/"], "l0": ".local/notes/areas/tui.l0.md"}
        });

        // Boundary check: `crates/delegate-tui/src/app.rs` matches `tui`, NOT `delegate`.
        let m_tui = match_area_for_start(
            &areas,
            &repo,
            &repo,
            "修 crates/delegate-tui/src/app.rs 的刷新",
            None,
        )
        .unwrap();
        assert_eq!(s(&m_tui, "area"), "tui");
        assert_eq!(strings(&m_tui["l0Lines"]), vec!["TUI-1", "TUI-2"]);

        // 3-line truncation on `delegate` area.
        let m_del = match_area_for_start(
            &areas,
            &repo,
            &repo,
            "改 crates/delegate/src/worktree.rs",
            None,
        )
        .unwrap();
        assert_eq!(s(&m_del, "area"), "delegate");
        assert_eq!(
            strings(&m_del["l0Lines"]),
            vec!["L0-line-1", "L0-line-2", "L0-line-3"]
        );

        // Unmatched path -> None.
        assert!(match_area_for_start(&areas, &repo, &repo, "改 docs/readme.md", None).is_none());

        // Deterministic parity alternation per area (odd -> B injected, even -> A control).
        let repo_key = repo.to_string_lossy().into_owned();
        let e1 = assign_area_ab_locked(&state, &repo_key, &m_del);
        let e2 = assign_area_ab_locked(&state, &repo_key, &m_del);
        let e3 = assign_area_ab_locked(&state, &repo_key, &m_del);
        let t1 = assign_area_ab_locked(&state, &repo_key, &m_tui);
        assert_eq!((s(&e1, "group"), b(&e1, "injected"), e1["seq"].as_u64()), ("B", true, Some(1)));
        assert_eq!((s(&e2, "group"), b(&e2, "injected"), e2["seq"].as_u64()), ("A", false, Some(2)));
        assert_eq!((s(&e3, "group"), b(&e3, "injected"), e3["seq"].as_u64()), ("B", true, Some(3)));
        assert_eq!((s(&t1, "group"), b(&t1, "injected"), t1["seq"].as_u64()), ("B", true, Some(1)));

        let _ = fs::remove_dir_all(&dir);
    }
}
