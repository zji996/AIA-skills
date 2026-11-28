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
    // Last time the running command showed output progress (streaming stdout
    // events or remote-output.log / stderr.log growth) and lane queue seconds
    // at that moment.
    command_output_at: Option<Instant>,
    command_output_queued: f64,
    command_remote_log: Option<FileFingerprint>,
    command_stderr_log: Option<FileFingerprint>,
    command: String,
    // Rate-limit stall reminders so we warn once per stall interval without spinning or killing.
    idle_warned_at: Option<Instant>,
    command_warned_at: Option<Instant>,
}

fn is_command_output_event(on_agy: bool, event: &Value) -> bool {
    if on_agy {
        if s(event, "event") != "step_update" {
            return false;
        }
        let u = &event["step_update"];
        s(u, "step_type") == "tool"
            && s(u, "tool_name") == "run_command"
            && s(u, "state") == "ACTIVE"
    } else {
        s(event, "type") == "tool_execution_update"
            && matches!(s(event, "toolName"), "bash" | "")
    }
}

/// Split a shell command string on `;`, `&&`, `||`, `|`, and `\n` while respecting single/double quotes.
fn split_shell_segments(cmd: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut cur = String::new();
    let mut chars = cmd.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    while let Some(c) = chars.next() {
        if escaped {
            cur.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' && !in_single {
            cur.push(c);
            escaped = true;
            continue;
        }
        if c == '\'' && !in_double {
            in_single = !in_single;
            cur.push(c);
            continue;
        }
        if c == '"' && !in_single {
            in_double = !in_double;
            cur.push(c);
            continue;
        }
        if !in_single && !in_double {
            match c {
                ';' | '\n' => {
                    if !cur.trim().is_empty() {
                        segments.push(std::mem::take(&mut cur));
                    } else {
                        cur.clear();
                    }
                    continue;
                }
                '&' if chars.peek() == Some(&'&') => {
                    chars.next();
                    if !cur.trim().is_empty() {
                        segments.push(std::mem::take(&mut cur));
                    } else {
                        cur.clear();
                    }
                    continue;
                }
                '|' => {
                    if chars.peek() == Some(&'|') {
                        chars.next();
                    }
                    if !cur.trim().is_empty() {
                        segments.push(std::mem::take(&mut cur));
                    } else {
                        cur.clear();
                    }
                    continue;
                }
                _ => {}
            }
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        segments.push(cur);
    }
    segments
}

/// True if `seg` contains a shell output redirection to a file (ignoring `2>&1`, `>/dev/null`, `2>/dev/null`).
fn has_file_write_redirection(seg: &str) -> bool {
    let bytes = seg.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && !in_single {
            i += 2;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_single && !in_double && b == b'>' {
            let rest = &seg[i..];
            let allowed = rest.starts_with(">&1")
                || rest.starts_with(">&2")
                || rest.starts_with(">/dev/null")
                || rest.starts_with("> /dev/null")
                || rest.starts_with(">>/dev/null")
                || rest.starts_with(">> /dev/null");
            if !allowed {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Extract command basename and remaining argument tokens from a single shell segment,
/// stripping leading env assignments (`FOO=bar`), `env`, `command`, and shell wrappers.
fn segment_cmd_and_args(seg: &str) -> Option<(String, Vec<String>)> {
    let mut tokens = seg.split_whitespace().peekable();
    while let Some(&tok) = tokens.peek() {
        let stripped = tok.trim_start_matches(['(', '{', '!']);
        if stripped.is_empty() {
            tokens.next();
            continue;
        }
        if matches!(stripped, "env" | "command" | "builtin" | "time" | "do" | "then" | "else" | "elif" | "while" | "until") {
            tokens.next();
            continue;
        }
        if stripped.contains('=')
            && !stripped.starts_with('-')
            && stripped
                .split_once('=')
                .is_some_and(|(k, _)| !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        {
            tokens.next();
            continue;
        }
        let first = stripped.trim_end_matches([')', '}']);
        tokens.next();
        let base = first.rsplit('/').next().unwrap_or(first).to_string();
        let rest: Vec<String> = tokens.map(str::to_string).collect();
        return Some((base, rest));
    }
    None
}

/// Returns true for pure waiting/polling shell commands (`sleep`, `wait`, `tail` polling,
/// optionally combined with read-only liveness/log-inspection companions like `ps`, `kill -0`,
/// `pgrep`, `jobs`, `wc`, `head`, `ls`, `stat`, `cat`, `test`, `[`, `echo`, `date`, `cd`, `true`).
/// Mutating commands or file-write redirections always return `false`.
pub(crate) fn is_pure_wait_command(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return false;
    }
    let segments = split_shell_segments(trimmed);
    if segments.is_empty() {
        return false;
    }
    let mut has_wait_core = false;
    for seg in &segments {
        if has_file_write_redirection(seg) {
            return false;
        }
        let Some((base, args)) = segment_cmd_and_args(seg) else {
            continue;
        };
        if matches!(base.as_str(), "done" | "fi") {
            continue;
        }
        if matches!(base.as_str(), "sleep" | "wait" | "tail") {
            has_wait_core = true;
            continue;
        }
        // `kill -0 <pid>` is a pure liveness probe; any other `kill` sends a signal.
        if base == "kill" {
            if args.iter().any(|a| a == "-0" || a == "-s0") {
                continue;
            }
            return false;
        }
        // `sed -i` mutates files in-place; reject it.
        if base == "sed" && args.iter().any(|a| a == "-i" || a.starts_with("-i")) {
            return false;
        }
        let read_only_companion = matches!(
            base.as_str(),
            "cd" | "true"
                | "false"
                | ":"
                | "echo"
                | "printf"
                | "date"
                | "ps"
                | "pgrep"
                | "jobs"
                | "wc"
                | "head"
                | "stat"
                | "ls"
                | "test"
                | "["
                | "[["
                | "cat"
                | "grep"
                | "rg"
                | "sed"
                | "awk"
                | "tr"
                | "cut"
                | "sort"
                | "uniq"
        );
        if !read_only_companion {
            return false;
        }
    }
    has_wait_core
}

/// Mechanical repetition detector (ADR 0120): only consecutive identical commands with identical
/// outcomes count; edits/writes or changed arguments/outcomes reset the streak.
/// Pure waiting/polling commands (`sleep`, `wait`, `tail` polling) are excluded for normal writers
/// so polling a background build/test never triggers false loop warnings; reviewers disable this
/// exemption (`ignore_wait_exemption = true`) because acceptance is owned by the Host.
struct LoopDetector {
    threshold: usize,
    ignore_wait_exemption: bool,
    last_sig: Option<(String, bool, String)>,
    streak: usize,
}

impl LoopDetector {
    fn new(threshold: usize, ignore_wait_exemption: bool) -> Self {
        Self {
            threshold,
            ignore_wait_exemption,
            last_sig: None,
            streak: 0,
        }
    }

    fn reset(&mut self) {
        self.last_sig = None;
        self.streak = 0;
    }

    /// Record a completed bash command; returns `Some(count)` when the threshold is reached
    /// and resets the streak so another `threshold` identical repeats are needed to trigger again.
    fn record_bash(&mut self, cmd: &str, ok: bool, detail: &str) -> Option<usize> {
        if self.threshold == 0
            || cmd.trim().is_empty()
            || (!self.ignore_wait_exemption && is_pure_wait_command(cmd))
        {
            return None;
        }
        let sig = (cmd.trim().to_string(), ok, detail.trim().to_string());
        if self.last_sig.as_ref() == Some(&sig) {
            self.streak += 1;
        } else {
            self.last_sig = Some(sig);
            self.streak = 1;
        }
        if self.streak >= self.threshold {
            let count = self.streak;
            self.streak = 0;
            Some(count)
        } else {
            None
        }
    }
}

fn loop_threshold(meta: &Value) -> usize {
    let default = meta["loop"]["threshold"]
        .as_u64()
        .map(|n| n.to_string())
        .unwrap_or_else(|| "12".into());
    let raw = setting("LOOP_THRESHOLD", &default);
    raw.trim().parse::<usize>().unwrap_or(12)
}

/// Optional ADR 0120 colleague-invisible background fork self-check on a copied Pi session file.
/// Never mutates the live session and never kills the colleague; records a `loop_review` event.
fn spawn_isolated_loop_review(meta: &Value, run: &Path, cmd: &str, count: usize) {
    if crate::agy::is_agy(s(meta, "model")) {
        return;
    }
    let enabled = setting("LOOP_REVIEW", "") == "1" || b(&meta["loop"], "review");
    if !enabled {
        return;
    }
    let session_dir = Path::new(s(meta, "sessionDir"));
    let Some(latest) = session_files(session_dir).into_iter().last() else {
        return;
    };
    let bin = Some(s(meta, "agentBin"))
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| agent_bin(AGENT));
    let Some(bin) = bin else {
        return;
    };
    let meta = meta.clone();
    let run = run.to_path_buf();
    let cmd = cmd.to_string();
    std::thread::spawn(move || {
        let scratch = std::env::temp_dir().join(format!(
            "delegate-loop-review-{}-{}",
            std::process::id(),
            now_ns()
        ));
        if fs::create_dir_all(&scratch).is_err() {
            return;
        }
        let copy = scratch.join(latest.file_name().unwrap_or_default());
        if fs::copy(&latest, &copy).is_err() {
            let _ = fs::remove_dir_all(&scratch);
            return;
        }
        // Copy rpi progress sidecar if present.
        let mut sidecar_name = latest.as_os_str().to_owned();
        sidecar_name.push(".progress.jsonl");
        let sidecar = PathBuf::from(sidecar_name);
        if sidecar.is_file() {
            let mut copy_sidecar = copy.as_os_str().to_owned();
            copy_sidecar.push(".progress.jsonl");
            let _ = fs::copy(&sidecar, PathBuf::from(copy_sidecar));
        }
        let prompt = format!(
            "你是后台自检复核（同事看不见此消息）：同事刚连续 {count} 次执行完全相同的命令 `{cmd}` 且结果相同。\
             请判断是否陷入机械重复。只输出一行 JSON：`{{\"stop\": true|false, \"reason\": \"一句话理由\"}}`，不要调用任何工具。\n"
        );
        let mut c = std::process::Command::new(&bin);
        c.args([
            "--fork",
            &copy.to_string_lossy(),
            "--session-dir",
            &scratch.to_string_lossy(),
            "--mode",
            "json",
            "--tools",
            "",
            "-p",
        ]);
        for key in ["provider", "model", "thinking"] {
            if !s(&meta, key).is_empty() {
                c.args([format!("--{key}"), s(&meta, key).to_string()]);
            }
        }
        c.current_dir(s(&meta, "workdir"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        clean_env(&mut c, &meta["env"]);
        c.env(
            "CARGO_BUILD_JOBS",
            crate::lane::local_heavy_jobs(Some(&run)).to_string(),
        );
        if let Ok(mut child) = c.spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(prompt.as_bytes());
            }
            if let Ok(out) = child.wait_with_output() {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let mut text = String::new();
                for line in stdout.lines() {
                    if let Ok(ev) = serde_json::from_str::<Value>(line) {
                        for item in filter_pi(&ev) {
                            if s(&item, "e") == "result" {
                                text = s(&item, "text").to_string();
                            }
                        }
                    }
                }
                if let Some(start) = text.find('{') {
                    if let Some(end) = text.rfind('}') {
                        if let Ok(verdict) = serde_json::from_str::<Value>(&text[start..=end]) {
                            if verdict.get("stop").and_then(Value::as_bool).is_some() {
                                let stop = b(&verdict, "stop");
                                let reason = clip(s(&verdict, "reason"), 200);
                                let ev = json!({
                                    "e": "loop_review",
                                    "cmd": clip(&cmd, 120),
                                    "count": count,
                                    "stop": stop,
                                    "reason": reason,
                                    "at": iso(),
                                });
                                let _ = append(run.join("events.jsonl"), &format!("{ev}\n"));
                                let run_id = crate::events::run_id_for(&run, &meta);
                                let repo = crate::events::repo_for_meta(&meta);
                                crate::events::emit(
                                    &run_id,
                                    &repo,
                                    "loop_review",
                                    json!({
                                        "name": s(&meta, "name"),
                                        "cmd": clip(&cmd, 120),
                                        "count": count,
                                        "stop": stop,
                                        "reason": reason,
                                    }),
                                );
                            }
                        }
                    }
                }
            }
        }
        let _ = fs::remove_dir_all(&scratch);
    });
}

/// Harness-facing protocol version; bump when docs/delegate-protocol.md §2 breaks.
pub const PROTOCOL: u32 = 1;

/// The one colleague. Tiers choose its model, not another agent.
pub const AGENT: &str = "pi";
pub use crate::models::{THINKING, TIERS};
pub const ALL_TIERS: [&str; 3] = TIERS;

pub fn tier_model(config: &Value, tier: &str) -> String {
    tier_model_with(config, tier, &|_| 0)
}
pub fn tier_model_with(config: &Value, tier: &str, running: &dyn Fn(&str) -> usize) -> String {
    crate::models::pick(config, tier, None, &[], local_clock(), running)
        .map(|p| p.model)
        .unwrap_or_default()
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
/// Does one rule list entry match now? `days`/`hours` read the local clock, `maxActive`
/// counts this machine's runs on the rule's model; a plain string always matches.
pub fn rule_matches(rule: &Value, now: Clock, running: &dyn Fn(&str) -> usize) -> bool {
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
    let day = rule["days"]
        .as_str()
        .is_none_or(|days| parse_days(days).is_some_and(|set| set.contains(&now.weekday)));
    let time = rule["hours"].as_str().is_none_or(within);
    let room = rule["maxActive"]
        .as_u64()
        .is_none_or(|cap| (running(s(rule, "model")) as u64) < cap);
    day && time && room
}

#[cfg(test)]
pub fn resolve_tier(value: &Value, now: Clock, running: &dyn Fn(&str) -> usize) -> String {
    if let Some(model) = value.as_str() {
        return model.to_string();
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|rule| match rule {
            Value::String(model) => Some(model.clone()),
            _ => rule_matches(rule, now, running)
                .then(|| rule["model"].as_str().unwrap_or("").to_string()),
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
- 本机还有别的任务和开发服务在跑；结束进程用你启动时记下的 pid。
- Otherwise finish the work yourself; do not stop at a plan.
- After a context compaction, continue from the summary; do not restart or redo finished work.
- Answer in the brief's language: the conclusion first, then evidence (file:line) and what you did not verify.
";
pub const REVIEWER_COLLEAGUE: &str = "You are an independent read-only code reviewer running in a fresh context.
- Do not create, modify, or delete any files, and do not delegate subtasks.
- Acceptance checks are run independently in parallel by the Host (which handles and summarizes the results, and cancels the review automatically if slow acceptance fails). Do not run acceptance commands or poll/wait for writer or acceptance processes (e.g. via `ps`/`sleep`); judge directly from the read-only snapshot and diff.
- 开头先给一句明确结论（通过 / 需返工 / 需主控决定，或 PASS / REWORK / NEEDS_CONTROLLER），下面自然写核查依据与重点关注位置。
- 通过 / PASS: changes satisfy the brief with no blocking defects, regressions, vacuous tests, or out-of-scope edits.
- 需返工 / REWORK: concrete fixable issues exist; list each issue with evidence and required fix.
- 需主控决定 / NEEDS_CONTROLLER: a requirement ambiguity, architecture trade-off, out-of-authority change, or inability to verify requires the controller's decision.
- After a context compaction, continue from the summary; do not restart.
";

/// Outcome of classifying whether a colleague's answer contains a question for the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionKind {
    /// No pending question detected.
    None,
    /// Stop-to-ask (explicit opening signal, or natural question when no files changed):
    /// pause in `question` state and skip acceptance until `delegate reply`.
    Blocking,
    /// Files were edited and the opening did not signal stop-to-ask, but the reply contains a
    /// natural question: run acceptance and review as usual, and attach a factual note on the card.
    TrailingNote,
}

fn md_fence_marker(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let ch = trimmed.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let count = trimmed.chars().take_while(|&c| c == ch).count();
    (count >= 3).then_some((ch, count))
}

fn clause_has_negation(clause_prefix: &str) -> bool {
    const NEGATIONS: &[&str] = &[
        "没有",
        "不需要",
        "不需",
        "无需",
        "无须",
        "不必",
        "不用",
        "暂无",
        "并无",
        "不存在",
    ];
    if NEGATIONS.iter().any(|neg| clause_prefix.contains(neg)) {
        return true;
    }
    let mut search = 0;
    while let Some(rel) = clause_prefix[search..].find('无') {
        let idx = search + rel;
        let after = &clause_prefix[idx + '无'.len_utf8()..];
        let next_ch = after.chars().next();
        if !matches!(
            next_ch,
            Some('法' | '论' | '从' | '权' | '效' | '线' | '限' | '视' | '意' | '关')
        ) {
            return true;
        }
        search = idx + '无'.len_utf8();
    }
    false
}

fn contains_positive_zh(haystack: &str, phrase: &str) -> bool {
    let mut search_from = 0usize;
    while let Some(rel) = haystack[search_from..].find(phrase) {
        let idx = search_from + rel;
        let prefix = &haystack[..idx];
        let prev_char = prefix.chars().next_back();
        let clause_prefix = match prefix.rfind([
            '，', ',', '；', ';', '。', '：', ':', '、', '！', '!', '？', '?',
        ]) {
            Some(sep) => {
                let sep_len = prefix[sep..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(1);
                &prefix[sep + sep_len..]
            }
            None => prefix,
        };
        if !matches!(prev_char, Some('无' | '不' | '未' | '非' | '没'))
            && !clause_has_negation(clause_prefix)
        {
            return true;
        }
        search_from = idx + phrase.len();
    }
    false
}

fn contains_positive_en(lower_haystack: &str, phrase: &str) -> bool {
    let mut search_from = 0usize;
    while let Some(rel) = lower_haystack[search_from..].find(phrase) {
        let idx = search_from + rel;
        let prefix = lower_haystack[..idx].trim_end();
        let negated = prefix.ends_with("no")
            || prefix.ends_with("not")
            || prefix.ends_with("don't")
            || prefix.ends_with("without")
            || prefix.ends_with("never");
        if !negated {
            return true;
        }
        search_from = idx + phrase.len();
    }
    false
}

fn strip_question_decor(line: &str) -> &str {
    line.trim().trim_matches([
        '#', '*', '_', '`', '>', '-', '•', '[', ']', '【', '】', '(', ')', '（', '）', ' ',
    ])
}

fn has_opening_stop_phrase(line: &str) -> bool {
    let stripped = strip_question_decor(line);
    if stripped.is_empty() {
        return false;
    }
    const ZH_STOP: &[&str] = &[
        "需要主控",
        "需主控",
        "暂停等主控",
        "停工等主控",
        "向主控提问",
    ];
    if ZH_STOP.iter().any(|p| contains_positive_zh(stripped, p)) {
        return true;
    }
    let lower = stripped.to_ascii_lowercase();
    const EN_STOP: &[&str] = &[
        "needs a decision",
        "need a decision",
        "needs controller",
        "need controller",
    ];
    EN_STOP.iter().any(|p| contains_positive_en(&lower, p))
}

fn has_natural_question_phrase(line: &str) -> bool {
    if has_opening_stop_phrase(line) {
        return true;
    }
    let stripped = strip_question_decor(line);
    if stripped.is_empty() {
        return false;
    }
    const ZH_PHRASES: &[&str] = &[
        "请主控",
        "待主控",
        "请拍板",
        "待拍板",
        "请确认",
        "需要确认",
        "需确认",
        "请决定",
        "待决定",
        "开工前确认",
        "开工前请",
    ];
    if ZH_PHRASES.iter().any(|p| contains_positive_zh(stripped, p)) {
        return true;
    }
    let lower = stripped.to_ascii_lowercase();
    const EN_PHRASES: &[&str] = &["please confirm", "please decide", "controller decision"];
    EN_PHRASES.iter().any(|p| contains_positive_en(&lower, p))
}

fn is_non_heading_question_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return false;
    }
    let body = trimmed
        .trim_end_matches(['”', '"', '\'', '」', '』', ')', '）', '*', '`'])
        .trim_end();
    if !(body.ends_with('？') || body.ends_with('?')) {
        return false;
    }
    let last_token = body.split_whitespace().next_back().unwrap_or(body);
    !(last_token.contains("://") && body.ends_with('?'))
}

/// Explicit stop-to-ask signal within the first 3 non-empty lines (skipping fenced code blocks).
/// Used when files have already been edited so trailing casual questions never bypass `accept`.
pub fn has_opening_stop_question(answer: &str) -> bool {
    let mut fence: Option<(char, usize)> = None;
    let mut seen = 0usize;
    for line in answer.lines() {
        if let Some((fc, flen)) = fence {
            if let Some((c, len)) = md_fence_marker(line) {
                if c == fc && len >= flen && line.trim_start()[len..].trim().is_empty() {
                    fence = None;
                }
            }
            continue;
        } else if let Some(m) = md_fence_marker(line) {
            fence = Some(m);
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "---" || trimmed == "***" {
            continue;
        }
        seen += 1;
        if has_opening_stop_phrase(trimmed) {
            return true;
        }
        if seen >= 3 {
            break;
        }
    }
    false
}

/// Relaxed natural-language question detection across the opening and closing paragraphs
/// (skipping fenced code blocks). Used when zero files were modified (including read-only scouts)
/// and for attaching a non-blocking factual note when files were edited.
pub fn has_natural_question(answer: &str) -> bool {
    if has_opening_stop_question(answer) {
        return true;
    }
    let mut fence: Option<(char, usize)> = None;
    let mut paragraphs: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();

    for line in answer.lines() {
        if let Some((fc, flen)) = fence {
            if let Some((c, len)) = md_fence_marker(line) {
                if c == fc && len >= flen && line.trim_start()[len..].trim().is_empty() {
                    fence = None;
                }
            }
            continue;
        } else if let Some(m) = md_fence_marker(line) {
            fence = Some(m);
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "---" || trimmed == "***" {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
        } else {
            current.push(trimmed);
        }
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    if paragraphs.is_empty() {
        return false;
    }

    let mut opening: Vec<&str> = Vec::new();
    for (p_idx, para) in paragraphs.iter().enumerate() {
        if p_idx > 0 && opening.iter().any(|l| !l.starts_with('#')) {
            break;
        }
        for &line in para {
            if p_idx > 0 && line.starts_with('#') && !opening.is_empty() {
                break;
            }
            opening.push(line);
            if opening.len() >= 6 {
                break;
            }
        }
        if opening.len() >= 6 {
            break;
        }
    }
    if opening
        .iter()
        .any(|l| has_natural_question_phrase(l) || is_non_heading_question_line(l))
    {
        return true;
    }
    if paragraphs.len() > 1 {
        if let Some(last) = paragraphs.last() {
            if last
                .iter()
                .any(|l| has_natural_question_phrase(l) || is_non_heading_question_line(l))
            {
                return true;
            }
        }
    }
    false
}

/// Classify whether a colleague's answer should pause as a blocking question or only carry a
/// non-blocking factual note, conditioned on whether the workspace had file changes.
pub fn classify_question(answer: &str, changed_empty: bool) -> QuestionKind {
    if changed_empty {
        if has_natural_question(answer) {
            QuestionKind::Blocking
        } else {
            QuestionKind::None
        }
    } else if has_opening_stop_question(answer) {
        QuestionKind::Blocking
    } else if has_natural_question(answer) {
        QuestionKind::TrailingNote
    } else {
        QuestionKind::None
    }
}

fn binary_cache_sig(bin: &Path, include_pi_models_cfg: bool) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let canon = fs::canonicalize(bin).unwrap_or_else(|_| bin.to_path_buf());
    let meta = fs::metadata(&canon).ok()?;
    let mut sig = format!(
        "{}|{}.{}:{}",
        canon.display(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.len()
    );
    // Small wrapper scripts (e.g. test stubs or shims <= 64 KiB) may be rewritten within the same
    // filesystem mtime quantum; hash their bytes to prevent stale cache hits.
    if meta.len() <= 65_536 {
        if let Ok(bytes) = fs::read(&canon) {
            let hash = sha1_smol::Sha1::from(&bytes).digest().to_string();
            sig.push('|');
            sig.push_str(&hash[..16]);
        }
    }
    if include_pi_models_cfg {
        let cfg = home().join(".pi/agent/models.json");
        if let Ok(cm) = fs::metadata(&cfg) {
            sig.push_str(&format!("|cfg:{}.{}:{}", cm.mtime(), cm.mtime_nsec(), cm.len()));
        }
    }
    Some(sig)
}

fn probe_cache_path() -> PathBuf {
    cache_dir().join("agent-probe-cache.json")
}

fn read_probe_cache(section: &str, key: &str, expected_sig: &str) -> Option<Value> {
    let doc = json(probe_cache_path());
    let entry = doc.get(section)?.get(key)?;
    if s(entry, "sig") == expected_sig {
        entry.get("val").cloned()
    } else {
        None
    }
}

fn write_probe_cache(section: &str, key: &str, sig: &str, val: Value) {
    let path = probe_cache_path();
    let mut doc = json(&path);
    if !doc.is_object() {
        doc = json!({});
    }
    if !doc[section].is_object() {
        doc[section] = json!({});
    }
    doc[section][key] = json!({"sig": sig, "val": val, "at": iso()});
    let _ = write_json(&path, &doc);
}

/// Pi capability metadata (not the selectable universe), from the offline model table;
/// empty when Pi is missing or prints nothing usable. Cached by binary mtime.
pub fn pi_models() -> Vec<crate::models::PiModel> {
    let Some(bin) = agent_bin(AGENT) else {
        return vec![];
    };
    pi_models_for(&bin)
}
pub fn pi_models_for(bin: &Path) -> Vec<crate::models::PiModel> {
    let key = bin.to_string_lossy().into_owned();
    let sig = binary_cache_sig(bin, true);
    if let Some(ref expected) = sig {
        if let Some(cached) = read_probe_cache("piModels", &key, expected)
            .and_then(|v| v.as_str().map(str::to_string))
        {
            return crate::models::parse_pi_models(&cached);
        }
    }
    crate::locks::assert_not_holding_state_lock("pi_models");
    let out = std::process::Command::new(bin)
        .args(["--offline", "--list-models"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else { return vec![] };
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let parsed = crate::models::parse_pi_models(&stdout);
    if !parsed.is_empty() {
        if let Some(ref expected) = sig {
            write_probe_cache("piModels", &key, expected, json!(stdout));
        }
    }
    parsed
}

/// What a caller needs to configure the tiers itself in one step.
pub fn tiers_guide(config: &Value) -> String {
    let models = pi_models();
    let list = config["models"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(alias, id)| {
            let id = crate::models::alias_model(id).unwrap_or("");
            if let Some(m) = models.iter().find(|m| m.name == id) {
                format!(
                    "  {alias} → {id}  context {}  images {}  thinking {}",
                    m.context, m.images, m.thinking
                )
            } else {
                format!("  {alias} → {id}  capabilities unknown")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let unregistered = crate::models::unregistered_count(config, &models);
    format!(
        "三个用途档都要配置：scout 只读侦察/摘要/文案，review 只读严密审查/根因/第二意见，write 改代码带验收。\n\
         配置 models 别名表，tiers 里用别名或 provider/id，thinking 是独立字段（旧配置不兼容，请迁移旧档到三个用途档并拆出 thinking）。\n\
         Registered models / 已登记模型:\n{list}\nPi 里还有 {unregistered} 个模型未登记。\n\
         配置 / configure: {} tiers --alias fast=provider/fast --alias coder=provider/coder --scout fast --review coder --write coder --thinking high\n\
         thinking: {}",
        script().display(),
        THINKING.join("|")
    )
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
        "capacityDefaults": {"maxActive": 32, "maxHeavy": crate::lane::default_local_max_heavy(), "repoMaxActive": 0},
    })
}
pub fn agent_identity(agent: &str) -> Res<(PathBuf, Option<String>)> {
    let bin = agent_bin(agent).ok_or_else(|| format!("missing required tools: {agent}"))?;
    let key = bin.to_string_lossy().into_owned();
    let sig = binary_cache_sig(&bin, false);
    if let Some(ref expected) = sig {
        if let Some(cached) = read_probe_cache("agentIdentity", &key, expected) {
            let ver = cached.as_str().filter(|x| !x.is_empty()).map(str::to_string);
            return Ok((bin, ver));
        }
    }
    crate::locks::assert_not_holding_state_lock("agent_identity");
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
    if let (Some(ref expected), Some(ref ver)) = (&sig, &version) {
        write_probe_cache("agentIdentity", &key, expected, json!(ver));
    }
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
        "agent_settled" => {
            let mut out = vec![];
            if s(v, "outcome") != "completed" && !s(v, "error").is_empty() {
                out.push(json!({"e":"turn_error","detail":clip(s(v,"error"),300)}));
            }
            out.push(json!({"e":"settled"}));
            out
        }
        "result" if s(v, "outcome") == "completed" => vec![json!({"e":"settled"})],
        _ => vec![],
    }
}
pub(crate) fn stall_seconds(meta: &Value, key: &str) -> Option<f64> {
    let env_name = format!("STALL_{}", key.to_uppercase());
    let default = match key {
        "idle" => "30m",
        "accept" | "generate" => "10m",
        _ => "90m",
    };
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

fn command_pi_rpc(meta: &Value, session: Option<&str>) -> Vec<String> {
    let fork = s(meta, "fork");
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
        "rpc".into(),
    ]);
    for key in ["provider", "model", "thinking"] {
        if !s(meta, key).is_empty() {
            a.extend([format!("--{key}"), s(meta, key).into()]);
        }
    }
    if s(meta, "mode") == "read-only" {
        a.extend([
            "--tools".into(),
            if b(meta, "shell") {
                "read,grep,find,ls,bash".into()
            } else {
                "read,grep,find,ls".into()
            },
        ]);
    }
    if !s(meta, "dir").is_empty() {
        let role = Path::new(s(meta, "dir")).join("colleague.md");
        a.extend([
            "--append-system-prompt".into(),
            role.to_string_lossy().into(),
        ]);
    }
    a
}

/// Probe whether a `pi` executable (specifically `rpi` or an RPC-capable binary) supports
/// `--mode rpc` with `queue_message(deliverAs: "steer")`. Cached by binary signature
/// (`mtime`/`size`/content hash for small scripts). Only probes binaries whose `--version`
/// identifies `rpi` (or explicitly mentions `rpc`) so legacy/non-`rpi` binaries and test stubs
/// are never invoked with `--mode rpc`.
pub fn pi_supports_rpc_steer(bin: &Path, version: Option<&str>) -> bool {
    let Some(ver) = version else {
        return false;
    };
    let lower = ver.trim().to_ascii_lowercase();
    if !lower.starts_with("rpi ") && !lower.contains("rpc") {
        return false;
    }
    let key = bin.to_string_lossy().into_owned();
    let sig = binary_cache_sig(bin, false);
    if let Some(ref expected) = sig {
        if let Some(cached) =
            read_probe_cache("piRpcSteer", &key, expected).and_then(|v| v.as_bool())
        {
            return cached;
        }
    }
    crate::locks::assert_not_holding_state_lock("pi_supports_rpc_steer");
    let mut cmd = std::process::Command::new(bin);
    cmd.args([
        "--mode",
        "rpc",
        "--no-session",
        "--offline",
        "--no-tools",
        "--no-skills",
        "--no-context-files",
        "--no-extensions",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    group(&mut cmd);
    let supported = cmd.spawn().ok().is_some_and(|mut child| {
        if let Some(mut stdin) = child.stdin.take() {
            let payload = format!(
                "{}\n{}\n",
                json!({"id":"probe-steer","type":"queue_message","deliverAs":"steer","message":"probe"}),
                json!({"id":"probe-stop","type":"stop"})
            );
            let _ = stdin.write_all(payload.as_bytes());
            let _ = stdin.flush();
        }
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
                    return false;
                }
            }
        }
        child.wait_with_output().ok().is_some_and(|out| {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let mut has_ready_cmds = false;
            let mut steer_accepted = false;
            for line in stdout.lines() {
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if s(&v, "type") == "ready" {
                    let cmds = strings(&v["supportedCommands"]);
                    if cmds.iter().any(|c| c == "prompt")
                        && cmds.iter().any(|c| c == "queue_message")
                        && cmds.iter().any(|c| c == "get_queue")
                        && cmds.iter().any(|c| c == "clear_queue")
                    {
                        has_ready_cmds = true;
                    }
                } else if s(&v, "type") == "response"
                    && s(&v, "command") == "queue_message"
                    && s(&v, "id") == "probe-steer"
                    && b(&v, "success")
                {
                    let q = s(&v["data"], "queue");
                    if matches!(q, "steer" | "steering") {
                        steer_accepted = true;
                    }
                }
            }
            has_ready_cmds && steer_accepted
        })
    });
    if let Some(ref expected) = sig {
        write_probe_cache("piRpcSteer", &key, expected, json!(supported));
    }
    supported
}

struct PiRpcShared {
    stdin: Option<std::process::ChildStdin>,
    closed: bool,
    prompt_sent: bool,
    /// True from `agent_settled` until either the next `prompt` is sent or `stop` closes the
    /// channel; blocks `steer_pump` so no `queue_message` can race with `get_queue` / `stop`.
    settling: bool,
    next_req: u64,
    /// `queue_message` requests sent on stdin whose response has not yet arrived (`req_id -> items`).
    in_flight_steers: std::collections::HashMap<String, Vec<crate::pending_reply::AppendItem>>,
    /// `queue_message` requests accepted by RPC (`entryId -> items`), retained until `get_queue`
    /// after `agent_settled` proves they were consumed rather than left queued after turn end.
    acked_steers: std::collections::HashMap<String, Vec<crate::pending_reply::AppendItem>>,
    /// Active post-`agent_settled` queue check: `(get_queue_req_id, is_ok_turn)`.
    pending_queue_check: Option<(String, bool)>,
}

fn send_pi_rpc_stop(st: &mut PiRpcShared, rpc_input_log: &Path) {
    if let Some(ref mut stdin_pipe) = st.stdin {
        let stop_line = format!("{}\n", json!({"id": "stop", "type": "stop"}));
        let _ = append(rpc_input_log, &stop_line);
        let _ = stdin_pipe.write_all(stop_line.as_bytes());
        let _ = stdin_pipe.flush();
    }
    st.stdin = None;
    st.closed = true;
}

#[allow(clippy::too_many_arguments)]
fn finalize_pi_rpc_turn(
    shared: &Arc<Mutex<PiRpcShared>>,
    run: &Path,
    attempt: i64,
    rpc_input_log: &Path,
    log: &mut fs::File,
    current_round: &mut usize,
    answer: &str,
    is_ok_turn: bool,
    unconsumed: Vec<crate::pending_reply::AppendItem>,
) -> bool {
    let mut st = shared.lock().unwrap();
    if st.stdin.is_none() || st.closed {
        if !unconsumed.is_empty() {
            drop(st);
            let _ = crate::pending_reply::revert_to_fallback(run, &unconsumed);
        }
        return false;
    }
    if !is_ok_turn {
        send_pi_rpc_stop(&mut st, rpc_input_log);
        drop(st);
        if !unconsumed.is_empty() {
            let _ = crate::pending_reply::revert_steer_to_in_run(run, &unconsumed);
        }
        return false;
    }
    let take_res = if unconsumed.is_empty() {
        crate::pending_reply::take_or_close_in_run(run, *current_round + 1)
    } else {
        crate::pending_reply::take_next_round_with_requeued(run, &unconsumed, *current_round + 1)
    };
    let batch = match take_res {
        Ok(crate::pending_reply::TakeInRun::Messages(mut items)) => {
            let (img_items, text_items): (Vec<_>, Vec<_>) = items
                .drain(..)
                .partition(|it| !it.images.is_empty());
            if !img_items.is_empty() {
                let _ = crate::pending_reply::promote_items_to_fallback(run, &img_items);
            }
            if text_items.is_empty() {
                match crate::pending_reply::take_or_close_in_run(run, *current_round + 1) {
                    Ok(crate::pending_reply::TakeInRun::Messages(more)) => more,
                    _ => Vec::new(),
                }
            } else {
                text_items
            }
        }
        Ok(crate::pending_reply::TakeInRun::Closed) | Err(_) => Vec::new(),
    };
    if batch.is_empty() {
        if *current_round > 1 && !answer.trim().is_empty() {
            let formatted = format!("{}\n", answer.trim_end_matches('\n'));
            let _ = write(
                run.join(format!("round-{current_round}-result.md")),
                &formatted,
            );
        }
        send_pi_rpc_stop(&mut st, rpc_input_log);
        return false;
    }
    if !answer.trim().is_empty() {
        let formatted = format!("{}\n", answer.trim_end_matches('\n'));
        let _ = write(
            run.join(format!("round-{current_round}-result.md")),
            &formatted,
        );
    }
    *current_round += 1;
    st.next_req += 1;
    let req_id = format!("p-{}", st.next_req);
    let merged_text = crate::pending_reply::format_merged_followup(&batch);
    let line = format!(
        "{}\n",
        json!({
            "id": req_id,
            "type": "prompt",
            "message": merged_text,
        })
    );
    let _ = append(rpc_input_log, &line);
    let wrote_ok = if let Some(ref mut stdin_pipe) = st.stdin {
        stdin_pipe.write_all(line.as_bytes()).is_ok() && stdin_pipe.flush().is_ok()
    } else {
        false
    };
    if wrote_ok {
        st.settling = false;
        drop(st);
        let seqs: Vec<u64> = batch.iter().map(|it| it.seq).collect();
        let _ = writeln!(
            log,
            "{}",
            json!({
                "e": "append_delivered",
                "round": *current_round,
                "seqs": seqs,
                "count": batch.len(),
                "mode": "in_run",
                "attempt": attempt,
                "at": iso(),
            })
        );
        let _ = log.flush();
        crate::index::upsert_run(run);
        crate::board::refresh_for_run(run);
        true
    } else {
        st.stdin = None;
        st.closed = true;
        drop(st);
        let _ = crate::pending_reply::revert_to_fallback(run, &batch);
        false
    }
}

fn pump_pi_rpc_steer_once(
    shared: &Arc<Mutex<PiRpcShared>>,
    run: &Path,
    attempt: i64,
    rpc_input_log: &Path,
) -> bool {
    let mut st = shared.lock().unwrap();
    if st.closed || st.stdin.is_none() {
        return false;
    }
    if !st.prompt_sent || st.settling {
        return true;
    }
    let Ok(batch) = crate::pending_reply::take_pending_in_run_for_steer(run) else {
        return true;
    };
    if batch.is_empty() {
        return true;
    }
    let (img_items, text_items): (Vec<_>, Vec<_>) =
        batch.into_iter().partition(|it| !it.images.is_empty());
    if !img_items.is_empty() {
        let _ = crate::pending_reply::promote_items_to_fallback(run, &img_items);
    }
    if text_items.is_empty() {
        drop(st);
        crate::index::upsert_run(run);
        crate::board::refresh_for_run(run);
        return true;
    }
    st.next_req += 1;
    let req_id = format!("steer-{}", st.next_req);
    let merged_text = crate::pending_reply::format_merged_followup(&text_items);
    let line = format!(
        "{}\n",
        json!({
            "id": req_id,
            "type": "queue_message",
            "deliverAs": "steer",
            "message": merged_text,
        })
    );
    let _ = append(rpc_input_log, &line);
    let wrote_ok = if let Some(ref mut stdin_pipe) = st.stdin {
        stdin_pipe.write_all(line.as_bytes()).is_ok() && stdin_pipe.flush().is_ok()
    } else {
        false
    };
    if wrote_ok {
        let seqs: Vec<u64> = text_items.iter().map(|it| it.seq).collect();
        let count = text_items.len();
        st.in_flight_steers.insert(req_id, text_items);
        drop(st);
        let _ = append(
            run.join("events.jsonl"),
            &format!(
                "{}\n",
                json!({
                    "e": "append_delivered",
                    "mode": "steer",
                    "seqs": seqs,
                    "count": count,
                    "attempt": attempt,
                    "at": iso(),
                })
            ),
        );
        crate::index::upsert_run(run);
        crate::board::refresh_for_run(run);
        true
    } else {
        st.stdin = None;
        st.closed = true;
        drop(st);
        let _ = crate::pending_reply::revert_to_fallback(run, &text_items);
        false
    }
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
    if s(meta, "mode") == "read-only" {
        a.extend([
            "--tools".into(),
            if b(meta, "shell") {
                "read,grep,find,ls,bash".into()
            } else {
                "read,grep,find,ls".into()
            },
        ]);
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
/// one subdirectory per working directory. Sidecars such as rpi's
/// `<session>.jsonl.progress.jsonl` are not sessions.
fn is_session(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
        && path
            .file_stem()
            .is_some_and(|s| !s.to_string_lossy().contains(".jsonl"))
}
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
                    .filter(|x| is_session(x)),
            );
        } else if is_session(&path) {
            files.push(path);
        }
    }
    files.sort();
    files
}
pub fn session_file(dir: &str, id: &str) -> Option<PathBuf> {
    // 6.12.2 and earlier recorded a sidecar's id (`<uuid>.jsonl.progress`); the uuid
    // has no dot, so its first segment names the real session.
    let id = id.split('.').next().unwrap_or(id);
    session_files(Path::new(dir)).into_iter().rfind(|x| {
        x.file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with(&format!("_{id}.jsonl")))
    })
}
/// What a reply or resume continues: Pi's saved session file, or agy's conversation id.
pub fn continuation_target(meta: &Value, id: &str) -> Option<String> {
    if crate::agy::is_agy(s(meta, "model")) {
        Some(id.to_string()).filter(|id| !id.is_empty())
    } else {
        session_file(s(meta, "sessionDir"), id).map(|p| p.to_string_lossy().into())
    }
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
    run_agent_cancellable(meta, run, attempt, holder, grace_out, None)
}
pub fn run_agent_cancellable(
    meta: &Value,
    run: &Path,
    attempt: i64,
    holder: Arc<AtomicI32>,
    grace_out: Arc<std::sync::Mutex<Option<f64>>>,
    cancel: Option<Arc<AtomicBool>>,
) -> Res<(String, String)> {
    if cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) {
        return Ok(("cancelled".into(), String::new()));
    }
    if ![AGENT, crate::agy::BIN].contains(&s(meta, "agent")) {
        return Err(format!(
            "{} runs are no longer supported; delegate runs Pi or agy",
            s(meta, "agent")
        ));
    }
    let on_agy = crate::agy::is_agy(s(meta, "model"));
    let uuid = format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        now_ns() as u32,
        std::process::id() as u16,
        0u16,
        0u16,
        (now_ns() >> 8) as u64 & 0xffffffffffff
    );
    // agy names its own conversation (reported in its first event); a fork there is the
    // conversation id to continue.
    let session = if !s(meta, "fork").is_empty() || on_agy {
        None
    } else {
        Some(uuid)
    };
    let session_dir = Path::new(s(meta, "sessionDir"));
    let known = session_files(session_dir);
    let addendum_text = if b(meta, "isReviewer") {
        REVIEWER_COLLEAGUE
    } else {
        COLLEAGUE
    };
    write(Path::new(s(meta, "dir")).join("colleague.md"), addendum_text)?;
    let allow_append =
        !b(meta, "isReviewer") && !b(meta, "isRework") && !b(meta, "noAppend");
    let has_images = meta["images"].as_array().is_some_and(|a| !a.is_empty());
    let pi_bin_path = if !s(meta, "agentBin").is_empty() {
        Some(PathBuf::from(s(meta, "agentBin")))
    } else {
        agent_bin(AGENT)
    };
    let pi_ver = Some(s(meta, "agentVersion")).filter(|v| !v.is_empty());
    let on_pi_rpc = !on_agy
        && allow_append
        && !has_images
        && pi_bin_path
            .as_deref()
            .is_some_and(|p| pi_supports_rpc_steer(p, pi_ver));
    let mut args = if on_agy {
        crate::agy::command(meta, Some(s(meta, "fork")))
    } else if on_pi_rpc {
        command_pi_rpc(meta, session.as_deref())
    } else {
        command_pi(meta, session.as_deref())
    };
    if !s(meta, "agentBin").is_empty() {
        args[0] = s(meta, "agentBin").to_string();
    }
    let prompt_path = meta["agentPromptFile"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| run.join("prompt.md"));
    let agy_input_log = run.join(format!("agy-input-{attempt}.jsonl"));
    let pi_rpc_input_log = run.join(format!("pi-rpc-input-{attempt}.jsonl"));
    let (prompt_file_stdin, initial_agy_line, initial_pi_rpc_line) = if on_agy {
        // agy takes the prompt as one stream-json message; the addendum Pi gets as a
        // system-prompt append opens a new conversation's first message.
        let task = fs::read_to_string(&prompt_path).map_err(|e| e.to_string())?;
        let addendum = if s(meta, "fork").is_empty() { addendum_text } else { "" };
        let images = meta["images"].as_array().cloned().unwrap_or_default();
        let line = crate::agy::input(addendum, &task, &images);
        write(&agy_input_log, &line)?;
        if allow_append {
            (None, Some(line), None)
        } else {
            let f = OpenOptions::new()
                .read(true)
                .open(&agy_input_log)
                .map_err(|e| e.to_string())?;
            (Some(f), None, None)
        }
    } else if on_pi_rpc {
        let task = fs::read_to_string(&prompt_path).map_err(|e| e.to_string())?;
        let line = format!(
            "{}\n",
            json!({"id": "p-1", "type": "prompt", "message": task})
        );
        write(&pi_rpc_input_log, &line)?;
        (None, None, Some(line))
    } else {
        let f = OpenOptions::new()
            .read(true)
            .open(&prompt_path)
            .map_err(|e| e.to_string())?;
        (Some(f), None, None)
    };
    if on_pi_rpc {
        touch(run.join(crate::pending_reply::PI_RPC_STEER_MARKER));
    }
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
    let (agy_account, mut agy_overlay) = if on_agy {
        let is_reviewer = b(meta, "isReviewer");
        let is_extra_reviewer = b(meta, "isExtraReviewer");
        let fresh_agy = b(meta, "freshAgySession");
        let is_handoff = b(meta, "isAgyHandoff");
        let preferred_acct = {
            let p = s(meta, "preferredAgyAccount");
            (!p.is_empty()).then_some(p)
        };
        let meta_path = if is_extra_reviewer {
            run.join("meta.json")
        } else if is_reviewer {
            let owner = s(meta, "ownerRunDir");
            if !owner.is_empty() {
                PathBuf::from(owner).join("meta.json")
            } else {
                run.join("meta.json")
            }
        } else {
            run.join("meta.json")
        };
        let (pin_key, switch_key) = if is_reviewer && !is_extra_reviewer {
            ("reviewAgyAccount", "reviewAgyAccountSwitch")
        } else {
            ("agyAccount", "agyAccountSwitch")
        };
        let pinned_id: Option<String> = if fresh_agy || preferred_acct.is_some() {
            None
        } else {
            let disk = json(&meta_path);
            let from_disk = s(&disk, pin_key);
            if !from_disk.is_empty() {
                Some(from_disk.to_string())
            } else {
                let from_meta = s(meta, pin_key);
                if !from_meta.is_empty() {
                    Some(from_meta.to_string())
                } else {
                    None
                }
            }
        };
        let prep = crate::agy_accounts::AgyCredOverlay::prepare_for_session_with_opts(
            pinned_id.as_deref(),
            s(meta, "model"),
            true,
            preferred_acct,
            Some(run),
        )?;
        let effective_switched = if prep.switched.is_some() {
            prep.switched.clone()
        } else if !is_handoff
            && !is_reviewer
            && !is_extra_reviewer
            && pinned_id.is_none()
            && preferred_acct.is_none()
            && crate::agy_accounts::is_auto_mode_enabled()
        {
            let parent_acct = s(meta, "parentAgyAccount");
            match (parent_acct, prep.account_id.as_deref()) {
                (from, Some(to)) if !from.is_empty() && from != to => {
                    Some(crate::agy_accounts::AgyAccountSwitch {
                        kind: "auto".to_string(),
                        from: from.to_string(),
                        to: to.to_string(),
                        reason: "新会话按 auto 选号".to_string(),
                        pool: crate::agy_accounts::classify_model_pool(s(meta, "model"))
                            .map(|p| p.key().to_string()),
                        from_quota: None,
                        to_quota: None,
                        cache_age_secs: None,
                    })
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some(ref acct) = prep.account_id {
            if meta_path.is_file() || is_extra_reviewer {
                let mut disk = if meta_path.is_file() {
                    json(&meta_path)
                } else {
                    json!({})
                };
                if disk.is_object() {
                    let mut dirty = false;
                    if s(&disk, pin_key) != acct.as_str() {
                        disk[pin_key] = json!(acct);
                        dirty = true;
                    }
                    if !is_handoff {
                        if let Some(ref sw) = effective_switched {
                            let mut sw_obj = json!({
                                "kind": sw.kind,
                                "from": sw.from,
                                "to": sw.to,
                                "reason": sw.reason,
                                "at": iso(),
                            });
                            if let Some(ref pool) = sw.pool {
                                sw_obj["pool"] = json!(pool);
                            }
                            disk[switch_key] = sw_obj;
                            dirty = true;
                        } else if fresh_agy && disk.get(switch_key).is_some() {
                            if let Some(obj) = disk.as_object_mut() {
                                obj.remove(switch_key);
                                dirty = true;
                            }
                        }
                    }
                    if dirty {
                        let _ = write_json(&meta_path, &disk);
                    }
                }
            }
        }
        if !is_handoff {
            if let Some(ref sw) = effective_switched {
                let role = if is_extra_reviewer {
                    "extra_reviewer"
                } else if is_reviewer {
                    "reviewer"
                } else {
                    "author"
                };
                let ev_at = iso();
                if sw.kind == "auto" {
                    let ev_run_id = crate::events::run_id_for(run, meta);
                    let ev_repo = crate::events::repo_for_meta(meta);
                    let mut payload = json!({
                        "role": role,
                        "from": sw.from,
                        "to": sw.to,
                        "model": s(meta, "model"),
                        "reason": sw.reason,
                    });
                    if let Some(ref pool) = sw.pool {
                        payload["pool"] = json!(pool);
                    }
                    if let Some(ref fq) = sw.from_quota {
                        payload["fromQuota"] = fq.clone();
                    }
                    if let Some(ref tq) = sw.to_quota {
                        payload["toQuota"] = tq.clone();
                    }
                    if let Some(age) = sw.cache_age_secs {
                        payload["cacheAgeSecs"] = json!(age);
                    }
                    let mut run_ev = payload.clone();
                    run_ev["e"] = json!("agy_account_auto");
                    run_ev["run"] = json!(&ev_run_id);
                    run_ev["at"] = json!(&ev_at);
                    let _ = writeln!(log, "{run_ev}");
                    crate::events::emit(&ev_run_id, &ev_repo, "agy_account_auto", payload);
                } else {
                    let _ = writeln!(
                        log,
                        "{}",
                        json!({
                            "e": "agy_account_switch",
                            "kind": sw.kind,
                            "from": sw.from,
                            "to": sw.to,
                            "reason": sw.reason,
                            "role": role,
                            "at": ev_at,
                        })
                    );
                }
            }
        }
        (prep.account_id, prep.overlay)
    } else {
        (None, None)
    };
    let mut c =
        crate::cleanup::scoped_command(run, &format!("agent-{attempt}"), &args[0], &args[1..]);
    c.current_dir(s(meta, "workdir"))
        .stdin(match prompt_file_stdin {
            Some(f) => Stdio::from(f),
            None => Stdio::piped(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr));
    clean_env(&mut c, &meta["env"]);
    if let Some(ref overlay) = agy_overlay {
        let _ = write(
            run.join("agy_home"),
            overlay.overlay_home().to_string_lossy().as_ref(),
        );
        overlay.apply_env(&mut c);
    }
    c.env(
        "CARGO_BUILD_JOBS",
        crate::lane::local_heavy_jobs(Some(run)).to_string(),
    );
    crate::deny::inject(&mut c, meta, run)?;
    crate::source_guard::inject(&mut c, meta, run)?;
    c.env("DELEGATE_AGENT", s(meta, "agent"))
        .env("DELEGATE_RUN_DIR", s(meta, "dir"));
    c.env("PI_DELEGATE_ACTIVE", "1");
    group(&mut c);
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let mut agy_stdin = if on_agy { child.stdin.take() } else { None };
    if let (Some(line), Some(ref mut stdin_pipe)) = (initial_agy_line, agy_stdin.as_mut()) {
        if stdin_pipe.write_all(line.as_bytes()).is_err() || stdin_pipe.flush().is_err() {
            agy_stdin = None;
        }
    }
    let pi_rpc_state = if on_pi_rpc {
        Some(Arc::new(Mutex::new(PiRpcShared {
            stdin: child.stdin.take(),
            closed: false,
            prompt_sent: false,
            settling: false,
            next_req: 1,
            in_flight_steers: std::collections::HashMap::new(),
            acked_steers: std::collections::HashMap::new(),
            pending_queue_check: None,
        })))
    } else {
        None
    };
    let steer_stop = Arc::new(AtomicBool::new(false));
    let steer_pump = pi_rpc_state.as_ref().map(|shared| {
        let shared = shared.clone();
        let stop_flag = steer_stop.clone();
        let pump_run = run.to_path_buf();
        let pump_log = pi_rpc_input_log.clone();
        std::thread::spawn(move || {
            let watcher = crate::inotify::StateWatcher::watch(&pump_run);
            while !stop_flag.load(Ordering::SeqCst) {
                if !pump_pi_rpc_steer_once(&shared, &pump_run, attempt, &pump_log) {
                    break;
                }
                crate::inotify::wait_or_sleep(watcher.as_ref(), Duration::from_millis(150));
            }
        })
    });
    let pid = child.id() as i32;
    holder.store(pid, Ordering::SeqCst);
    if cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) {
        kill_group(pid, 1.0);
    }
    write(run.join("agent.pid"), pid.to_string())?;
    let mut usage_tracker =
        crate::usage::LiveTracker::start_with_account(meta, run, attempt, agy_account.clone());
    let mut attempt_turns: Vec<Value> = Vec::new();
    let timed = Arc::new(AtomicBool::new(false));
    let watch = Arc::new((
        Mutex::new(WatchState {
            finished: false,
            last: Instant::now(),
            commands: 0,
            command_started: None,
            command_queued: 0.0,
            command_output_at: None,
            command_output_queued: 0.0,
            command_remote_log: file_fingerprint(&run.join("remote-output.log")),
            command_stderr_log: file_fingerprint(&run.join("stderr.log")),
            command: String::new(),
            idle_warned_at: None,
            command_warned_at: None,
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
    let (watcher_state, t, go) = (watch.clone(), timed.clone(), grace_out.clone());
    let runpath = run.to_path_buf();
    let source_meta = meta.clone();
    let watcher_run_id = crate::events::run_id_for(run, meta);
    let watcher_repo = crate::events::repo_for_meta(meta);
    let watcher_name = s(meta, "name").to_string();
    let watcher_cancel = cancel.clone();
    let test_clock: Option<PathBuf> = if cfg!(debug_assertions) {
        std::env::var("DELEGATE_TEST_AGENT_CLOCK")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    } else {
        None
    };
    let watcher = std::thread::spawn(move || {
        let (mutex, changed) = &*watcher_state;
        let mut state = mutex.lock().unwrap();
        let remote_log_path = runpath.join("remote-output.log");
        let stderr_log_path = runpath.join("stderr.log");
        let mut last_clock = 0.0_f64;
        let mut grace_marked = false;
        loop {
            if state.finished {
                break;
            }
            crate::source_watch::observe(&source_meta, &runpath);
            let queued = lane::queued_seconds(&runpath);
            let elapsed = if let Some(ref clock_path) = test_clock {
                if let Some(secs) = fs::read_to_string(clock_path)
                    .ok()
                    .and_then(|s| s.trim().parse::<f64>().ok())
                {
                    last_clock = secs;
                }
                last_clock
            } else {
                start.elapsed().as_secs_f64()
            };
            let spent = elapsed - queued;
            let idle = state.last.elapsed().as_secs_f64();
            if state.command_started.is_some() {
                let remote_grew =
                    file_output_advanced(&remote_log_path, &mut state.command_remote_log);
                let stderr_grew =
                    file_output_advanced(&stderr_log_path, &mut state.command_stderr_log);
                if remote_grew || stderr_grew {
                    state.command_output_at = Some(Instant::now());
                    state.command_output_queued = queued;
                    if state.command_warned_at.take().is_some() {
                        clear_stall_warning_for_kind(&runpath, "command");
                    }
                }
            }
            let command_idle = state
                .command_output_at
                .or(state.command_started)
                .map(|at| {
                    (at.elapsed().as_secs_f64() - (queued - state.command_output_queued).max(0.0))
                        .max(0.0)
                })
                .unwrap_or(0.0);
            let command_age = state.command_started.map(|at| {
                (at.elapsed().as_secs_f64() - (queued - state.command_queued).max(0.0)).max(0.0)
            });
            if let Some(limit) = stall_idle {
                let due = state.commands == 0
                    && idle >= limit
                    && state
                        .idle_warned_at
                        .is_none_or(|w| w.elapsed().as_secs_f64() >= limit);
                if due {
                    let elapsed = idle.round() as u64;
                    let at = iso();
                    let detail = format!("已 {elapsed} 秒无新动作");
                    let warning = json!({
                        "kind": "idle",
                        "seconds": elapsed,
                        "limitSeconds": limit,
                        "detail": detail,
                        "at": at,
                    });
                    write_stall_warning_for_kind(&runpath, "idle", &warning);
                    if let Ok(mut log) = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(runpath.join("events.jsonl"))
                    {
                        let _ = writeln!(
                            log,
                            "{}",
                            json!({"e":"stall_warning","kind":"idle","seconds":elapsed,"limitSeconds":limit,"detail":detail,"at":at})
                        );
                    }
                    crate::events::emit(
                        &watcher_run_id,
                        &watcher_repo,
                        "stall_warning",
                        json!({
                            "name": watcher_name,
                            "stallKind": "idle",
                            "seconds": elapsed,
                            "limitSeconds": limit,
                            "detail": detail,
                        }),
                    );
                    state.idle_warned_at = Some(Instant::now());
                }
            }
            if let Some(limit) = stall_command {
                let due = state.command_started.is_some()
                    && command_idle >= limit
                    && state
                        .command_warned_at
                        .is_none_or(|w| w.elapsed().as_secs_f64() >= limit);
                if due {
                    let elapsed = command_age.unwrap_or(0.0).round() as u64;
                    let silent = command_idle.round() as u64;
                    let at = iso();
                    let cmd = state.command.clone();
                    let detail = if elapsed > silent {
                        format!("命令 {} 已运行 {elapsed} 秒（{silent} 秒无新输出）", clip(&cmd, 80))
                    } else {
                        format!("命令 {} 已运行 {elapsed} 秒无新输出", clip(&cmd, 80))
                    };
                    let warning = json!({
                        "kind": "command",
                        "cmd": cmd,
                        "seconds": elapsed,
                        "silentSeconds": silent,
                        "limitSeconds": limit,
                        "detail": detail,
                        "at": at,
                    });
                    write_stall_warning_for_kind(&runpath, "command", &warning);
                    if let Ok(mut log) = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(runpath.join("events.jsonl"))
                    {
                        let _ = writeln!(
                            log,
                            "{}",
                            json!({"e":"stall_warning","kind":"command","cmd":cmd,"seconds":elapsed,"silentSeconds":silent,"limitSeconds":limit,"detail":detail,"at":at})
                        );
                    }
                    crate::events::emit(
                        &watcher_run_id,
                        &watcher_repo,
                        "stall_warning",
                        json!({
                            "name": watcher_name,
                            "stallKind": "command",
                            "cmd": cmd,
                            "seconds": elapsed,
                            "silentSeconds": silent,
                            "limitSeconds": limit,
                            "detail": detail,
                        }),
                    );
                    state.command_warned_at = Some(Instant::now());
                }
            }
            if let Some(soft) = soft {
                if spent >= soft {
                    if spent < hard.unwrap_or(soft) && (state.commands > 0 || idle < 120.0) {
                        if let Ok(mut x) = go.lock() {
                            *x = Some(((spent - soft) * 10.0).round() / 10.0);
                        }
                        if test_clock.is_some() && !grace_marked {
                            let _ = fs::write(runpath.join(".test-grace-entered"), "");
                            grace_marked = true;
                        }
                    } else {
                        t.store(true, Ordering::SeqCst);
                        crate::cleanup::record(&runpath, pid);
                        kill_group(pid, 5.0);
                        break;
                    }
                }
            }
            let mut delay: f64 = if source_meta["sourceStatusBaseline"].is_object() {
                2.0
            } else {
                60.0
            };
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
                    let rem = match state.idle_warned_at {
                        Some(w) => limit - w.elapsed().as_secs_f64(),
                        None => limit - idle,
                    };
                    delay = delay.min(rem.max(0.05));
                }
            }
            if let Some(limit) = stall_command {
                if state.command_started.is_some() {
                    let rem = match state.command_warned_at {
                        Some(w) => limit - w.elapsed().as_secs_f64(),
                        None => limit - command_idle,
                    };
                    delay = delay
                        .min(rem.max(0.05))
                        .min((limit / 4.0).clamp(0.05, 60.0));
                }
            }
            if watcher_cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) {
                crate::cleanup::record(&runpath, pid);
                kill_group(pid, 1.0);
                break;
            }
            if watcher_cancel.is_some() {
                delay = delay.min(0.05);
            }
            if test_clock.is_some() {
                delay = delay.min(0.02);
            }
            state = changed
                .wait_timeout(state, Duration::from_secs_f64(delay.max(0.001)))
                .unwrap()
                .0;
        }
    });
    let mut turn = Value::Null;
    let mut answer = String::new();
    let mut last_turn_error = String::new();
    let mut settled = false;
    let mut current_round = crate::runs::max_round_result_num(run) + 1;
    let mut agy_filter = crate::agy::Filter::default();
    let mut agy_session = String::new();
    let is_reviewer = b(meta, "isReviewer");
    let mut loop_detector = LoopDetector::new(loop_threshold(meta), is_reviewer);
    let mut loop_stopped = false;
    let loop_run_id = crate::events::run_id_for(run, meta);
    let loop_repo = crate::events::repo_for_meta(meta);
    let loop_name = s(meta, "name").to_string();
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
            if let Some(ref shared) = pi_rpc_state {
                let ev_type = s(&event, "type");
                if ev_type == "ready" {
                    let mut st = shared.lock().unwrap();
                    if !st.prompt_sent {
                        if let (Some(ref line), Some(ref mut stdin_pipe)) =
                            (&initial_pi_rpc_line, st.stdin.as_mut())
                        {
                            if stdin_pipe.write_all(line.as_bytes()).is_ok()
                                && stdin_pipe.flush().is_ok()
                            {
                                st.prompt_sent = true;
                            } else {
                                st.stdin = None;
                                st.closed = true;
                            }
                        }
                    }
                    drop(st);
                    // Drain any appends that arrived while queued before the first turn started.
                    pump_pi_rpc_steer_once(shared, run, attempt, &pi_rpc_input_log);
                } else if ev_type == "response" && s(&event, "command") == "queue_message" {
                    let req_id = s(&event, "id");
                    let ok = b(&event, "success");
                    let entry_id = {
                        let e = s(&event["data"], "entryId");
                        if !e.is_empty() {
                            e.to_string()
                        } else {
                            let r = s(&event["result"], "entryId");
                            if !r.is_empty() {
                                r.to_string()
                            } else {
                                req_id.to_string()
                            }
                        }
                    };
                    let mut failed_batch = None;
                    {
                        let mut st = shared.lock().unwrap();
                        if let Some(batch) = st.in_flight_steers.remove(req_id) {
                            if ok {
                                st.acked_steers.insert(entry_id, batch);
                            } else {
                                failed_batch = Some(batch);
                            }
                        }
                    }
                    if let Some(batch) = failed_batch {
                        let _ = crate::pending_reply::revert_steer_to_in_run(run, &batch);
                    }
                } else if ev_type == "response" && s(&event, "command") == "get_queue" {
                    let resp_id = s(&event, "id");
                    let matched = {
                        let mut st = shared.lock().unwrap();
                        if st
                            .pending_queue_check
                            .as_ref()
                            .is_some_and(|(qid, _)| qid == resp_id)
                        {
                            let (_, is_ok_turn) = st.pending_queue_check.take().unwrap();
                            let data_obj = if event["data"].is_object() {
                                &event["data"]
                            } else {
                                &event["result"]
                            };
                            let items_arr = data_obj["items"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default();
                            let queued_ids: std::collections::HashSet<String> = items_arr
                                .iter()
                                .filter(|it| {
                                    let status = s(it, "status");
                                    status.is_empty() || status == "queued"
                                })
                                .map(|it| s(it, "entryId").to_string())
                                .filter(|id| !id.is_empty())
                                .collect();
                            let queued_count = data_obj["count"]
                                .as_u64()
                                .unwrap_or(queued_ids.len() as u64);
                            let mut unconsumed: Vec<crate::pending_reply::AppendItem> = Vec::new();
                            for (eid, batch) in st.acked_steers.drain() {
                                if queued_ids.contains(&eid)
                                    || (queued_count > 0 && queued_ids.is_empty())
                                {
                                    unconsumed.extend(batch);
                                }
                            }
                            for (_, batch) in st.in_flight_steers.drain() {
                                unconsumed.extend(batch);
                            }
                            unconsumed.sort_by_key(|it| it.seq);
                            let mut clear_failed = false;
                            if !unconsumed.is_empty() || queued_count > 0 {
                                st.next_req += 1;
                                let clear_line = format!(
                                    "{}\n",
                                    json!({"id": format!("clear-{}", st.next_req), "type": "clear_queue"})
                                );
                                let _ = append(&pi_rpc_input_log, &clear_line);
                                if let Some(ref mut stdin_pipe) = st.stdin {
                                    if stdin_pipe.write_all(clear_line.as_bytes()).is_err()
                                        || stdin_pipe.flush().is_err()
                                    {
                                        st.stdin = None;
                                        st.closed = true;
                                        clear_failed = true;
                                    }
                                }
                            }
                            Some((is_ok_turn, unconsumed, clear_failed))
                        } else {
                            None
                        }
                    };
                    if let Some((is_ok_turn, unconsumed, clear_failed)) = matched {
                        if clear_failed {
                            if !unconsumed.is_empty() {
                                let _ = crate::pending_reply::revert_to_fallback(run, &unconsumed);
                            }
                        } else if finalize_pi_rpc_turn(
                            shared,
                            run,
                            attempt,
                            &pi_rpc_input_log,
                            &mut log,
                            &mut current_round,
                            &answer,
                            is_ok_turn,
                            unconsumed,
                        ) {
                            settled = false;
                        }
                    }
                }
            }
            let items = if on_agy {
                if agy_session.is_empty() {
                    if let Some(id) = crate::agy::conversation(&event) {
                        agy_session = id.to_string();
                        usage_tracker.observe_session(id);
                        let _ = writeln!(
                            log,
                            "{}",
                            json!({"e":"session","id":id,"attempt":attempt,"at":iso()})
                        );
                    }
                }
                if let Some(raw_u) = crate::agy::extract_usage(&event) {
                    usage_tracker.observe_live_usage(crate::usage::TokenCounts::from_tuple(raw_u));
                }
                agy_filter.events(&event)
            } else {
                filter_pi(&event)
            };
            if items.iter().any(|item| s(item, "e") == "bash") {
                let raw_cmd = if on_agy {
                    crate::agy::raw_bash_command(&event)
                } else {
                    let cmd = s(&event["args"], "command");
                    (!cmd.is_empty()).then_some(cmd)
                };
                if let Some(cmd) = raw_cmd {
                    if cmd.chars().count() > 180 || cmd.trim_start().starts_with("node -e") {
                        crate::source_watch::record_bash_if_source(meta, run, cmd);
                    }
                }
            }
            if items.is_empty() && is_command_output_event(on_agy, &event) {
                let (mutex, changed) = &*watch;
                let mut state = mutex.lock().unwrap();
                if state.commands > 0 {
                    state.command_output_at = Some(Instant::now());
                    state.command_output_queued = lane::queued_seconds(run);
                    if state.command_warned_at.take().is_some() {
                        clear_stall_warning_for_kind(run, "command");
                    }
                    changed.notify_one();
                }
            }
            let mut bash_done_in_batch: Option<(String, bool)> = None;
            let mut bash_err_in_batch = String::new();
            for mut item in items {
                let kind = s(&item, "e").to_string();
                item["attempt"] = json!(attempt);
                item["at"] = json!(iso());
                {
                    let (mutex, changed) = &*watch;
                    let mut state = mutex.lock().unwrap();
                    state.last = Instant::now();
                    if state.idle_warned_at.take().is_some() {
                        clear_stall_warning_for_kind(run, "idle");
                    }
                    if kind == "bash" {
                        let now = Instant::now();
                        let queued = lane::queued_seconds(run);
                        state.commands += 1;
                        state.command_started = Some(now);
                        state.command_queued = queued;
                        state.command_output_at = Some(now);
                        state.command_output_queued = queued;
                        state.command_remote_log =
                            file_fingerprint(&run.join("remote-output.log"));
                        state.command_stderr_log = file_fingerprint(&run.join("stderr.log"));
                        state.command = s(&item, "cmd").to_string();
                        if state.command_warned_at.take().is_some() {
                            clear_stall_warning_for_kind(run, "command");
                        }
                    } else if kind == "bash_done" {
                        state.commands = (state.commands - 1).max(0);
                        if state.commands == 0 {
                            state.command_started = None;
                            state.command_output_at = None;
                            if state.command_warned_at.take().is_some() {
                                clear_stall_warning_for_kind(run, "command");
                            }
                        }
                        if !state.command.is_empty() {
                            item["cmd"] = json!(state.command.clone());
                        }
                        bash_done_in_batch = Some((s(&item, "cmd").to_string(), b(&item, "ok")));
                    }
                    changed.notify_one();
                }
                if kind == "tool_error" && s(&item, "tool") == "bash" {
                    bash_err_in_batch = s(&item, "detail").to_string();
                } else if matches!(kind.as_str(), "edit" | "write") {
                    loop_detector.reset();
                }
                let _ = writeln!(log, "{item}");
                let _ = log.flush();
                match kind.as_str() {
                    "turn" => {
                        attempt_turns.push(item.clone());
                        turn = item;
                        answer.clear();
                    }
                    "result" | "message" if on_agy => {
                        answer = crate::agy::relative_links(s(&item, "text"), s(meta, "workdir"))
                    }
                    "result" | "message" => answer = s(&item, "text").into(),
                    "settled" => settled = true,
                    "turn_error" => {
                        let detail = s(&item, "detail");
                        if !detail.is_empty() {
                            last_turn_error = detail.to_string();
                        }
                        turn = json!({"stopReason":"error"});
                    }
                    _ => {}
                }
            }
            if let Some((cmd, ok)) = bash_done_in_batch {
                if let Some(count) = loop_detector.record_bash(&cmd, ok, &bash_err_in_batch) {
                    let at = iso();
                    let detail = format!(
                        "同一命令连续重复 {count} 次且结果相同：{}",
                        clip(&cmd, 80)
                    );
                    let self_check = if is_reviewer {
                        format!(
                            "审查员机械重复（ADR 0120 规则：连续 {count} 次相同命令与结果）；已终止当前轮询并补问直接给出结论"
                        )
                    } else {
                        format!(
                            "疑似机械重复（ADR 0120 规则：连续 {count} 次相同命令与结果）；请检查进展或执行 delegate stop {loop_run_id} 后 reply 纠正"
                        )
                    };
                    let warning = json!({
                        "kind": "loop",
                        "cmd": clip(&cmd, 180),
                        "count": count,
                        "ok": ok,
                        "detail": detail,
                        "selfCheck": self_check,
                        "at": at,
                    });
                    let _ = write_json(run.join("stall-warning.json"), &warning);
                    let _ = writeln!(
                        log,
                        "{}",
                        json!({"e":"stall_warning","kind":"loop","cmd":clip(&cmd, 180),"count":count,"ok":ok,"detail":detail,"selfCheck":self_check,"at":at})
                    );
                    let _ = log.flush();
                    crate::events::emit(
                        &loop_run_id,
                        &loop_repo,
                        "stall_warning",
                        json!({
                            "name": loop_name,
                            "stallKind": "loop",
                            "cmd": clip(&cmd, 180),
                            "count": count,
                            "ok": ok,
                            "detail": detail,
                            "selfCheck": self_check,
                        }),
                    );
                    if is_reviewer {
                        loop_stopped = true;
                        crate::cleanup::record(run, pid);
                        kill_group(pid, 1.0);
                        break;
                    } else {
                        spawn_isolated_loop_review(meta, run, &cmd, count);
                    }
                }
            }
            // At each agy turn boundary (`result` / `settled`), drain any appended follow-up
            // messages over the live stdin pipe, or mark the in-run channel closed and drop stdin
            // so agy finishes cleanly while later arrivals fall back to `_reply-pending`.
            if on_agy && allow_append && settled && agy_stdin.is_some() {
                let is_ok_turn = s(&turn, "stopReason") == "stop"
                    && !lane::stopped()
                    && !cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst));
                if is_ok_turn {
                    match crate::pending_reply::take_or_close_in_run(run, current_round + 1) {
                        Ok(crate::pending_reply::TakeInRun::Messages(batch)) => {
                            if !answer.trim().is_empty() {
                                let formatted = format!("{}\n", answer.trim_end_matches('\n'));
                                let _ = write(
                                    run.join(format!("round-{current_round}-result.md")),
                                    &formatted,
                                );
                            }
                            current_round += 1;
                            let merged_text =
                                crate::pending_reply::format_merged_followup(&batch);
                            let images = crate::pending_reply::merged_images(&batch);
                            let line = crate::agy::followup_input(&merged_text, &images);
                            let _ = append(&agy_input_log, &line);
                            let wrote_ok = if let Some(ref mut stdin_pipe) = agy_stdin {
                                stdin_pipe.write_all(line.as_bytes()).is_ok()
                                    && stdin_pipe.flush().is_ok()
                            } else {
                                false
                            };
                            if wrote_ok {
                                let seqs: Vec<u64> = batch.iter().map(|it| it.seq).collect();
                                let _ = writeln!(
                                    log,
                                    "{}",
                                    json!({
                                        "e": "append_delivered",
                                        "round": current_round,
                                        "seqs": seqs,
                                        "count": batch.len(),
                                        "mode": "in_run",
                                        "attempt": attempt,
                                        "at": iso(),
                                    })
                                );
                                let _ = log.flush();
                                crate::index::upsert_run(run);
                                crate::board::refresh_for_run(run);
                                settled = false;
                            } else {
                                let _ = crate::pending_reply::revert_to_fallback(run, &batch);
                                agy_stdin = None;
                            }
                        }
                        Ok(crate::pending_reply::TakeInRun::Closed) | Err(_) => {
                            if current_round > 1 && !answer.trim().is_empty() {
                                let formatted = format!("{}\n", answer.trim_end_matches('\n'));
                                let _ = write(
                                    run.join(format!("round-{current_round}-result.md")),
                                    &formatted,
                                );
                            }
                            agy_stdin = None;
                        }
                    }
                } else {
                    agy_stdin = None;
                }
            }
            // At each Pi RPC `agent_settled` boundary:
            // 1. Block `steer_pump` (`settling = true`) so no new `queue_message` races with close.
            // 2. If any `steer` was sent this turn, query `get_queue` first to verify whether it
            //    was consumed by the turn or landed after turn end (still `queued`).
            // 3. Otherwise finalize immediately (send next `prompt` or `stop`).
            if let Some(ref shared) = pi_rpc_state {
                if settled {
                    let is_ok_turn = s(&turn, "stopReason") == "stop"
                        && !lane::stopped()
                        && !cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst));
                    enum SettleAction {
                        AlreadyHandling,
                        WaitQueueCheck,
                        FinalizeNow,
                        PipeBroken(Vec<crate::pending_reply::AppendItem>),
                    }
                    let action = {
                        let mut st = shared.lock().unwrap();
                        if st.stdin.is_none() || st.closed || st.pending_queue_check.is_some() {
                            SettleAction::AlreadyHandling
                        } else {
                            st.settling = true;
                            if !st.in_flight_steers.is_empty() || !st.acked_steers.is_empty() {
                                st.next_req += 1;
                                let qid = format!("qcheck-{}", st.next_req);
                                let qline =
                                    format!("{}\n", json!({"id": qid, "type": "get_queue"}));
                                let _ = append(&pi_rpc_input_log, &qline);
                                let wrote_ok = if let Some(ref mut stdin_pipe) = st.stdin {
                                    stdin_pipe.write_all(qline.as_bytes()).is_ok()
                                        && stdin_pipe.flush().is_ok()
                                } else {
                                    false
                                };
                                if wrote_ok {
                                    st.pending_queue_check = Some((qid, is_ok_turn));
                                    SettleAction::WaitQueueCheck
                                } else {
                                    st.stdin = None;
                                    st.closed = true;
                                    let mut lost: Vec<_> = st
                                        .in_flight_steers
                                        .drain()
                                        .flat_map(|(_, v)| v)
                                        .collect();
                                    lost.extend(st.acked_steers.drain().flat_map(|(_, v)| v));
                                    lost.sort_by_key(|it| it.seq);
                                    SettleAction::PipeBroken(lost)
                                }
                            } else {
                                SettleAction::FinalizeNow
                            }
                        }
                    };
                    match action {
                        SettleAction::AlreadyHandling | SettleAction::WaitQueueCheck => {}
                        SettleAction::PipeBroken(lost) => {
                            if !lost.is_empty() {
                                let _ = crate::pending_reply::revert_to_fallback(run, &lost);
                            }
                        }
                        SettleAction::FinalizeNow => {
                            if finalize_pi_rpc_turn(
                                shared,
                                run,
                                attempt,
                                &pi_rpc_input_log,
                                &mut log,
                                &mut current_round,
                                &answer,
                                is_ok_turn,
                                Vec::new(),
                            ) {
                                settled = false;
                            }
                        }
                    }
                }
            }
        }
    }
    drop(agy_stdin);
    steer_stop.store(true, Ordering::SeqCst);
    if on_pi_rpc {
        let _ = fs::remove_file(run.join(crate::pending_reply::PI_RPC_STEER_MARKER));
    }
    if let Some(ref shared) = pi_rpc_state {
        let mut leftover: Vec<crate::pending_reply::AppendItem> = {
            let mut st = shared.lock().unwrap();
            st.closed = true;
            st.stdin = None;
            let mut out: Vec<_> = st
                .in_flight_steers
                .drain()
                .flat_map(|(_, items)| items)
                .collect();
            if !settled || st.pending_queue_check.is_some() || !st.acked_steers.is_empty() {
                out.extend(st.acked_steers.drain().flat_map(|(_, items)| items));
            }
            out
        };
        leftover.sort_by_key(|it| it.seq);
        if !leftover.is_empty() {
            let _ = crate::pending_reply::revert_to_fallback(run, &leftover);
        }
    }
    if let Some(handle) = steer_pump {
        let _ = handle.join();
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
    if let Some(ref mut overlay) = agy_overlay {
        overlay.finish();
    }
    drop(agy_overlay);
    let _ = fs::remove_file(run.join("agy_home"));
    if !on_agy {
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
    let _ = fs::remove_file(run.join("stall-warning.json"));
    let verdict = if lane::stopped() {
        "stopped"
    } else if cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) {
        "cancelled"
    } else if timed.load(Ordering::SeqCst) {
        "timeout"
    } else if loop_stopped {
        "loop_stopped"
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
    if on_agy && verdict == "failed" {
        if let Some(ref acct) = agy_account {
            let mut exhausted = crate::agy_accounts::is_quota_exhausted_error(&last_turn_error);
            if !exhausted {
                let stderr_txt = read(run.join("stderr.log"));
                let tail: String = stderr_txt
                    .lines()
                    .rev()
                    .take(8)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                exhausted = crate::agy_accounts::is_quota_exhausted_error(&tail);
            }
            if exhausted {
                let _ = crate::agy_accounts::mark_slot_quota_exhausted_passive(
                    acct,
                    s(meta, "model"),
                    epoch() as i64,
                );
            }
        }
    }
    usage_tracker.finish(&attempt_turns, verdict);
    Ok((verdict.into(), answer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(_agent: &str, meta: &Value, session: Option<&str>) -> Vec<String> {
        command_pi(meta, session)
    }

    #[test]
    fn a_progress_sidecar_is_not_a_session() {
        let dir = std::env::temp_dir().join(format!("delegate-sessions-{}", std::process::id()));
        let cwd = dir.join("--work--");
        fs::create_dir_all(&cwd).unwrap();
        let main = cwd.join("2026-11-08T06-13-05Z_abc.jsonl");
        fs::write(&main, "").unwrap();
        fs::write(
            cwd.join("2026-11-08T06-13-05Z_abc.jsonl.progress.jsonl"),
            "",
        )
        .unwrap();
        assert_eq!(session_files(&dir), std::slice::from_ref(&main));
        assert_eq!(
            session_file(dir.to_str().unwrap(), "abc"),
            Some(main.clone())
        );
        assert_eq!(
            session_file(dir.to_str().unwrap(), "abc.jsonl.progress"),
            Some(main)
        );
        fs::remove_dir_all(&dir).unwrap();
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
    fn stall_settings_have_defaults_and_zero_disables() {
        std::env::remove_var("DELEGATE_STALL_IDLE");
        std::env::remove_var("PI_DELEGATE_STALL_IDLE");
        std::env::remove_var("DELEGATE_STALL_GENERATE");
        std::env::remove_var("PI_DELEGATE_STALL_GENERATE");
        assert_eq!(stall_seconds(&json!({}), "idle"), Some(1800.0));
        assert_eq!(stall_seconds(&json!({}), "generate"), Some(600.0));
        assert_eq!(stall_seconds(&json!({"stall":{"generate":"0"}}), "generate"), None);
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
                "--tools",
                "read,grep,find,ls",
                "-p",
                "@/a.png"
            ]
        );
    }

    #[test]
    fn loop_detector_counts_only_identical_consecutive_commands_and_resets() {
        let mut d = LoopDetector::new(3, false);
        assert_eq!(d.record_bash("cargo test", false, "err A"), None);
        assert_eq!(d.record_bash("cargo test", false, "err A"), None);
        // Different error resets streak
        assert_eq!(d.record_bash("cargo test", false, "err B"), None);
        assert_eq!(d.record_bash("cargo test", false, "err B"), None);
        // 3rd identical triggers and resets
        assert_eq!(d.record_bash("cargo test", false, "err B"), Some(3));
        assert_eq!(d.record_bash("cargo test", false, "err B"), None);
        // Explicit reset (e.g. after edit/write) resets streak
        d.reset();
        assert_eq!(d.record_bash("cargo test", false, "err B"), None);
    }

    #[test]
    fn loop_detector_excludes_pure_wait_and_poll_commands() {
        for cmd in [
            "sleep 5",
            "/bin/sleep 10",
            "wait",
            "wait 12345",
            "tail -n 30 /tmp/build.log",
            "tail -f app.log",
            "sleep 10 && tail -n 20 /tmp/build.log",
            "sleep 5; ps -p 12345 && tail -n 10 /tmp/out.log",
            "sleep 5 && kill -0 12345 2>/dev/null",
            "sleep 5 && test -f /tmp/done && cat /tmp/done",
            "cd /work && sleep 3 && tail -n 50 log | grep -i error || true",
            "while kill -0 1234; do sleep 2; done",
        ] {
            assert!(
                is_pure_wait_command(cmd),
                "expected pure wait/poll command: {cmd}"
            );
            let mut d = LoopDetector::new(3, false);
            for _ in 0..10 {
                assert_eq!(
                    d.record_bash(cmd, true, ""),
                    None,
                    "wait/poll should never trigger loop: {cmd}"
                );
            }
            // Reviewers disable the wait/poll exemption so repeated polling triggers self-check.
            let mut rev_d = LoopDetector::new(3, true);
            assert_eq!(rev_d.record_bash(cmd, true, ""), None);
            assert_eq!(rev_d.record_bash(cmd, true, ""), None);
            assert_eq!(
                rev_d.record_bash(cmd, true, ""),
                Some(3),
                "reviewer loop detector must NOT exempt wait/poll: {cmd}"
            );
        }

        for cmd in [
            "cargo test",
            "sleep 1 && cargo test",
            "sleep 2 && rm -f /tmp/lock",
            "sleep 1 && kill -9 12345",
            "tail -n 20 /tmp/a.log > /tmp/b.log",
            "sleep 1 && sed -i 's/a/b/' file.txt",
            "ps aux",
            "cat /tmp/out.log",
        ] {
            assert!(
                !is_pure_wait_command(cmd),
                "expected NON-wait command: {cmd}"
            );
        }

        let mut d = LoopDetector::new(3, false);
        assert_eq!(d.record_bash("sleep 1 && cargo test", false, "fail"), None);
        assert_eq!(d.record_bash("sleep 1 && cargo test", false, "fail"), None);
        assert_eq!(
            d.record_bash("sleep 1 && cargo test", false, "fail"),
            Some(3)
        );
    }

    #[test]
    fn pi_rpc_command_omits_print_flag_and_steer_probe_detects_capability() {
        let meta = json!({"workdir":"/repo","sessionDir":"/runs/session","mode":"read-only","provider":"p","model":"m","thinking":"high"});
        let rpc_args = command_pi_rpc(&meta, Some("s-1"));
        assert!(rpc_args.windows(2).any(|w| w == ["--mode", "rpc"]));
        assert!(!rpc_args.contains(&"-p".to_string()));
        assert!(rpc_args.windows(2).any(|w| w == ["--session-id", "s-1"]));

        let dir = std::env::temp_dir().join(format!(
            "delegate-rpc-probe-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&dir).unwrap();

        let rpc_bin = dir.join("pi-rpc");
        fs::write(
            &rpc_bin,
            "#!/bin/sh\n\
             echo '{\"type\":\"ready\",\"supportedCommands\":[\"prompt\",\"queue_message\",\"get_queue\",\"clear_queue\",\"stop\"]}'\n\
             while IFS= read -r line; do\n\
               case \"$line\" in\n\
                 *probe-steer*) echo '{\"type\":\"response\",\"command\":\"queue_message\",\"success\":true,\"id\":\"probe-steer\",\"data\":{\"accepted\":true,\"queue\":\"steer\"}}' ;;\n\
                 *probe-stop*) echo '{\"type\":\"response\",\"command\":\"stop\",\"success\":true,\"id\":\"probe-stop\"}' ; exit 0 ;;\n\
               esac\n\
             done\n",
        )
        .unwrap();
        fs::set_permissions(&rpc_bin, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(pi_supports_rpc_steer(&rpc_bin, Some("rpi 0.20.0")));

        // Legacy fake-pi version string skips RPC probe completely.
        assert!(!pi_supports_rpc_steer(&rpc_bin, Some("fake-pi 1.0")));

        let no_steer_bin = dir.join("pi-no-steer");
        fs::write(
            &no_steer_bin,
            "#!/bin/sh\n\
             echo '{\"type\":\"ready\",\"supportedCommands\":[\"prompt\",\"queue_message\",\"stop\"]}'\n\
             while IFS= read -r line; do\n\
               case \"$line\" in\n\
                 *probe-steer*) echo '{\"type\":\"response\",\"command\":\"queue_message\",\"success\":true,\"id\":\"probe-steer\",\"data\":{\"accepted\":true,\"queue\":\"followUp\"}}' ;;\n\
                 *probe-stop*) echo '{\"type\":\"response\",\"command\":\"stop\",\"success\":true,\"id\":\"probe-stop\"}' ; exit 0 ;;\n\
               esac\n\
             done\n",
        )
        .unwrap();
        fs::set_permissions(&no_steer_bin, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!pi_supports_rpc_steer(&no_steer_bin, Some("rpi 0.19.0")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_question_branches_on_changed_empty() {
        // Branch A: zero files modified -> relaxed natural question detection pauses as Blocking.
        assert_eq!(
            classify_question("查到两处实现冲突，请确认按方案 A 还是 B？", true),
            QuestionKind::Blocking
        );
        assert_eq!(
            classify_question("## 现状\n已排查完成。\n\n请问是否需要兼容旧字段？", true),
            QuestionKind::Blocking
        );
        assert_eq!(
            classify_question("已完成调研，无需主控介入，一切正常。", true),
            QuestionKind::None
        );
        assert_eq!(
            classify_question("无矛盾需要主控拍板。\n\n### 改动位置\n改 `src/alpha.rs`。", true),
            QuestionKind::None
        );
        assert_eq!(
            classify_question("不需要主控决定，按原计划推进即可。", true),
            QuestionKind::None
        );
        assert_eq!(
            classify_question("无法确定实现方式需要主控决定。", true),
            QuestionKind::Blocking
        );

        // Branch B: files were modified -> only explicit opening stop-to-ask skips accept;
        // trailing natural question produces TrailingNote so accept & review still run.
        assert_eq!(
            classify_question("**需要主控决定**\n改到一半发现契约冲突，倾向 A。", false),
            QuestionKind::Blocking
        );
        assert_eq!(
            classify_question("# 暂停等主控\n是否删除旧配置字段？", false),
            QuestionKind::Blocking
        );
        assert_eq!(
            classify_question(
                "已完成改动并跑完单测。\n- 修改了 src/a.rs\n\n顺便确认一下，下次要不要把旧别名一起删掉？",
                false
            ),
            QuestionKind::TrailingNote
        );
        assert_eq!(
            classify_question("已完成全部改动，验收全绿，无需主控确认。", false),
            QuestionKind::None
        );
        assert_eq!(
            classify_question("无矛盾需要主控拍板，已完成全部改动。", false),
            QuestionKind::None
        );
    }

    #[test]
    fn file_output_advanced_tracks_growth_and_tail_rotation() {
        let dir = std::env::temp_dir().join(format!(
            "delegate-output-fp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("remote-output.log");

        // Missing file -> no progress.
        let mut prev = file_fingerprint(&log);
        assert!(prev.is_none());
        assert!(!file_output_advanced(&log, &mut prev));

        // Creating an empty file (TailLogWriter::create before any output) is not progress.
        fs::write(&log, b"").unwrap();
        assert!(!file_output_advanced(&log, &mut prev));
        assert!(prev.is_some());

        // Appending bytes counts as progress; unchanged second poll does not.
        fs::write(&log, b"epoch 1 loss=0.5\n").unwrap();
        assert!(file_output_advanced(&log, &mut prev));
        assert!(!file_output_advanced(&log, &mut prev));

        // TailLogWriter rotation: rename a smaller temp file over the log (len shrinks, ino changes).
        let tmp = dir.join(".remote-output.log.tmp");
        fs::write(&tmp, b"tail\n").unwrap();
        fs::rename(&tmp, &log).unwrap();
        assert!(file_output_advanced(&log, &mut prev));
        assert!(!file_output_advanced(&log, &mut prev));

        // Truncating to 0 (new command starting) updates baseline without counting as progress.
        fs::write(&log, b"").unwrap();
        assert!(!file_output_advanced(&log, &mut prev));
        assert!(!file_output_advanced(&log, &mut prev));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_output_event_matches_pi_and_agy_streaming_updates() {
        assert!(is_command_output_event(
            false,
            &json!({"type": "tool_execution_update", "toolName": "bash", "partialResult": {"content": [{"text": "step 1"}]}})
        ));
        assert!(!is_command_output_event(
            false,
            &json!({"type": "tool_execution_update", "toolName": "read"})
        ));
        assert!(!is_command_output_event(
            false,
            &json!({"type": "tool_execution_start", "toolName": "bash"})
        ));

        assert!(is_command_output_event(
            true,
            &json!({"event": "step_update", "step_update": {"step_type": "tool", "tool_name": "run_command", "state": "ACTIVE"}})
        ));
        assert!(!is_command_output_event(
            true,
            &json!({"event": "step_update", "step_update": {"step_type": "tool", "tool_name": "run_command", "state": "DONE"}})
        ));
        assert!(!is_command_output_event(
            true,
            &json!({"event": "step_update", "step_update": {"step_type": "agent_response", "text_delta": "hi"}})
        ));
    }
}
