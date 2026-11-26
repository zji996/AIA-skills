mod accept;
mod activity;
mod agents;
mod agy;
mod agy_accounts;
mod archive;
mod usage;
mod board;
mod card;
mod changes;
mod cleanup;
mod common;
mod config;
mod dag_queue;
mod deny;
mod disk;
mod eta;
mod events;
mod fill;
mod fit;
mod gc;
mod handoff;
mod help;
mod history;
mod hook;
mod hook_check;
mod index;
mod inotify;
mod jobserver;
mod lane;
mod launch;
mod locks;
mod measure;
mod merge;
mod models;
mod notes;
mod output;
mod pool;
mod premerge;
mod probe;
mod process_match;
mod pulse;
mod pending_reply;
mod source_guard;
mod queue;
mod radar;
mod refs;
mod resources;
mod remote;
mod review;
mod runs;
mod source_build;
mod source_watch;
mod speed;
mod supervise;
mod transient;
mod tui;
mod waiting;
mod work;
mod worktree;
use common::*;
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) fn mark_result_read(run: &Path) {
    let meta_path = run.join("meta.json");
    if meta_path.is_file() {
        let mut meta = json(&meta_path);
        if meta.is_object() {
            meta["resultReadAt"] = json!(iso());
            let _ = write_json(&meta_path, &meta);
            index::upsert_run(run);
        }
    }
}

fn print_status_lanes() -> Res<()> {
    for (label, since, running) in lane::remote_holders()? {
        println!(
            "remote {:<8} {since}  [重队列]  {label}",
            if running { "running" } else { "queued" }
        );
    }
    for (label, since, running) in lane::remote_light_holders()? {
        println!(
            "remote {:<8} {since}  [轻队列]  {label}",
            if running { "running" } else { "queued" }
        );
    }
    for (label, since, running) in lane::holders()? {
        println!(
            "local  {:<8} {since}  [本机]  {label}",
            if running { "running" } else { "queued" }
        );
    }
    Ok(())
}

fn status_line(run: &Path) -> String {
    output_line(run, runs::status(run))
}
fn output_line(run: &Path, status: serde_json::Value) -> String {
    if output::json_enabled() {
        format_status(status)
    } else {
        output::human(run, &status)
    }
}

fn default_check_in_with(get: impl Fn(&str) -> Option<String>) -> Option<f64> {
    if get("CLAUDE_CODE_SESSION_ID").is_some_and(|v| !v.trim().is_empty())
        || get("ANTIGRAVITY_CONVERSATION_ID").is_some_and(|v| !v.trim().is_empty())
    {
        Some(50.0 * 60.0)
    } else {
        None
    }
}

fn check_in(max: Option<f64>, runs: &[PathBuf]) -> Option<f64> {
    if max.is_some() {
        return max;
    }
    let configured = setting("CHECK_IN", "");
    let configured = if configured.is_empty() {
        runs.iter()
            .find_map(|run| {
                json(run.join("meta.json"))["checkIn"]
                    .as_str()
                    .map(str::to_string)
            })
            .or_else(|| {
                config::load(git_top(&env::current_dir().unwrap_or_default()).as_deref())
                    .ok()
                    .and_then(|c| c.value["checkIn"].as_str().map(str::to_string))
            })
    } else {
        Some(configured)
    };
    if let Some(value) = configured {
        return if value.trim() == "0" {
            None
        } else {
            seconds(&value).ok()
        };
    }
    default_check_in_with(|k| env::var(k).ok())
}

fn minutes_text(seconds: i64) -> String {
    let minutes = seconds.max(0) / 60;
    if minutes >= 60 {
        format!("{} 小时 {} 分钟", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} 分钟")
    }
}

fn health_verdict(run: &Path) -> (String, bool) {
    let events = runs::events(run);
    let now = epoch() as i64;
    // A long command in flight (a test suite, a lane-queued gate) is progress, not silence.
    let mut in_flight: Vec<&serde_json::Value> = Vec::new();
    for e in &events {
        match s(e, "e") {
            "bash" => in_flight.push(e),
            "bash_done" => {
                in_flight.pop();
            }
            _ => {}
        }
    }
    if let Some(command) = in_flight.last() {
        let age = iso_epoch(s(command, "at")).map_or(0, |at| now - at);
        let mut cmd: String = s(command, "cmd").chars().take(60).collect();
        if s(command, "cmd").chars().count() > 60 {
            cmd.push('…');
        }
        return (format!("正在执行 {cmd}（{}）", minutes_text(age)), false);
    }
    let last_at = events.iter().rev().find_map(|e| iso_epoch(s(e, "at")));
    let idle = last_at.map(|at| (now - at).max(0)).unwrap_or(0);
    if idle >= 15 * 60 {
        return (
            format!("可能卡住：已 {} 无新动作", minutes_text(idle)),
            true,
        );
    }
    let mut failures: Vec<(String, usize)> = Vec::new();
    for e in events
        .iter()
        .rev()
        .filter(|e| s(e, "e") == "bash_done" && !b(e, "ok"))
    {
        let cmd = s(e, "cmd").to_string();
        if let Some((last, count)) = failures.last_mut() {
            if *last == cmd {
                *count += 1;
                continue;
            }
        }
        failures.push((cmd, 1));
        if failures.len() > 1 {
            break;
        }
    }
    if let Some((_, count)) = failures.first().filter(|(_, n)| *n >= 3) {
        return (
            format!("可能在原地打转：同一命令连续失败 {} 次", count),
            true,
        );
    }
    ("正常推进".into(), false)
}

/// One check-in line for a still-running run: its status line plus a health verdict.
/// Callers print `继续等：<next>` once after all lines.
fn glance_line(run: &Path, next: &str) -> String {
    let status = runs::status(run);
    let (verdict, warn) = health_verdict(run);
    if output::json_enabled() {
        let mut status = status;
        status["health"] = json!(verdict);
        status["glance"] = json!(true);
        status["next"] = json!(next);
        if warn {
            status["suggestion"] = json!(format!(
                "delegate stop {} then delegate reply {} \"纠正\"",
                s(&status, "run"),
                s(&status, "run")
            ));
        }
        return format_status(status);
    }
    let suggestion = if warn {
        format!(
            "；建议：{} stop {}，再 {} reply {} \"纠正\"",
            script().display(),
            s(&status, "run"),
            script().display(),
            s(&status, "run")
        )
    } else {
        String::new()
    };
    format!(
        "{} · {verdict}{suggestion}",
        output_line(run, status.clone())
    )
}

fn print_continue(next: &str) {
    if !output::json_enabled() {
        println!("继续等：{next}");
    }
}
/// A finished run's status (zero locks, zero side effects).
fn outcome_line(run: &Path, timing: Option<&str>) -> String {
    let mut status = runs::status(run);
    if runs::active(&runs::state(run)) {
        return output_line(run, status);
    }
    if let Some(timing) = timing {
        status["completionTiming"] = json!(timing);
    }
    output_line(run, status)
}
fn format_status(status: serde_json::Value) -> String {
    let keys = [
        "run",
        "name",
        "state",
        "finishedAt",
        "completionTiming",
        "agent",
        "agentBin",
        "tier",
        "mode",
        "area",
        "areaGroup",
        "areaExperiment",
        "parent",
        "redispatch",
        "worktree",
        "netBase",
        "ageSeconds",
        "elapsedSeconds",
        "attempts",
        "model",
        "turns",
        "files",
        "changes",
        "pendingChanges",
        "shape",
        "accept",
        "card",
        "review",
        "sign",
        "readOnlyViolation",
        "protectViolation",
        "protectViolationReasons",
        "promptProtectedPaths",
        "workspaceChanged",
        "escalatedFrom",
        "queuedSeconds",
        "graceSeconds",
        "tokens",
        "tokensDelta",
        "cleanup",
        "warnings",
        "warning",
        "error",
        "policyRefused",
        "policyRewriteAttempted",
        "missingObjects",
        "recovery",
        "last",
        "activity",
        "stallWarning",
        "idleSeconds",
        "result",
        "resultChars",
        "sourceDrift",
        "next",
        "dir",
    ];
    let mut fields = Vec::new();
    for key in keys {
        if let Some(value) = status.get(key) {
            fields.push(format!("\"{key}\":{value}"));
        }
    }
    if let Some(object) = status.as_object() {
        for (key, value) in object {
            if !keys.contains(&key.as_str()) {
                fields.push(format!("{}:{value}", json!(key)));
            }
        }
    }
    format!("{{{}}}", fields.join(","))
}
fn hint_undelivered_other_runs(created: &Path) {
    let current = caller();
    let others = runs::all_runs()
        .into_iter()
        .filter(|run| {
            run != created && !runs::active(&runs::state(run)) && !run.join(".delivered").exists()
        })
        .filter(|run| {
            let meta = json(run.join("meta.json"));
            current
                .as_deref()
                .is_none_or(|id| meta["caller"].as_str() != Some(id))
        })
        .collect::<Vec<_>>();
    runs::other_runs_hint(&others);
}

fn hint_concurrent_writes(created: &Path) {
    for other in worktree::concurrent_writes(created) {
        let run_id = s(&other, "run");
        let name = s(&other, "name");
        let status = s(&other, "status");
        let named_list = other["named"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let collisions = other["collisions"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let files = other["sample"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let files_part = if files.is_empty() {
            String::new()
        } else {
            format!(": {files}")
        };
        let coll_part = if collisions.is_empty() {
            String::new()
        } else {
            format!("; {}", collisions.join("、"))
        };
        if status == "unapplied" {
            let mut reasons = Vec::new();
            if !named_list.is_empty() {
                reasons.push(format!("同文件 {}", named_list.join(", ")));
            }
            if !collisions.is_empty() {
                reasons.push(collisions.join("、"));
            }
            eprintln!(
                "delegate: overlap: unapplied write task {name} ({run_id}) on this source ({} files changed{files_part}; {}): consider applying it first or queueing with delegate queue add … --after {run_id}",
                other["changed"],
                reasons.join("；")
            );
        } else if !named_list.is_empty() {
            eprintln!(
                "delegate: overlap: write task {name} is running on this source ({} files changed so far{files_part}); this prompt names {} of them ({}){coll_part}: consider waiting for {name}, using --protect, or queueing with --after {run_id}",
                other["changed"],
                named_list.len(),
                named_list.join(", ")
            );
        } else if !collisions.is_empty() {
            eprintln!(
                "delegate: overlap: write task {name} ({run_id}) is running on this source ({} files changed so far{files_part}; {}): consider waiting for {name} or queueing with delegate queue add … --after {run_id}",
                other["changed"],
                collisions.join("、")
            );
        } else {
            eprintln!(
                "delegate: note: write task {name} is running on this source ({} files changed so far{files_part})",
                other["changed"]
            );
        }
    }
}

pub(crate) type Parsed = (Vec<String>, Vec<String>, Vec<(String, String)>);
pub(crate) fn parse_simple(args: &[String], flags: &[&str], values: &[&str]) -> Res<Parsed> {
    let mut pos = vec![];
    let mut present = vec![];
    let mut kv = vec![];
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if flags.contains(&a.as_str()) {
            present.push(a.clone());
        } else if values.contains(&a.as_str()) {
            i += 1;
            if i >= args.len() {
                return Err(format!("argument {a}: expected one argument"));
            }
            kv.push((a.clone(), args[i].clone()));
        } else if a.starts_with('-') && a != "-" {
            return Err(format!("unrecognized arguments: {a}"));
        } else {
            pos.push(a.clone());
        }
        i += 1;
    }
    Ok((pos, present, kv))
}
pub(crate) fn has(flags: &[String], key: &str) -> bool {
    flags.iter().any(|x| x == key)
}
pub(crate) fn value<'a>(kv: &'a [(String, String)], key: &str) -> Option<&'a str> {
    kv.iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}
/// Returns true when the answer was abbreviated.
fn print_answer(run: &Path, full: bool) -> bool {
    let result = read(run.join("result.md"));
    let configured = json(run.join("meta.json"))["resultDisplayChars"]
        .as_u64()
        .unwrap_or(20000);
    let limit = setting("RESULT_CHARS", &configured.to_string())
        .parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or(20000);
    println!(
        "\n===== result: {} ({} chars) =====",
        run.file_name().unwrap_or_default().to_string_lossy(),
        result.chars().count()
    );
    let compression = json(run.join("compression.json"));
    let failed_compression = compression.is_object() && !b(&compression, "ok");
    let limit = if failed_compression && !full {
        limit.min(
            json(run.join("meta.json"))["maxAnswer"]
                .as_u64()
                .unwrap_or(limit as u64) as usize,
        )
    } else {
        limit
    };
    let truncated = !full
        && (failed_compression && result.chars().count() > limit
            || result.chars().count() > limit.saturating_add(limit / 4));
    if compression.is_object() && !failed_compression {
        println!("答复压缩完成；原文：{}", s(&compression, "original"));
    }
    if !truncated {
        println!("{}", result.trim_end_matches('\n'));
    } else {
        let head_len = (limit * 2).div_ceil(3);
        let tail_len = limit - head_len;
        let head: String = result.chars().take(head_len).collect();
        let tail: String = result
            .chars()
            .skip(result.chars().count() - tail_len)
            .collect();
        println!(
            "{}\n[… 省略 {} 字符；完整答复: {}]\n{}",
            head.trim_end_matches('\n'),
            result.chars().count() - limit,
            if output::json_enabled() {
                run.join("result.md").display().to_string()
            } else {
                format!(
                    "delegate result {}",
                    run.file_name().unwrap_or_default().to_string_lossy()
                )
            },
            tail.trim_end_matches('\n')
        );
    }
    println!(
        "===== end: {} =====",
        run.file_name().unwrap_or_default().to_string_lossy()
    );
    if failed_compression && !full {
        println!(
            "答复压缩失败，保留原答复；原文：{}；原文字数：{}；压缩失败：{}",
            s(&compression, "original"),
            result.trim_end_matches('\n').chars().count(),
            s(&compression, "error").lines().next().unwrap_or("unknown")
        );
    }
    if !output::json_enabled() {
        let rounds = runs::collect_result_rounds(run, true);
        if rounds.len() > 1 {
            let run_id = run.file_name().unwrap_or_default().to_string_lossy();
            println!(
                "各轮答复：共 {} 轮（当前最新；查看各轮：delegate result {run_id} --round <1..={}> / --all / --rounds）",
                rounds.len(),
                rounds.len()
            );
        }
    }
    truncated
}
fn progress(run: &Path, tag: &str) {
    let marker = run.join(".progress");
    let seen = read(&marker).trim().parse::<usize>().unwrap_or(0);
    let evs = runs::events(run);
    for e in evs.iter().skip(seen) {
        if [
            "edit",
            "write",
            "tool_error",
            "turn_error",
            "retry",
            "rerun",
            "compaction_start",
            "compaction_end",
        ]
        .contains(&s(e, "e"))
        {
            let mut x = e.clone();
            if !tag.is_empty() {
                x["run"] = json!(tag);
            }
            println!("{x}");
        }
    }
    let _ = write(marker, evs.len().to_string());
}
fn sleep_on_supervisors(runs: &[PathBuf], timeout: Option<f64>) -> bool {
    let locks: Vec<_> = runs
        .iter()
        .filter(|run| runs::active(&runs::state(run)))
        .map(|run| run.join("supervisor.lock"))
        .collect();
    if locks.iter().any(|lock| !lock.is_file()) {
        return false;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let count = locks.len();
    for path in locks {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _guard = common::lock(&path, false, false);
            let _ = tx.send(());
        });
    }
    drop(tx);
    let start = Instant::now();
    for _ in 0..count {
        if let Some(seconds) = timeout {
            let left = (seconds - start.elapsed().as_secs_f64()).max(0.0);
            if rx.recv_timeout(Duration::from_secs_f64(left)).is_err() {
                break;
            }
        } else if rx.recv().is_err() {
            break;
        }
    }
    true
}
fn collect(
    runs: &[PathBuf],
    max: Option<f64>,
    show_progress: bool,
    full: bool,
    show_result: bool,
    next: &str,
) -> i32 {
    let requested = runs;
    let Ok((runs, _claims)) = waiting::claim(runs) else {
        return 2;
    };
    if runs.is_empty()
        || requested
            .iter()
            .any(|r| !runs.contains(r) && runs::active(&runs::state(r)))
    {
        return waiting::COVERED;
    }
    collect_timed(&runs, max, show_progress, full, show_result, None, next)
}
fn collect_timed(
    runs: &[PathBuf],
    max: Option<f64>,
    show_progress: bool,
    full: bool,
    show_result: bool,
    already: Option<&std::collections::HashSet<PathBuf>>,
    next: &str,
) -> i32 {
    let begin = Instant::now();
    let poll = setting("POLL", "1").parse::<f64>().unwrap_or(1.0).max(0.01);
    loop {
        let mut active = false;
        for run in runs {
            if show_progress {
                progress(
                    run,
                    if runs.len() > 1 {
                        run.file_name().unwrap_or_default().to_str().unwrap_or("")
                    } else {
                        ""
                    },
                );
            }
            active |= runs::active(&runs::state(run));
        }
        if !active || max.is_some_and(|m| begin.elapsed().as_secs_f64() >= m) {
            break;
        }
        let left = max.map(|m| (m - begin.elapsed().as_secs_f64()).max(0.0));
        if show_progress || !sleep_on_supervisors(runs, left) {
            std::thread::sleep(Duration::from_secs_f64(left.unwrap_or(poll).min(poll)));
        }
    }
    let timed_out = max.is_some_and(|m| begin.elapsed().as_secs_f64() >= m);
    let mut code = 0;
    let mut glanced = false;
    let mut settled_for_hints: Vec<PathBuf> = Vec::new();
    for run in runs {
        if run.join("exit_code").is_file() {
            let lock = run.join("supervisor.lock");
            if lock.is_file() {
                // Wait briefly for `_supervise` to finish post-exit_code bookkeeping (events/work/index).
                let _ = common::lock(&lock, false, false);
            }
        }
        let state = runs::state(run);
        if timed_out && runs::active(&state) {
            glanced = true;
            println!("{}", glance_line(run, next));
        } else {
            println!(
                "{}",
                outcome_line(run, already.map(|a| completion_timing(a, run)))
            );
        }
        if runs::active(&state) {
            code = 75;
            continue;
        }
        let summary = json(run.join("summary.json"));
        if (!["delivered", "answered"].contains(&state.as_str()) || b(&summary, "policyRefused"))
            && code == 0
        {
            code = 1;
        }
        if output::json_enabled() {
            changes::print_changes(run);
            changes::print_shape(run);
        }
        if let Some(cleanup) = summary
            .get("cleanup")
            .filter(|c| c["terminated"].as_u64().unwrap_or(0) > 0)
        {
            let ports = cleanup["ports"]
                .as_array()
                .map(|v| {
                    v.iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let executables = cleanup["executables"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let mut details = Vec::new();
            if !executables.is_empty() {
                details.push(format!("进程 {executables}"));
            }
            if !ports.is_empty() {
                details.push(format!("端口 {ports}"));
            }
            println!(
                "note: 任务结束时终止了 {} 个后台进程{}；答复中提到的服务/地址已不可用",
                cleanup["terminated"],
                if details.is_empty() {
                    String::new()
                } else {
                    format!("（{}）", details.join("；"))
                }
            );
        }
        if let Some(count) = summary["cleanup"]["systemdStopped"]
            .as_u64()
            .filter(|n| *n > 0)
        {
            println!(
                "note: 任务结束时停止了 {count} 个 systemd 服务；答复中提到的服务/地址已不可用"
            );
        }
        if let Some(warnings) = summary["warnings"].as_array() {
            if output::json_enabled() {
                for warning in warnings {
                    println!("warning: {}", warning.as_str().unwrap_or(""));
                }
            } else if !warnings.is_empty() {
                println!(
                    "提示：{} 个 worktree 参考路径问题；详情：delegate status {} --json",
                    warnings.len(),
                    run.file_name().unwrap_or_default().to_string_lossy()
                );
            }
        }
        let notices_path = run.join("prepare-notices.txt");
        if notices_path.is_file() {
            let notices_text = read(&notices_path);
            for line in notices_text.lines().filter(|l| !l.trim().is_empty()) {
                eprintln!("{line}");
            }
        }
        let has_result = run.join("result.md").is_file();
        if has_result && show_result {
            // A truncated answer counts as reported, but clean keeps it until read in full.
            if print_answer(run, full) {
                touch(run.join(".truncated"));
            } else {
                fs::remove_file(run.join(".truncated")).ok();
                if matches!(state.as_str(), "delivered" | "answered")
                    && !b(&summary, "question")
                    && !b(&summary, "policyRefused")
                {
                    mark_result_read(run);
                }
            }
            if !output::json_enabled() {
                review::print_delivery_review(run);
            }
        } else if !output::json_enabled() && show_result {
            review::print_delivery_review(run);
        }
        if show_result || !has_result {
            touch(run.join(".delivered"));
            board::refresh_for_run(run);
            let meta = json(run.join("meta.json"));
            events::mark_run_read(&events::run_id_for(run, &meta));
            if !run.join(".truncated").exists() {
                work::on_run_result_read(run, &meta);
            }
            index::upsert_run(run);
        }
        settled_for_hints.push(run.clone());
    }
    if !output::json_enabled() && !settled_for_hints.is_empty() {
        for hint in work::hints_for_wait(&settled_for_hints) {
            println!("{hint}");
        }
    }
    if glanced {
        print_continue(next);
    }
    if code == 75 {
        let pending = runs
            .iter()
            .filter(|r| runs::active(&runs::state(r)))
            .map(|r| {
                r.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        eprintln!(
            "delegate: still running ({}); call wait again (exit 75)",
            pending.join(", ")
        );
    }
    code
}
/// Collect active or undelivered runs across all registered repositories (plus live `.slot`
/// machine runs), deduplicated by canonical path.
fn cross_repo_wait_runs(session: Option<&str>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::<PathBuf>::new();
    let mut push_unique = |p: PathBuf| {
        let key = fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if seen.insert(key) {
            out.push(p);
        }
    };
    let state = state_dir();
    for r in runs::all_runs() {
        if runs::active(&runs::state(&r)) || !r.join(".delivered").exists() {
            push_unique(r);
        }
    }
    for r in launch::machine_runs(&state) {
        push_unique(r);
    }
    if let Some(caller_id) = session {
        for (_, runs_dir) in work::registered_repo_runs(&state) {
            if !runs_dir.is_dir() {
                continue;
            }
            for r in runs::all_runs_in(&runs_dir) {
                let st = runs::state(&r);
                let live = runs::active(&st);
                let undelivered = !live
                    && !r.join(".delivered").exists()
                    && !r.join(".stopped").exists()
                    && st != "stopped";
                if (live || undelivered)
                    && json(r.join("meta.json"))["caller"].as_str() == Some(caller_id)
                {
                    push_unique(r);
                }
            }
        }
    }
    out.sort();
    out
}

/// The runs a `wait` collects; the flag is set when a hint about other sessions' runs was printed.
fn wait_list(pos: &[String], flags: &[String], quiet: bool) -> Res<(Vec<PathBuf>, bool)> {
    if has(flags, "--machine") {
        let mut v = launch::machine_runs(&state_dir());
        v.sort();
        v.dedup();
        let mut hinted = false;
        if !has(flags, "--all") {
            if let Some(caller) = caller() {
                let (own, other): (Vec<_>, Vec<_>) = v
                    .into_iter()
                    .partition(|run| json(run.join("meta.json"))["caller"].as_str() == Some(&caller));
                if !quiet {
                    runs::other_runs_hint_with(&other, true);
                }
                hinted = !other.is_empty();
                v = own;
            }
        }
        return Ok((v, hinted));
    }
    if !pos.is_empty() {
        return Ok((
            pos.iter()
                .map(|x| runs::resolve_head(x))
                .collect::<Res<Vec<_>>>()?,
            false,
        ));
    }
    let session = caller();
    let mut v = if has(flags, "--current-repo") {
        runs::all_runs()
            .into_iter()
            .filter(|r| runs::active(&runs::state(r)) || !r.join(".delivered").exists())
            .collect::<Vec<_>>()
    } else {
        cross_repo_wait_runs(session.as_deref())
    };
    let mut hinted = false;
    if !has(flags, "--all") {
        if let Some(caller) = session {
            let (own, other): (Vec<_>, Vec<_>) = v
                .into_iter()
                .partition(|run| json(run.join("meta.json"))["caller"].as_str() == Some(&caller));
            if !quiet {
                runs::other_runs_hint(&other);
            }
            hinted = !other.is_empty();
            v = own;
        }
    }
    Ok((v, hinted))
}
/// End every human-readable wait with the same command to collect the next result.
fn wait_remaining_hint(args: &[String]) {
    if output::json_enabled() {
        return;
    }
    let session = caller();
    let list = if has(args, "--machine") {
        launch::machine_runs(&state_dir())
    } else if has(args, "--current-repo") {
        runs::all_runs()
    } else {
        cross_repo_wait_runs(session.as_deref())
    };
    let left = list
        .iter()
        .filter(|run| runs::active(&runs::state(run)))
        .filter(|run| {
            session.as_ref().is_none_or(|id| {
                json(run.join("meta.json"))["caller"].as_str() == Some(id.as_str())
            })
        })
        .count();
    let undelivered = list
        .iter()
        .filter(|run| {
            let st = runs::state(run);
            !runs::active(&st)
                && !run.join(".delivered").exists()
                && !run.join(".stopped").exists()
                && st != "stopped"
        })
        .filter(|run| {
            session.as_ref().is_none_or(|id| {
                json(run.join("meta.json"))["caller"].as_str() == Some(id.as_str())
            })
        })
        .count();
    if undelivered > 0 {
        println!(
            "另有 {undelivered} 个本会话已结束未收取，运行 {} wait 收取",
            script().display()
        );
    }
    if left == 0 {
        println!("本会话没有在跑的任务");
    } else {
        let command = std::iter::once(script().to_string_lossy().into_owned())
            .chain(args.iter().cloned())
            .map(|arg| shell_quote(&arg))
            .collect::<Vec<_>>()
            .join(" ");
        println!("本会话还剩 {left} 个任务在跑；原样再跑：{command}");
    }
}
/// Blocks until the run leaves its active states: on its supervisor lock, else by polling.
fn until_finished(run: &Path, poll: f64) {
    while runs::active(&runs::state(run)) {
        let lock = run.join("supervisor.lock");
        if lock.is_file() {
            let _held = common::lock(&lock, false, false);
        }
        if runs::active(&runs::state(run)) {
            std::thread::sleep(Duration::from_secs_f64(poll));
        }
    }
}
/// `wait --any` reports the runs finished when the first one ends; `wait --stream` prints one
/// outcome line per run as each ends and exits once none is left. A stream started without
/// run arguments also picks up runs this caller starts while it waits.
#[allow(clippy::too_many_arguments)]
fn wait_each(
    mut list: Vec<PathBuf>,
    rescan: Option<(&[String], &[String])>,
    max: Option<f64>,
    stream: bool,
    show_progress: bool,
    full: bool,
    show_result: bool,
    next: &str,
) -> i32 {
    let initial = list.clone();
    let Ok((claimed, mut claims)) = waiting::claim(&list) else {
        return 2;
    };
    list = claimed;
    let mut covered = initial
        .into_iter()
        .filter(|run| !list.contains(run))
        .collect::<std::collections::HashSet<_>>();
    if list.is_empty() {
        return waiting::COVERED;
    }
    let mut begin = Instant::now();
    let already = list
        .iter()
        .filter(|run| !runs::active(&runs::state(run)))
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    let poll = setting("POLL", "1").parse::<f64>().unwrap_or(1.0).max(0.01);
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let mut watched = std::collections::HashSet::new();
    let mut reported = std::collections::HashSet::new();
    let mut code = 0;
    let mut delivered = false;
    loop {
        if show_progress {
            for run in &list {
                progress(
                    run,
                    run.file_name().unwrap_or_default().to_str().unwrap_or(""),
                );
            }
        }
        // Controller cancellation is not a delivery; dependency skips must be reported.
        reported.extend(list.iter().filter(|r| {
            r.join(".stopped").exists() || runs::state(r) == "stopped"
        }).cloned());
        let rescan_into =
            |list: &mut Vec<PathBuf>,
             covered: &mut std::collections::HashSet<PathBuf>,
             claims: &mut Vec<locks::LockGuard>| {
                if let Some((pos, flags)) = rescan {
                    if let Ok((fresh, _)) = wait_list(pos, flags, true) {
                        for run in fresh {
                            if !list.contains(&run) && !covered.contains(&run) {
                                if let Ok((fresh, held)) = waiting::claim(std::slice::from_ref(&run))
                                {
                                    if fresh.is_empty() {
                                        covered.insert(run);
                                    }
                                    list.extend(fresh);
                                    claims.extend(held);
                                }
                            }
                        }
                    }
                }
            };
        let done = list
            .iter()
            .filter(|r| !reported.contains(*r) && !runs::active(&runs::state(r)))
            .cloned()
            .collect::<Vec<_>>();
        if !done.is_empty() {
            if stream {
                for run in &done {
                    let state = runs::state(run);
                    let mut line = outcome_line(run, Some(completion_timing(&already, run)));
                    if run.join("result.md").is_file() && !run.join(".delivered").exists() {
                        // The answer is not printed here; name the command that reports it.
                        if output::json_enabled() {
                            line.pop();
                            line.push_str(&format!(
                                ",\"report\":{}}}",
                                json!(format!(
                                    "{} wait {}",
                                    script().display(),
                                    run.file_name().unwrap_or_default().to_string_lossy()
                                ))
                            ));
                        } else {
                            let suffix = format!(
                                " · 答复：delegate result {}",
                                run.file_name().unwrap_or_default().to_string_lossy()
                            );
                            if let Some(pos) = line.find('\n') {
                                line.insert_str(pos, &suffix);
                            } else {
                                line.push_str(&suffix);
                            }
                        }
                    }
                    println!("{line}");
                    let meta = json(run.join("meta.json"));
                    events::mark_run_read(&events::run_id_for(run, &meta));
                    let sum_pol = json(run.join("summary.json"));
                    if !["delivered", "answered"].contains(&state.as_str())
                        || b(&sum_pol, "policyRefused")
                    {
                        code = 1;
                    }
                }
                reported.extend(done);
            } else {
                // Report every run that settled since the previous wait:
                // full answer for the first, one-line summary + view command for the rest.
                rescan_into(&mut list, &mut covered, &mut claims);
                reported.extend(
                    list.iter()
                        .filter(|r| r.join(".stopped").exists() || runs::state(r) == "stopped")
                        .cloned(),
                );
                let mut settled = list
                    .iter()
                    .filter(|r| !reported.contains(*r) && !runs::active(&runs::state(r)))
                    .cloned()
                    .collect::<Vec<_>>();
                code = collect_timed(&settled[..1], None, false, full, show_result, Some(&already), next);
                reported.insert(settled[0].clone());
                // Pick up any sibling run that settled while the first answer was printed.
                rescan_into(&mut list, &mut covered, &mut claims);
                reported.extend(
                    list.iter()
                        .filter(|r| r.join(".stopped").exists() || runs::state(r) == "stopped")
                        .cloned(),
                );
                for run in &list {
                    if !reported.contains(run)
                        && !runs::active(&runs::state(run))
                        && !settled.contains(run)
                    {
                        settled.push(run.clone());
                    }
                }
                for run in settled.iter().skip(1) {
                    let state = runs::state(run);
                    let id = run
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    let notices_path = run.join("prepare-notices.txt");
        if notices_path.is_file() {
            let notices_text = read(&notices_path);
            for line in notices_text.lines().filter(|l| !l.trim().is_empty()) {
                eprintln!("{line}");
            }
        }
        let has_result = run.join("result.md").is_file();
                    let mut line = outcome_line(run, Some(completion_timing(&already, run)));
                    if has_result && show_result {
                        if output::json_enabled() {
                            line.pop();
                            line.push_str(&format!(
                                ",\"summaryOnly\":true,\"view\":{}}}",
                                json!(format!("{} result {id}", script().display()))
                            ));
                        } else {
                            line.push_str(&format!(" · 查看全文：{} result {id}", script().display()));
                        }
                        touch(run.join(".truncated"));
                        touch(run.join(".delivered"));
                    } else if !has_result {
                        touch(run.join(".delivered"));
                    }
                    println!("{line}");
                    events::mark_run_read(&id);
                    let sum_pol = json(run.join("summary.json"));
                    if (!["delivered", "answered"].contains(&state.as_str())
                        || b(&sum_pol, "policyRefused"))
                        && code == 0
                    {
                        code = 1;
                    }
                    reported.insert(run.clone());
                }
            }
            delivered = true;
            if !stream {
                break;
            }
        }
        let pending = list
            .iter()
            .filter(|r| !reported.contains(*r))
            .cloned()
            .collect::<Vec<_>>();
        if pending.is_empty() {
            break;
        }
        let left = max.map(|m| m - begin.elapsed().as_secs_f64());
        if left.is_some_and(|l| l <= 0.0) {
            for run in list
                .iter()
                .filter(|r| !reported.contains(*r) && runs::active(&runs::state(r)))
            {
                println!("{}", glance_line(run, next));
            }
            print_continue(next);
            if stream {
                begin = Instant::now();
                continue;
            }
            if stream || !delivered {
                code = 75;
            }
            break;
        }
        for run in pending {
            if watched.insert(run.clone()) {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    until_finished(&run, poll);
                    let _ = tx.send(());
                });
            }
        }
        let mut wake = left;
        if show_progress {
            wake = Some(wake.map_or(poll, |l| l.min(poll)));
        }
        if rescan.is_some() {
            wake = Some(wake.map_or(5.0, |l| l.min(5.0)));
        }
        match wake {
            Some(seconds) => {
                let _ = rx.recv_timeout(Duration::from_secs_f64(seconds.max(0.0)));
            }
            None => {
                let _ = rx.recv();
            }
        }
        if let Some((pos, flags)) = rescan {
            if let Ok((fresh, _)) = wait_list(pos, flags, true) {
                for run in fresh {
                    if !list.contains(&run) && !covered.contains(&run) {
                        if let Ok((fresh, held)) = waiting::claim(std::slice::from_ref(&run)) {
                            if fresh.is_empty() {
                                covered.insert(run);
                            }
                            list.extend(fresh);
                            claims.extend(held);
                        }
                    }
                }
            }
        }
    }
    let left = list
        .iter()
        .filter(|r| !reported.contains(*r) && runs::active(&runs::state(r)))
        .map(|r| {
            r.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    if !left.is_empty() {
        eprintln!(
            "delegate: {} still running ({}); {}",
            left.len(),
            left.join(", "),
            if stream {
                "call wait --stream again (exit 75)".to_string()
            } else if code == 75 {
                "call wait --any again (exit 75)".to_string()
            } else {
                format!("collect the next with: {} wait --any", script().display())
            }
        );
    } else if stream {
        eprintln!("delegate: all {} runs reported", reported.len());
    }
    code
}
fn completion_timing(already: &std::collections::HashSet<PathBuf>, run: &Path) -> &'static str {
    if already.contains(run) {
        "already-finished"
    } else {
        "finished-during-wait"
    }
}
fn print_archive_rescan(res: &archive::ArchiveRescan) {
    if output::json_enabled() {
        println!(
            "{}",
            json!({
                "archiveRescan": {
                    "path": res.root,
                    "count": res.count,
                    "bytes": res.bytes,
                    "updatedManifests": res.updated_manifests,
                    "previousBytes": res.previous_bytes,
                    "previousCount": res.previous_count,
                }
            })
        );
    } else if let (Some(pb), Some(pc)) = (res.previous_bytes, res.previous_count) {
        println!(
            "archive rescan: {} 份，共 {}（更新 {} 份 manifest；此前 {} 份 / {}）",
            res.count,
            format_bytes(res.bytes),
            res.updated_manifests,
            pc,
            format_bytes(pb),
        );
    } else {
        println!(
            "archive rescan: {} 份，共 {}（更新 {} 份 manifest）",
            res.count,
            format_bytes(res.bytes),
            res.updated_manifests,
        );
    }
}
fn clean(args: &[String]) -> Res<i32> {
    let (pos, flags, _) = parse_simple(
        args,
        &[
            "--finished",
            "--force",
            "--json",
            "--no-archive",
            "--sync",
            "--rescan-archive",
        ],
        &[],
    )?;
    if has(&flags, "--json") {
        output::enable_json();
    }
    let sync_gc = has(&flags, "--sync");
    let rescan_archive = has(&flags, "--rescan-archive");
    if pos.is_empty() && !has(&flags, "--finished") {
        if rescan_archive {
            let res = archive::rescan()?;
            print_archive_rescan(&res);
            return Ok(0);
        }
        return Err("clean requires runs, --finished, or --rescan-archive".into());
    }
    let mut targets = pos
        .iter()
        .map(|p| runs::resolve(p))
        .collect::<Res<Vec<_>>>()?;
    if has(&flags, "--finished") {
        for run in runs::all_runs().into_iter().chain(runs::archived_runs()) {
            // Archived records follow archive LRU policy, not `clean --finished`.
            if archive::is_record(&run) {
                continue;
            }
            if !runs::active(&runs::state(&run)) {
                let m = json(run.join("meta.json"));
                worktree::detect_applied(&run, &m);
            }
            if has(&flags, "--force")
                || (run.join(".delivered").exists()
                    && !run.join(".truncated").exists()
                    && !runs::unmerged(&run))
            {
                targets.push(run);
            } else if runs::unmerged(&run) {
                eprintln!(
                    "delegate: keep unmerged worktree run {}; apply it or pass --force",
                    run.file_name().unwrap_or_default().to_string_lossy()
                );
            } else if run.join(".delivered").exists() {
                eprintln!(
                    "delegate: keep run {0}; its answer was only shown truncated; read it with `result {0}` or pass --force",
                    run.file_name().unwrap_or_default().to_string_lossy()
                );
            } else if !runs::active(&runs::state(&run)) {
                eprintln!(
                    "delegate: keep unreported run {}; read it with wait/result or pass --force",
                    run.file_name().unwrap_or_default().to_string_lossy()
                );
            }
        }
    }
    targets.sort();
    targets.dedup();
    let mut staged_any = false;
    for run in targets {
        if archive::is_record(&run) {
            let meta = json(run.join("meta.json"));
            let id = notes::run_id(&run, &meta);
            let arch_path = run
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            if output::json_enabled() {
                println!(
                    "{}",
                    json!({"run": id, "alreadyCleaned": true, "removed": false, "archive": arch_path})
                );
            } else {
                println!("already cleaned & archived: {id} ({arch_path})");
            }
            continue;
        }
        let st = runs::state(&run);
        let name = run.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if pending_reply::status(&run).is_some() && !has(&flags, "--force") {
            eprintln!("delegate: keep run {name}; pending reply (stop cancels it)");
            continue;
        }
        if runs::active(&st) || runs::agent_alive(&run) {
            if runs::active(&st) {
                eprintln!("delegate: skip active run {name}");
            } else {
                eprintln!("delegate: skip active run {name} (its agent outlived the supervisor; stop it first: {} stop {name})",script().display());
            }
            continue;
        }
        // W026 fix: detect if changes are already in working tree or HEAD BEFORE archiving/removing,
        // even under `clean --force`, so `run_applied` dependencies unlock cleanly!
        let pre_meta = json(run.join("meta.json"));
        worktree::detect_applied(&run, &pre_meta);
        let note = if runs::unmerged(&run) {
            "; its worktree was never applied"
        } else {
            ""
        };
        let run_meta = json(run.join("meta.json"));
        let archived = if has(&flags, "--no-archive") {
            None
        } else {
            archive::preserve(&run)?
        };
        let pending_notes = notes::pending_count_for_run(&run);
        let work_keep_hint = work::on_run_cleaned(&run, &run_meta);
        let run_id_for_ev = events::run_id_for(&run, &run_meta);
        let repo_for_ev = events::repo_for_meta(&run_meta);

        // Fast foreground bookkeeping: delete refs, stop any tracked systemd scopes, update history/index/events,
        // and atomically rename run + unshared non-pooled worktree to trash (<5ms).
        refs::delete_run(&run, &run_meta);
        let units = cleanup::reclaim_on_clean(&run, sync_gc);
        let wt_path = s(&run_meta["worktree"], "path").to_string();
        let wt_source = s(&run_meta["worktree"], "source").to_string();
        let shared = !wt_path.is_empty()
            && runs::all_runs().iter().any(|r| {
                r != &run && s(&json(r.join("meta.json"))["worktree"], "path") == wt_path
            });
        // In synchronous `--sync` mode, reclaim compose containers BEFORE renaming the worktree away
        // so compose/docker checks that expect the project working_dir still present succeed.
        let sync_containers = if sync_gc && !shared && !wt_path.is_empty() {
            let wt_p = Path::new(&wt_path);
            if wt_p.is_absolute() && wt_p != Path::new(&wt_source) {
                cleanup::containers(wt_p, false)
            } else {
                cleanup::Containers::default()
            }
        } else {
            cleanup::Containers::default()
        };
        let job_path = gc::stage_clean_to_trash(&run, &run_meta, !shared)?;
        staged_any = true;
        history::record_clean(&run, &run_meta, archived.as_deref());
        index::remove_run(&run);
        board::refresh_for_meta(&run_meta);
        events::emit(
            &run_id_for_ev,
            &repo_for_ev,
            "cleaned",
            json!({
                "name": s(&run_meta, "name"),
                "state": st,
                "archive": archived.as_ref().map(|p| p.to_string_lossy().into_owned()),
            }),
        );
        events::mark_run_read(&run_id_for_ev);

        let containers = if sync_gc {
            let _ = gc::execute_job_inner(&job_path, false);
            sync_containers
        } else {
            cleanup::Containers::default()
        };
        let cleanup_note = if units > 0 {
            format!("；停止了 {units} 个 systemd 服务")
        } else {
            String::new()
        };
        let todo_note = if pending_notes > 0 {
            format!("；保留 {pending_notes} 条未完成待办")
        } else {
            String::new()
        };
        if output::json_enabled() {
            let mut obj = json!({"run":name,"state":st,"removed":true,"archive":archived,"systemdStopped":units,"containersRemoved":containers.removed,"containersRemovedNames":containers.removed_names,"containersKeptServing":containers.kept_serving,"diagnostic":containers.diagnostic,"gcAsync":!sync_gc});
            if pending_notes > 0 {
                obj["pendingNotes"] = json!(pending_notes);
            }
            println!("{obj}");
        } else {
            let container_note = if containers.removed > 0 {
                format!("；删除了容器 {}", containers.removed_names.join("、"))
            } else {
                String::new()
            };
            println!("removed {name} ({st}{note}{cleanup_note}{container_note}{todo_note})");
            if let Some(hint) = work_keep_hint {
                println!("{hint}");
            }
            containers.warn_kept();
            if let Some(path) = archived {
                println!("archive: {}", path.display());
            }
        }
    }
    if sync_gc {
        let orphans = cleanup::containers(&cache_dir().join("worktrees"), true);
        if output::json_enabled() {
            println!("{}", json!({"orphanCleanup":orphans.json()}));
        } else if orphans.removed > 0 {
            println!(
                "清理：删除了孤儿容器 {}（worktree 目录已不存在）",
                orphans.removed_names.join("、")
            );
        }
        orphans.warn_kept();
    } else {
        // Always kick off detached `_gc-worker` so trash removal, `git worktree prune`,
        // and orphan container sweeps run asynchronously without blocking foreground `clean`.
        if staged_any || has(&flags, "--finished") {
            gc::spawn_worker();
        }
        if output::json_enabled() {
            println!("{}", json!({"orphanCleanup":{"async":true}}));
        }
    }
    if rescan_archive {
        let res = archive::rescan()?;
        print_archive_rescan(&res);
    }
    Ok(0)
}
fn stop(args: &[String]) -> Res<i32> {
    let (args, flags, _) = parse_simple(args, &["--json"], &[])?;
    if has(&flags, "--json") {
        output::enable_json();
    }
    if args.is_empty() {
        return Err("stop requires at least one run".into());
    }
    for reference in args {
        let run = runs::resolve_head(&reference)?;
        crate::pending_reply::cancel(&run)?;
        let run = runs::resolve_head(&reference)?;
        crate::pending_reply::cancel(&run)?;
        let old = runs::state(&run);
        if !runs::active(&old) && runs::agent_alive(&run) {
            let pid = read(run.join("agent.pid"))
                .trim()
                .parse::<i32>()
                .unwrap_or(0);
            cleanup::record(&run, pid);
            kill_group(pid, 1.0);
        }
        if runs::active(&old) || runs::supervisor_alive(&run) {
            let pid = read(run.join("pid")).trim().parse::<i32>().unwrap_or(0);
            if pid > 0 {
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
            for tick in 0..60 {
                if run.join("exit_code").is_file() {
                    break;
                }
                if tick >= 1 && !runs::supervisor_alive(&run) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if !run.join("exit_code").is_file() {
                for file in ["agent.pid", "pi.pid", "pid"] {
                    let pid = read(run.join(file)).trim().parse::<i32>().unwrap_or(0);
                    if pid > 0 {
                        cleanup::record(&run, pid);
                        kill_group(pid, 1.0);
                    }
                }
                cleanup::record(&run, 0);
                let mut sum = json!({"state":"stopped"});
                if let Some(cleanup) = cleanup::summary(&run) {
                    sum["cleanup"] = cleanup;
                }
                finish_run(&run, sum, 1)?;
            } else {
                let mut sum = json(run.join("summary.json"));
                if s(&sum, "state") != "stopped" {
                    sum["state"] = json!("stopped");
                    write_json(run.join("summary.json"), &sum)?;
                    history::record_finish(&run);
                    let run_meta = json(run.join("meta.json"));
                    events::emit(
                        &events::run_id_for(&run, &run_meta),
                        &events::repo_for_meta(&run_meta),
                        "stopped",
                        json!({"name": s(&run_meta, "name"), "state": "stopped"}),
                    );
                }
            }
        }
        if !run.join("result.md").exists() {
            touch(run.join(".delivered"));
        }
        board::refresh_for_run(&run);
        println!("{}", status_line(&run));
    }
    Ok(0)
}
fn filter_patch_bytes(data: &[u8], paths: &[String]) -> Res<Vec<u8>> {
    if paths.is_empty() {
        return Ok(data.to_vec());
    }
    let mut out_bytes = Vec::new();
    let mut blocks = Vec::<Vec<u8>>::new();
    for line in data.split_inclusive(|b| *b == b'\n') {
        if let Some(name) = line.strip_prefix(b"# binary/large file omitted: ") {
            if let Ok(name) = serde_json::from_slice::<String>(name) {
                if paths.iter().any(|p| crate::changes::under_path(&name, p)) {
                    out_bytes.extend_from_slice(line);
                }
            }
            continue;
        }
        if line.starts_with(b"diff --git ") || blocks.is_empty() {
            blocks.push(Vec::new());
        }
        blocks.last_mut().unwrap().extend_from_slice(line);
    }
    for block in blocks {
        if !block.starts_with(b"diff --git ") {
            continue;
        }
        let mut child = git_command()
            .args(["apply", "--numstat", "-z"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&block)
            .map_err(|e| e.to_string())?;
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into());
        }
        let name = out
            .stdout
            .splitn(3, |b| *b == b'\t')
            .nth(2)
            .unwrap_or_default();
        let name = String::from_utf8_lossy(name)
            .trim_end_matches('\0')
            .to_string();
        if paths.iter().any(|p| crate::changes::under_path(&name, p)) {
            out_bytes.extend_from_slice(&block);
        }
    }
    Ok(out_bytes)
}

fn archived_patch_file(run: &Path) -> Option<PathBuf> {
    if run.join("applied.patch").is_file() {
        Some(run.join("applied.patch"))
    } else if run.join("changes.patch").is_file() {
        Some(run.join("changes.patch"))
    } else {
        None
    }
}

fn print_archived_diff(run: &Path, paths: &[String], stat_only: bool) -> Res<i32> {
    let Some(patch_file) = archived_patch_file(run) else {
        return Err(format!("archived run {} has no retained patch", run.display()));
    };
    let meta = json(run.join("meta.json"));
    let id = notes::run_id(run, &meta);
    eprintln!(
        "delegate: {id} 已清理归档，从归档补丁读取（完整信息见 `delegate show {id}`）：{}",
        patch_file.display()
    );
    let data = fs::read(&patch_file).map_err(|e| format!("{}: {e}", patch_file.display()))?;
    let filtered = filter_patch_bytes(&data, paths)?;
    if stat_only {
        let mut child = git_command()
            .args(["apply", "--stat"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&filtered)
            .map_err(|e| e.to_string())?;
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if out.status.success() {
            io::stdout().write_all(&out.stdout).map_err(|e| e.to_string())?;
            return Ok(0);
        }
    }
    io::stdout().write_all(&filtered).map_err(|e| e.to_string())?;
    Ok(0)
}

fn diff(args: &[String]) -> Res<i32> {
    let (options, separated_paths) = match args.iter().position(|arg| arg == "--") {
        Some(index) => (&args[..index], &args[index + 1..]),
        None => (args, &[][..]),
    };
    let (pos, flags, _) = parse_simple(options, &["--stat", "--total", "--risky", "--round"], &[])?;
    let reference = pos.first().map(String::as_str).unwrap_or("last");
    let run = match runs::resolve_head(reference) {
        Ok(r) => r,
        Err(err) => match history::resolve_archive_record(reference) {
            Some(rec) => rec,
            None => return Err(err),
        },
    };
    let filter_paths: Vec<String> = pos
        .iter()
        .skip(1)
        .chain(separated_paths.iter())
        .cloned()
        .collect();
    if has(&flags, "--risky") {
        let meta = json(run.join("meta.json"));
        let round_only = has(&flags, "--round") && !has(&flags, "--total");
        let (rendered, _) = card::render_risky_diff(&run, &meta, round_only, &filter_paths)?;
        print!("{rendered}");
        return Ok(0);
    }
    if archive::is_record(&run) && archived_patch_file(&run).is_some() {
        return print_archived_diff(&run, &filter_paths, has(&flags, "--stat"));
    }
    let (meta, top, mut before, after) = match changes::chain_changes(&run) {
        Ok(x) => x,
        Err(err) => {
            if archived_patch_file(&run).is_some() {
                return print_archived_diff(&run, &filter_paths, has(&flags, "--stat"));
            }
            return Err(err);
        }
    };
    let before_label = if has(&flags, "--total") {
        let (total_before, label) = worktree::effective_total_base(&top, &meta);
        before = total_before;
        label
    } else if b(&meta, "netBaseAdvanced") {
        if let Some(tree) = changes::net_base_tree(&top, &meta) {
            before = tree;
            "net-base"
        } else {
            "base"
        }
    } else {
        "base"
    };
    let missing = refs::missing_in_top(
        &top,
        &run,
        &meta,
        &[(before_label, &before), ("after", &after)],
    );
    if !missing.is_empty() {
        if archive::is_record(&run) && archived_patch_file(&run).is_some() {
            return print_archived_diff(&run, &filter_paths, has(&flags, "--stat"));
        }
        return Err(refs::recovery_message(&run, &meta, &missing));
    }
    let mut cmd = git_command();
    cmd.arg("-C").arg(&top).args([
        "diff",
        "--no-renames",
        if unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1 {
            "--color=always"
        } else {
            "--color=never"
        },
    ]);
    if has(&flags, "--stat") {
        cmd.arg("--stat");
    }
    cmd.args([&before, &after, "--"]);
    cmd.args(pos.iter().skip(1));
    cmd.args(separated_paths);
    let status = cmd.status().map_err(|e| e.to_string())?;
    if !status.success() && run.join("changes.patch").is_file() && !has(&flags, "--total") {
        print!("{}", read(run.join("changes.patch")));
        return Ok(0);
    }
    Ok(status.code().unwrap_or(1))
}
fn patch(args: &[String]) -> Res<i32> {
    let split = args.iter().position(|a| a == "--").unwrap_or(args.len());
    if split != 1 {
        return Err("patch requires one run, followed by optional -- paths".into());
    }
    let run = match runs::resolve(&args[0]) {
        Ok(r) => r,
        Err(err) => match history::resolve_archive_record(&args[0]) {
            Some(rec) => rec,
            None => return Err(err),
        },
    };
    let file = if run.join("applied.patch").is_file() {
        run.join("applied.patch")
    } else {
        run.join("changes.patch")
    };
    let data = fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let paths = args.get(split + 1..).unwrap_or_default();
    let filtered = filter_patch_bytes(&data, paths)?;
    io::stdout().write_all(&filtered).map_err(|e| e.to_string())?;
    Ok(0)
}
fn adopt_command(args: &[String]) -> Res<i32> {
    let (pos, flags, _) = parse_simple(args, &["--json"], &[])?;
    if has(&flags, "--json") {
        output::enable_json();
    }
    if pos.is_empty() {
        return Err("adopt requires at least one run (usage: delegate adopt <run>...)".into());
    }
    let Some((new_caller, new_source)) = caller_source() else {
        return Err(
            "adopt requires an active caller session (DELEGATE_CALLER / CLAUDE_CODE_SESSION_ID / CODEX_THREAD_ID / PI_SESSION_ID)"
                .into(),
        );
    };
    touch_caller_session();

    let mut seen_runs = std::collections::HashSet::<PathBuf>::new();
    for reference in &pos {
        let resolved = runs::resolve(reference)?;
        let chain = runs::chain_rounds(&resolved);
        if chain.is_empty() {
            continue;
        }
        // Skip if this chain was already adopted by an earlier argument in the same invocation.
        if chain.iter().all(|r| seen_runs.contains(r)) {
            continue;
        }
        let head = chain.last().cloned().unwrap_or_else(|| resolved.clone());
        let head_meta_before = json(head.join("meta.json"));
        let head_id = events::run_id_for(&head, &head_meta_before);
        let head_name = s(&head_meta_before, "name").to_string();
        let head_repo = events::repo_for_meta(&head_meta_before);
        let from_caller = s(&head_meta_before, "caller").to_string();

        let mut round_ids: Vec<String> = Vec::with_capacity(chain.len());
        let mut work_ids: Vec<String> = Vec::new();
        for round in &chain {
            seen_runs.insert(round.clone());
            let mut meta = json(round.join("meta.json"));
            if !meta.is_object() {
                continue;
            }
            let rid = events::run_id_for(round, &meta);
            let wid = s(&meta, "workId").to_string();
            if !wid.is_empty() && !work_ids.contains(&wid) {
                work_ids.push(wid);
            }
            meta["caller"] = json!(&new_caller);
            meta["callerSource"] = json!(new_source);
            write_json(round.join("meta.json"), &meta)?;
            index::upsert_run(round);
            board::refresh_for_run(round);
            round_ids.push(rid);
        }

        work::on_runs_adopted(&round_ids, &work_ids, &new_caller);

        events::emit(
            &head_id,
            &head_repo,
            "adopted",
            json!({
                "name": head_name,
                "fromCaller": from_caller,
                "toCaller": new_caller,
                "callerSource": new_source,
                "rounds": round_ids,
            }),
        );

        if output::json_enabled() {
            println!(
                "{}",
                json!({
                    "run": head_id,
                    "name": head_name,
                    "fromCaller": from_caller,
                    "toCaller": new_caller,
                    "callerSource": new_source,
                    "rounds": round_ids,
                })
            );
        } else {
            let from_disp = if from_caller.is_empty() {
                "(无)"
            } else {
                from_caller.as_str()
            };
            let rounds_note = if round_ids.len() > 1 {
                format!("，共 {} 轮", round_ids.len())
            } else {
                String::new()
            };
            println!(
                "已接手 {head_id}（{head_name}{rounds_note}）：caller {from_disp} → {new_caller}"
            );
        }
    }
    Ok(0)
}

fn mark_applied_command(args: &[String]) -> Res<i32> {
    let (pos, flags, values) = parse_simple(
        args,
        &["--check-files", "--json"],
        &["--commit"],
    )?;
    if has(&flags, "--json") {
        output::enable_json();
    }
    if pos.len() != 1 {
        return Err(
            "usage: delegate mark-applied <run> [--commit <sha>] [--check-files] [--json]".into(),
        );
    }
    let resolved = runs::resolve(&pos[0])?;
    let run = if runs::continues_conversation(&resolved) {
        runs::latest(resolved)
    } else {
        resolved
    };
    if runs::active(&runs::state(&run)) {
        return Err(format!(
            "{} is still running",
            run.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    let commit_arg = value(&values, "--commit");
    let check_files = has(&flags, "--check-files");
    let res = worktree::mark_applied(&run, commit_arg, check_files)?;

    let mut applied_ids = Vec::new();
    let mut committed_runs = std::collections::HashSet::new();
    for (rdir, meta, applied_doc) in &res.marked_runs {
        let rid = events::run_id_for(rdir, meta);
        let arch = archive::status(rdir).map(|m| PathBuf::from(s(&m, "path")));
        history::record_apply(
            rdir,
            meta,
            s(applied_doc, "at"),
            &res.commit_sha,
            "manual",
            arch.as_deref(),
        );
        events::emit(
            &rid,
            &events::repo_for_meta(meta),
            "applied",
            json!({
                "name": s(meta, "name"),
                "head": res.commit_sha,
                "commit": res.commit_sha,
                "appliedBy": "manual",
            }),
        );
        eta::record_calibration_applied(rdir, meta, s(applied_doc, "at"));
        events::mark_run_read(&rid);
        index::upsert_run(rdir);
        board::refresh_for_run(rdir);
        applied_ids.push(rid.clone());
        committed_runs.insert(rid);
    }

    let work_opts = work::ApplyWorkOptions {
        committed_runs,
        ..Default::default()
    };
    let work_hints = work::on_runs_applied_with_options(&res.marked_runs, &work_opts);
    work::check_stale_and_expired();
    dag_queue::advance();
    hook::record_action("apply", &applied_ids, None, false);
    for (rdir, _, _) in &res.marked_runs {
        pool::release_if_applied_chain(rdir);
    }
    premerge::spawn_worker_force(&res.source);

    let head_meta = json(run.join("meta.json"));
    let head_id = events::run_id_for(&run, &head_meta);
    let head_name = s(&head_meta, "name").to_string();
    let short_sha = &res.commit_sha[..res.commit_sha.len().min(7)];

    if output::json_enabled() {
        println!(
            "{}",
            json!({
                "run": head_id,
                "name": head_name,
                "commit": res.commit_sha,
                "appliedAt": res.applied_at,
                "appliedBy": "manual",
                "markedRuns": applied_ids,
                "checkedFiles": res.checked_files,
                "missingFiles": res.missing_files,
                "workHints": work_hints,
            })
        );
    } else {
        println!(
            "已标记为已合入：{}（{}）· commit {}（manual）",
            head_name, head_id, short_sha
        );
        if !res.missing_files.is_empty() {
            eprintln!(
                "提示：以下净改动文件未在 commit {} 中出现（可能是拆分提交或手工改名）：{}",
                short_sha,
                res.missing_files.join("、")
            );
        }
        for hint in work_hints {
            println!("{hint}");
        }
    }
    Ok(0)
}

fn files_command(args: &[String]) -> Res<i32> {
    let (pos, flags, _) = parse_simple(args, &["--json"], &[])?;
    if pos.len() > 1 {
        return Err(format!("unrecognized arguments: {}", pos[1..].join(" ")));
    }
    let reference = pos.first().map(String::as_str).unwrap_or("last");
    let resolved = runs::resolve(reference)?;
    let run = if runs::continues_conversation(&resolved) {
        runs::latest(resolved)
    } else {
        resolved
    };
    let meta = json(run.join("meta.json"));
    let run_id = {
        let r = s(&meta, "run");
        if !r.is_empty() {
            r.to_string()
        } else {
            run.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }
    };
    let files = worktree::run_files(&run);
    if has(&flags, "--json") || output::json_enabled() {
        println!(
            "{}",
            json!({
                "run": run_id,
                "count": files.len(),
                "files": files,
            })
        );
    } else {
        for path in &files {
            println!("{path}");
        }
    }
    Ok(0)
}
fn apply(args: &[String]) -> Res<i32> {
    let apply_started_at = iso();
    let apply_t0 = std::time::Instant::now();
    let (pos, flags, kv) = parse_simple(
        args,
        &[
            "--dry-run",
            "--merge",
            "--allow-dirty-overlap",
            "--verify",
            "--no-verify",
            "--keep",
            "--keep-open",
            "--force",
            "--keep-commits",
            "--commit",
            "--json",
        ],
        &["--closes"],
    )?;
    let closes_work = value(&kv, "--closes").map(str::to_string);
    if let Some(ref raw_w) = closes_work {
        let _ = work::validate_work_id(raw_w)?;
    }
    let keep_open = has(&flags, "--keep-open");
    let force_apply = has(&flags, "--force");
    if keep_open && closes_work.is_some() {
        return Err("--keep-open and --closes are exclusive".into());
    }
    if has(&flags, "--json") {
        output::enable_json();
    }
    if has(&flags, "--verify") && has(&flags, "--no-verify") {
        return Err("--verify and --no-verify are exclusive".into());
    }
    let commit_mode = has(&flags, "--commit");
    if commit_mode && has(&flags, "--keep-commits") {
        return Err("--commit and --keep-commits are exclusive".into());
    }
    let names = if pos.is_empty() {
        vec!["last".to_string()]
    } else {
        pos.clone()
    };
    let mut targets = Vec::new();
    for name in &names {
        let resolved = runs::resolve(name)?;
        let run = if runs::continues_conversation(&resolved) {
            runs::latest(resolved)
        } else {
            resolved
        };
        if runs::active(&runs::state(&run)) {
            return Err(format!(
                "{} is still running",
                run.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
        if !targets.contains(&run) {
            targets.push(run);
        }
    }
    let forced = if has(&flags, "--verify") {
        Some(true)
    } else if has(&flags, "--no-verify") {
        Some(false)
    } else {
        None
    };
    let dry = has(&flags, "--dry-run");
    // Runs apply in the order given and stop at the first that fails; their acceptance
    // reruns once on the merged tree, after the last merge (field notes 16).
    struct DoneApply {
        run: PathBuf,
        name: String,
        source: String,
        conclusion: serde_json::Value,
        commands: Vec<String>,
        written_files: Vec<String>,
        regenerated_files: Vec<String>,
        had_dirty_merge: bool,
        already_applied: bool,
    }
    let mut done: Vec<DoneApply> = Vec::new();
    let mut applied_runs = Vec::new();
    let mut code = 0;
    let mut plan = Vec::<String>::new();
    for run in &targets {
        let meta = json(run.join("meta.json"));
        let keep_commits =
            !commit_mode && (has(&flags, "--keep-commits") || b(&meta["defaults"], "keepCommits"));
        let mut outcome = worktree::apply(
            run,
            has(&flags, "--merge"),
            dry,
            keep_commits,
            commit_mode,
            has(&flags, "--allow-dirty-overlap"),
        )?;
        if !outcome.already_applied && !dry && run.join(".applied").exists() {
            if !applied_runs.contains(run) {
                applied_runs.push(run.clone());
            }
            let applied_json = json(run.join(".applied"));
            let run_meta = json(run.join("meta.json"));
            if b(&run_meta, "isMerge") {
                merge::mark_merge_run_applied(run, s(&applied_json, "head"));
            }
            let path = s(&run_meta["worktree"], "path").to_string();
            let run_ns = run_meta["startedNs"].as_u64().unwrap_or(0);
            for other in runs::all_runs() {
                if other == *run {
                    continue;
                }
                let mut other_meta = json(other.join("meta.json"));
                let other_ns = other_meta["startedNs"].as_u64().unwrap_or(0);
                if s(&other_meta["worktree"], "path") == path
                    && !runs::active(&runs::state(&other))
                    && (other_ns <= run_ns || !runs::continues_conversation(&other))
                {
                    let _ = write(other.join(".applied"), read(run.join(".applied")));
                    if !s(&applied_json, "at").is_empty() {
                        other_meta["appliedAt"] = applied_json["at"].clone();
                    }
                    if !s(&applied_json, "head").is_empty() {
                        other_meta["appliedHead"] = applied_json["head"].clone();
                    }
                    let _ = write_json(other.join("meta.json"), &other_meta);
                    if run.join(".sync-base").is_file() {
                        let _ = write(other.join(".sync-base"), read(run.join(".sync-base")));
                    }
                    refs::pin_run(&other, &other_meta);
                    if !applied_runs.contains(&other) {
                        applied_runs.push(other);
                    }
                }
            }
        }
        if outcome.code == 0 && (!outcome.already_applied || (forced == Some(true) && !dry)) {
            for command in worktree::verify_plan(run, forced, &mut outcome.conclusion)? {
                if !plan.contains(&command) {
                    plan.push(command);
                }
            }
        }
        code = outcome.code;
        // Capture acceptance before cleaning, since clean removes the recorded task metadata.
        let commands = worktree::acceptance(&meta);
        done.push(DoneApply {
            run: run.clone(),
            name: s(&meta, "name").to_string(),
            source: s(&meta["worktree"], "source").to_string(),
            conclusion: outcome.conclusion,
            commands,
            written_files: outcome.written_files,
            regenerated_files: outcome.regenerated_files,
            had_dirty_merge: outcome.had_dirty_merge,
            already_applied: outcome.already_applied,
        });
        if code != 0 {
            break;
        }
    }
    let mut verify = serde_json::Value::Null;
    if code == 0 && !plan.is_empty() {
        if let Some(last) = done.last_mut() {
            verify = worktree::run_verify(&last.run, &plan);
            if verify["ok"] != json!(true) {
                code = 1;
            }
            last.conclusion["verify"] = verify.clone();
        }
    }
    if code == 0 && !dry && commit_mode {
        let mut sources = Vec::<String>::new();
        for item in &done {
            if item.already_applied {
                continue;
            }
            if !item.source.is_empty() && !sources.contains(&item.source) {
                sources.push(item.source.clone());
            }
        }
        for src in sources {
            let mut paths_set = std::collections::BTreeSet::<String>::new();
            let mut titles = Vec::<String>::new();
            let mut run_names = Vec::<String>::new();
            let mut dirty_merge = false;
            let mut indices = Vec::new();
            for (idx, item) in done.iter().enumerate() {
                if item.already_applied || item.source != src {
                    continue;
                }
                indices.push(idx);
                for p in &item.written_files {
                    paths_set.insert(p.clone());
                }
                for p in &item.regenerated_files {
                    paths_set.insert(p.clone());
                }
                if let Some(arr) = item.conclusion["commits"].as_array() {
                    for c in arr {
                        let t = s(c, "title");
                        if !t.is_empty() {
                            titles.push(t.to_string());
                        }
                    }
                }
                if !item.name.is_empty() {
                    run_names.push(item.name.clone());
                }
                dirty_merge |= item.had_dirty_merge;
            }
            let paths: Vec<String> = paths_set.into_iter().collect();
            if paths.is_empty() {
                continue;
            }
            let src_runs: Vec<PathBuf> = applied_runs
                .iter()
                .filter(|r| s(&json(r.join("meta.json"))["worktree"], "source") == src)
                .cloned()
                .collect();
            match worktree::commit_applied(
                Path::new(&src),
                &src_runs,
                &paths,
                &titles,
                &run_names,
                dirty_merge,
            ) {
                Ok((head, subject)) => {
                    for idx in indices {
                        done[idx].conclusion["apply"]["committed"] = json!(true);
                        if !head.is_empty() {
                            done[idx].conclusion["apply"]["commit"] = json!(&head);
                        }
                        done[idx].conclusion["apply"]["commitSubject"] = json!(&subject);
                    }
                    // HEAD advanced: wake `_merge-worker` for any other pending / blocked_mainline runs in this repo.
                    premerge::spawn_worker_force(Path::new(&src));
                }
                Err(err) => {
                    code = 1;
                    if let Some(&last_idx) = indices.last() {
                        done[last_idx].conclusion["apply"]["ok"] = json!(false);
                        done[last_idx].conclusion["apply"]["failedStep"] = json!("commit");
                        done[last_idx].conclusion["apply"]["failureDetail"] = json!(err);
                        done[last_idx].conclusion["apply"]["writtenFiles"] = json!(&paths);
                    }
                    break;
                }
            }
        }
    }
    for item in &done {
        let run = &item.run;
        let conclusion = &item.conclusion;
        let commands = &item.commands;
        if item.already_applied {
            let run_id = s(conclusion, "run");
            let at = s(&conclusion["apply"], "appliedAt");
            if output::json_enabled() {
                println!("{}", format_status(conclusion.clone()));
                eprintln!("delegate: {run_id} 已于 {at} 合入");
            } else {
                println!("{run_id} 已于 {at} 合入");
            }
            continue;
        }
        let final_line = if conclusion["acceptStillValid"] == json!(true) {
            "验收仍然有效（仓库快照范围）".to_string()
        } else if conclusion["apply"]["ok"] == json!(true)
            && !dry
            && !verify.is_null()
            && conclusion["verify"].get("skipped").is_none()
        {
            String::new()
        } else if conclusion["apply"]["ok"] == json!(true)
            && !dry
            && b(conclusion, "applyVerifyDisabled")
            && conclusion["acceptStillValid"] == json!(false)
            && !b(conclusion, "acceptPending")
            && s(conclusion, "acceptValidityReason") != "acceptance failed"
        {
            if commands.is_empty() {
                "主干已前进，验收快照失效；本仓库配置合入后不自动验收（任务没有验收命令）"
                    .to_string()
            } else {
                format!(
                    "主干已前进，验收快照失效；本仓库配置合入后不自动验收，批量门禁时跑：{}",
                    commands.join("；")
                )
            }
        } else {
            let validity = if b(conclusion, "acceptPending") {
                "验收未完成（排队/基础设施）"
            } else if conclusion["acceptStillValid"] == json!(false) {
                "验收无效"
            } else {
                "验收有效性未确认"
            };
            if commands.is_empty() {
                format!("{validity}；任务没有验收命令")
            } else if b(conclusion, "acceptPending") {
                format!("{validity}；重排：{} apply --verify --keep {}；或本地：DELEGATE_REMOTE=0 {} lane {}",
                    shell_quote(&script().to_string_lossy()), run.file_name().unwrap_or_default().to_string_lossy(),
                    shell_quote(&script().to_string_lossy()), shell_quote(&commands.join(" && ")))
            } else {
                format!("{validity}；需要在主干重跑：{}", commands.join("；"))
            }
        };
        let blind_note = output::blind_note(&conclusion["acceptBlind"]);
        println!("{}", format_status(conclusion.clone()));
        if !output::json_enabled() && conclusion["fastForward"].is_object() {
            let ff = &conclusion["fastForward"];
            if b(ff, "hit") {
                let bh = s(ff, "baseHead");
                let short = &bh[..7.min(bh.len())];
                let ms = ff["ms"].as_u64().unwrap_or(0);
                if dry {
                    println!("快进预演 ✓（基于 {short}）");
                } else if conclusion["acceptStillValid"] == json!(true) {
                    println!("快进合入 ✓（基于 {short}，跳过重跑验收，{ms}ms）");
                } else {
                    println!("快进合入 ✓（基于 {short}，{ms}ms）");
                }
            } else {
                let reason = s(ff, "reason");
                if !reason.is_empty() && s(ff, "missCode") != "missing_premerge" {
                    println!("{reason}");
                }
            }
        }
        if !output::json_enabled() && !dry && conclusion["apply"]["ok"] == json!(true) {
            let mut touched = std::collections::BTreeSet::<&str>::new();
            for p in &item.written_files {
                touched.insert(p.as_str());
            }
            for p in &item.regenerated_files {
                touched.insert(p.as_str());
            }
            let count = touched.len();
            if b(&conclusion["apply"], "committed") {
                let hash = s(&conclusion["apply"], "commit");
                let short = &hash[..7.min(hash.len())];
                let subject = s(&conclusion["apply"], "commitSubject");
                println!("已提交 {short} {subject}（{count} 个文件）");
            } else if !b(&conclusion["apply"], "keepCommits") && count > 0 {
                let titles: Vec<&str> = conclusion["commits"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| s(c, "title"))
                    .filter(|t| !t.is_empty())
                    .collect();
                if titles.is_empty() {
                    println!("改动未提交：{count} 个文件");
                } else if titles.len() == 1 {
                    println!("改动未提交：{count} 个文件；同事原提交标题：{}", titles[0]);
                } else {
                    println!("改动未提交：{count} 个文件；同事原提交标题：");
                    for title in titles {
                        println!("  - {title}");
                    }
                }
            }
        }
        if !output::json_enabled() && !(final_line.is_empty() && blind_note.is_empty()) {
            println!(
                "{final_line}{}",
                if blind_note.is_empty() || final_line.is_empty() {
                    blind_note.clone()
                } else {
                    format!("；{blind_note}")
                }
            );
        }
    }
    if !output::json_enabled() && !verify.is_null() {
        println!(
            "{}",
            if verify["ok"] == json!(true) {
                format!("合并后验收通过：{}", plan.join("；"))
            } else if b(&verify, "notExecuted") || b(&verify, "infrastructureFailure") {
                format!("合并已写入；{}；日志 {}", s(&verify, "next"), s(&verify, "log"))
            } else {
                format!(
                    "合并后验收失败：{}；合并已写入主干，在主干修或 reply 同事；日志 {}",
                    s(&verify, "command"),
                    s(&verify, "log")
                )
            }
        );
    }
    if !output::json_enabled() {
        for item in &done {
            let conclusion = &item.conclusion;
            if conclusion["apply"]["ok"] == json!(false) {
                let step = s(&conclusion["apply"], "failedStep");
                let step_zh = match step {
                    "dirty-overlap" => "源工作区目标文件已有未提交改动",
                    "conflicts" => "文件冲突",
                    "skipped-conflicts" => "跳过无法合并的文件",
                    "conflict-markers" => "存在冲突标记",
                    "regeneration" => "重新生成生成物",
                    "cherry-pick" => "cherry-pick 保留提交",
                    "commit" => "自动提交",
                    _ => "应用",
                };
                let detail = s(&conclusion["apply"], "failureDetail");
                let written = strings(&conclusion["apply"]["writtenFiles"]);
                let written_note = if written.is_empty() {
                    "未写入源工作区".to_string()
                } else {
                    let shown: Vec<_> = written.iter().take(10).cloned().collect();
                    if written.len() > 10 {
                        format!(
                            "以下 {} 个文件已写入源工作区：{} 等",
                            written.len(),
                            shown.join("、")
                        )
                    } else {
                        format!(
                            "以下 {} 个文件已写入源工作区：{}",
                            written.len(),
                            shown.join("、")
                        )
                    }
                };
                println!(
                    "!!! apply 失败（阶段：{step_zh} / {step}{}）；{written_note}",
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!("；{detail}")
                    }
                );
                if let Some(nexts) = conclusion["apply"]["next"].as_array() {
                    for next_cmd in nexts.iter().filter_map(serde_json::Value::as_str) {
                        if !next_cmd.is_empty() {
                            println!("  下一步：{next_cmd}");
                        }
                    }
                }
            }
        }
    }
    if !output::json_enabled() && !dry {
        for item in &done {
            let conclusion = &item.conclusion;
            if let Some(paths) = conclusion["apply"]["conflictMarkers"]
                .as_array()
                .filter(|p| !p.is_empty())
            {
                println!(
                    "!!! applied with conflict markers / 有冲突待解决: {}",
                    paths
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }
    if !dry {
        let apply_finished_at = iso();
        let apply_elapsed_ms = apply_t0.elapsed().as_millis() as u64;
        let mut applied_records: Vec<(PathBuf, Value, Value)> = Vec::new();
        for run in &applied_runs {
            let applied = json(run.join(".applied"));
            if !applied.is_object() {
                continue;
            }
            let meta = json(run.join("meta.json"));
            let arch = archive::status(run).map(|m| PathBuf::from(s(&m, "path")));
            let by = s(&applied, "appliedBy");
            history::record_apply(
                run,
                &meta,
                s(&applied, "at"),
                s(&applied, "head"),
                if by.is_empty() { "delegate" } else { by },
                arch.as_deref(),
            );
            let run_id_ev = events::run_id_for(run, &meta);
            let done_item = done.iter().find(|d| d.run == *run);
            let run_started = done_item
                .map(|d| s(&d.conclusion, "startedAt"))
                .filter(|v| !v.is_empty())
                .unwrap_or(&apply_started_at);
            let run_finished = done_item
                .map(|d| s(&d.conclusion, "finishedAt"))
                .filter(|v| !v.is_empty())
                .unwrap_or(&apply_finished_at);
            let run_elapsed_ms = done_item
                .and_then(|d| d.conclusion["elapsedMs"].as_u64())
                .unwrap_or(apply_elapsed_ms);
            let mut ev_payload = json!({
                "name": s(&meta, "name"),
                "head": s(&applied, "head"),
                "appliedBy": if by.is_empty() { "delegate" } else { by },
                "startedAt": run_started,
                "finishedAt": run_finished,
                "elapsedMs": run_elapsed_ms,
            });
            if let Some(d) = done_item {
                if d.conclusion["fastForward"].is_object() {
                    ev_payload["fastForward"] = json!(b(&d.conclusion["fastForward"], "hit"));
                }
                if let Some(gms) = d.conclusion["applyTiming"]["generateMs"].as_u64() {
                    ev_payload["generateMs"] = json!(gms);
                }
                if b(&d.conclusion["applyTiming"], "regenerateTargetIsolated") {
                    ev_payload["regenerateTargetIsolated"] = json!(true);
                }
            }
            events::emit(
                &run_id_ev,
                &events::repo_for_meta(&meta),
                "applied",
                ev_payload,
            );
            eta::record_calibration_applied(run, &meta, s(&applied, "at"));
            events::mark_run_read(&run_id_ev);
            index::upsert_run(run);
            board::refresh_for_run(run);
            applied_records.push((run.clone(), meta, applied));
        }
        if !applied_records.is_empty() {
            let applied_ids: Vec<String> = applied_records
                .iter()
                .map(|(r, m, _)| events::run_id_for(r, m))
                .collect();
            hook::record_action_timed(
                "apply",
                &applied_ids,
                None,
                false,
                &apply_started_at,
                &apply_finished_at,
                apply_elapsed_ms,
            );
            let mut committed_runs = std::collections::HashSet::new();
            let mut committed_worktrees = std::collections::HashSet::new();
            let mut verify_failed_runs = std::collections::HashSet::new();
            let post_verify_failed = !verify.is_null() && verify["ok"] == json!(false);
            for d in &done {
                let is_committed = b(&d.conclusion["apply"], "committed")
                    || b(&d.conclusion["apply"], "keepCommits");
                if is_committed {
                    let dm = json(d.run.join("meta.json"));
                    committed_runs.insert(events::run_id_for(&d.run, &dm));
                    let wt = s(&dm["worktree"], "path");
                    if !wt.is_empty() {
                        committed_worktrees.insert(wt.to_string());
                    }
                }
            }
            for (r, m, _) in &applied_records {
                let rid = events::run_id_for(r, m);
                let wt = s(&m["worktree"], "path");
                if !wt.is_empty() && committed_worktrees.contains(wt) {
                    committed_runs.insert(rid.clone());
                }
                if post_verify_failed {
                    verify_failed_runs.insert(rid);
                }
            }
            let work_opts = work::ApplyWorkOptions {
                closes_work_id: closes_work.clone(),
                keep_open,
                forced: force_apply,
                committed_runs,
                verify_failed_runs,
            };
            let work_hints = work::on_runs_applied_with_options(&applied_records, &work_opts);
            work::check_stale_and_expired();
            dag_queue::advance();
            if !output::json_enabled() {
                for hint in work_hints {
                    println!("{hint}");
                }
            }
        }
    }
    // Invariant: whenever any run reported `apply.ok == false`, a `failedStep`, unresolved
    // conflict markers, or a failed verify step, `apply` MUST NOT exit 0 (3 for conflicts /
    // unresolved conflict markers, 1 for other failures; signal codes >= 128 are preserved).
    {
        let mut has_conflict = false;
        let mut has_failure = !verify.is_null() && verify["ok"] != json!(true);
        for item in &done {
            let ap = &item.conclusion["apply"];
            let step = s(ap, "failedStep");
            let has_markers = ap["conflictMarkers"]
                .as_array()
                .is_some_and(|a| !a.is_empty());
            if has_markers || matches!(step, "conflicts" | "skipped-conflicts" | "conflict-markers")
            {
                has_conflict = true;
            }
            if ap["ok"] == json!(false) || !step.is_empty() {
                has_failure = true;
            }
        }
        if has_conflict && code < 128 {
            code = worktree::EXIT_CONFLICT;
        } else if has_failure && code == 0 {
            code = 1;
        }
    }
    if code == 0 && !dry && !output::json_enabled() {
        for item in &done {
            if item.already_applied {
                continue;
            }
            let mut touched = std::collections::BTreeSet::<&str>::new();
            for p in &item.written_files {
                touched.insert(p.as_str());
            }
            for p in &item.regenerated_files {
                touched.insert(p.as_str());
            }
            let count = if touched.is_empty() {
                worktree::run_files(&item.run).len()
            } else {
                touched.len()
            };
            if count > 0 {
                let run_id = {
                    let r = s(&item.conclusion, "run");
                    if !r.is_empty() {
                        r.to_string()
                    } else {
                        item.run
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    }
                };
                println!("本 run 改动文件（{count} 个）：delegate files {run_id}");
            }
        }
    }
    if code == 0 && !dry && !has(&flags, "--keep") {
        let done_set: std::collections::HashSet<PathBuf> =
            done.iter().map(|item| item.run.clone()).collect();
        let mut staged_gc = false;

        let wt_is_shared_active = |run: &Path, wt_path: &str| -> bool {
            !wt_path.is_empty()
                && runs::all_runs().iter().any(|other| {
                    other != run
                        && s(&json(other.join("meta.json"))["worktree"], "path") == wt_path
                        && (runs::active(&runs::state(other)) || !other.join(".applied").exists())
                })
        };

        // 1. Chain-tail applied runs (`done`):
        //    Fail-closed full auto-clean when committed (`--commit`/`--keep-commits`) or
        //    `verify.ok == true` or `acceptStillValid == true` (with no conflicts, no pending
        //    reply, no `.generate-pending`, and no unread truncated result).
        //    Otherwise keep lightweight `.applied-cleaned` and stage worktree removal to trash.
        for item in &done {
            if item.already_applied || !worktree::clean_on_apply(&item.run) {
                continue;
            }
            let sum = json(item.run.join("summary.json"));
            let no_conflicts = item.conclusion["apply"]["conflictMarkers"]
                .as_array()
                .is_none_or(|a| a.is_empty())
                && sum["conflictMarkers"]
                    .as_array()
                    .is_none_or(|a| a.is_empty());
            let no_gen_pending = !item.run.join(".generate-pending").exists();
            let no_pending_reply = pending_reply::status(&item.run).is_none();
            let not_truncated = !item.run.join(".truncated").exists();
            let not_active =
                !runs::active(&runs::state(&item.run)) && !runs::agent_alive(&item.run);
            let committed = b(&item.conclusion["apply"], "committed")
                || b(&item.conclusion["apply"], "keepCommits");
            let verify_ok = verify["ok"] == json!(true);
            let accept_valid = item.conclusion["acceptStillValid"] == json!(true);
            let should_full_clean = no_conflicts
                && no_gen_pending
                && no_pending_reply
                && not_truncated
                && not_active
                && (committed || verify_ok || accept_valid);

            if should_full_clean {
                let meta = json(item.run.join("meta.json"));
                let wt_path = s(&meta["worktree"], "path").to_string();
                let shared = wt_is_shared_active(&item.run, &wt_path);
                match runs::auto_clean_applied_run(&item.run, !shared) {
                    Ok(Some(arch)) => {
                        staged_gc = true;
                        eprintln!("delegate: archive: {}", arch.display());
                    }
                    Ok(None) => staged_gc = true,
                    Err(err) => {
                        eprintln!("delegate: apply cleanup: kept run: {err}");
                    }
                }
            } else {
                let (diagnostics, staged) = runs::remove_after_apply(&item.run);
                if staged {
                    staged_gc = true;
                }
                for diagnostic in diagnostics {
                    eprintln!("delegate: apply cleanup: {diagnostic}");
                }
                index::upsert_run(&item.run);
                board::refresh_for_run(&item.run);
            }
        }

        // 2. Superseded predecessors in the applied chain (`applied_runs` not in `done`):
        //    Clean immediately (fail-closed) so applied reply chains never leave dead rounds behind.
        for pred in &applied_runs {
            if done_set.contains(pred) || !pred.is_dir() {
                continue;
            }
            if !worktree::clean_on_apply(pred)
                || pred.join(".generate-pending").exists()
                || pending_reply::status(pred).is_some()
                || runs::active(&runs::state(pred))
                || runs::agent_alive(pred)
            {
                continue;
            }
            let sum = json(pred.join("summary.json"));
            if sum["conflictMarkers"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
            {
                continue;
            }
            let meta = json(pred.join("meta.json"));
            let wt_path = s(&meta["worktree"], "path").to_string();
            let shared = wt_is_shared_active(pred, &wt_path);
            match runs::auto_clean_applied_run(pred, !shared) {
                Ok(_) => staged_gc = true,
                Err(err) => {
                    eprintln!("delegate: apply predecessor cleanup: kept run: {err}");
                }
            }
        }

        if staged_gc {
            gc::spawn_worker();
        }
    }
    // Always release pool slots for runs that wrote `.applied` and have no unapplied/active sharer
    // left in the chain (even when `verify` failed, `--keep`, or `cleanOnApply: false`).
    if !dry {
        for run in &applied_runs {
            pool::release_if_applied_chain(run);
        }
        for item in &done {
            pool::release_if_applied_chain(&item.run);
        }
    }
    Ok(code)
}
fn main_inner(args: &[String]) -> Res<i32> {
    let Some((command, rest)) = args.split_first() else {
        return Err("the following arguments are required: command".into());
    };
    if command == "--version" {
        println!("delegate {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    if command == "--help" || command == "-h" {
        help::print(None);
        return Ok(0);
    }
    let help_requested = if command == "lane" {
        let mut i = 0;
        while rest.get(i).is_some_and(|arg| arg == "--label") {
            i += 2;
        }
        rest.get(i)
            .is_some_and(|arg| arg == "--help" || arg == "-h")
    } else {
        rest.iter()
            .take_while(|arg| *arg != "--")
            .any(|arg| arg == "--help" || arg == "-h")
    };
    if help_requested && help::print(Some(command)) {
        return Ok(0);
    }
    if command == "_reply-pending" {
        pending_reply::deliver(Path::new(rest.first().ok_or("missing run")?))?;
        return Ok(0);
    }
    if command == "_source-check" { return source_guard::command(rest); }
    if command == "_supervise" {
        let run = rest.first().ok_or("missing run")?;
        supervise::supervise(Path::new(run))?;
        return Ok(0);
    }
    if command == "_remote" { return remote::command(rest); }
    if command == "remote" { return remote::check(rest); }
    if command == "_resource" {
        return resources::command(rest);
    }
    if command == "_measure" {
        let run = rest.first().ok_or("missing measurement run")?;
        let real = rest.get(1).ok_or("missing measured executable")?;
        return measure::execute_argv(Path::new(run), real, &rest[2..]);
    }
    if command == "_process-match" {
        return process_match::command(rest);
    }
    if command == "_gc-worker" {
        return gc::worker_command(rest);
    }
    if command == "_merge-worker" {
        return premerge::worker_command(rest);
    }
    if command == "snapshot" {
        return index::snapshot_command(rest);
    }
    if command == "tui" {
        return tui::command(rest);
    }
    match command.as_str() {
        "start" | "run" => {
            let o = launch::parse_launch(rest, false, command == "run")?;
            let max = o.max;
            let prog = o.progress;
            let full = o.full;
            let run = launch::start(o)?;
            let created_meta = json(run.join("meta.json"));
            let created_prompt = history::extract_user_prompt(&run, &created_meta);
            history::warn_similar_runs(&run, s(&created_meta, "name"), &created_prompt);
            hint_undelivered_other_runs(&run);
            hint_concurrent_writes(&run);
            if command == "start" {
                println!("{}", status_line(&run));
                queue::notice(&run);
                eprintln!(
                    "delegate: started {}; collect with: {} wait {}",
                    run.file_name().unwrap_or_default().to_string_lossy(),
                    script().display(),
                    run.file_name().unwrap_or_default().to_string_lossy()
                );
                Ok(0)
            } else {
                eprintln!(
                    "delegate: started {}",
                    run.file_name().unwrap_or_default().to_string_lossy()
                );
                let max = check_in(max, std::slice::from_ref(&run));
                Ok(collect(
                    std::slice::from_ref(&run),
                    max,
                    prog,
                    full,
                    true,
                    &format!(
                        "{} wait {}",
                        script().display(),
                        run.file_name().unwrap_or_default().to_string_lossy()
                    ),
                ))
            }
        }
        "reply" => {
            let mut o = launch::parse_launch(rest, true, true)?;
            if let Some(enqueued) = pending_reply::enqueue(rest, &mut o)? {
                if output::json_enabled() {
                    let mut st = runs::status(&enqueued.run);
                    st["appendDelivery"] = json!(enqueued.mode_str());
                    st["appendSeq"] = json!(enqueued.seq);
                    println!("{}", format_status(st));
                } else {
                    println!("{}", enqueued.human_line());
                }
                return Ok(0);
            }
            let max = o.max;
            let prog = o.progress;
            let full = o.full;
            let wait = o.wait;
            let parent = o.run.clone().unwrap_or_default();
            let run = launch::reply(o)?;
            hint_undelivered_other_runs(&run);
            let name = run
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if wait {
                eprintln!("delegate: started {name} (reply to {parent})");
                let max = check_in(max, std::slice::from_ref(&run));
                Ok(collect(
                    std::slice::from_ref(&run),
                    max,
                    prog,
                    full,
                    true,
                    &format!("{} wait {}", script().display(), name),
                ))
            } else {
                println!("{}", status_line(&run));
                queue::notice(&run);
                eprintln!(
                    "delegate: started {name} (reply to {parent}); collect with: {} wait {name}",
                    script().display()
                );
                Ok(0)
            }
        }
        "wait" => {
            let (pos, flags, kv) = parse_simple(
                rest,
                &[
                    "--all",
                    "--machine",
                    "--current-repo",
                    "--no-result",
                    "--full",
                    "--progress",
                    "--any",
                    "--until-all",
                    "--stream",
                    "--json",
                ],
                &["--max", "--since"],
            )?;
            hook::record_action("wait", &pos, None, has(&flags, "--machine"));
            if has(&flags, "--json") {
                output::enable_json();
            }
            let mut max = value(&kv, "--max").map(seconds).transpose()?;
            if let Some(raw_since) = value(&kv, "--since") {
                let since = raw_since
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --since sequence number: {raw_since}"))?;
                return events::wait_since(since, &pos, max);
            }
            if has(&flags, "--machine") && !pos.is_empty() {
                return Err("--machine does not take run arguments".into());
            }
            let (explicit_any, stream, explicit_until_all) = (
                has(&flags, "--any"),
                has(&flags, "--stream"),
                has(&flags, "--until-all"),
            );
            if explicit_any && stream {
                return Err("--any and --stream cannot be combined".into());
            }
            if explicit_until_all && (explicit_any || stream) {
                return Err("--until-all cannot be combined with --any or --stream".into());
            }
            let until_all = explicit_until_all || (has(&flags, "--all") && !explicit_any && !stream);
            // Named waits have the same first-completion semantics as the default wait;
            // use --until-all when the caller needs every selected run.
            let any = !until_all && (explicit_any || !stream);
            let (mut list, hinted) = wait_list(&pos, &flags, false)?;
            if !until_all && !output::json_enabled() {
                let delivered = list
                    .iter()
                    .filter(|r| r.join(".delivered").exists())
                    .map(|r| {
                        r.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect::<Vec<_>>();
                if !delivered.is_empty() {
                    println!("以下 run 已交付，未等待：{}", delivered.join(", "));
                }
            }

            if any && (pos.is_empty() || explicit_any) {
                // A named run that was already reported in full is not waited for again, so
                // repeating the same `wait --any` walks through the rest.
                list.retain(|r| runs::active(&runs::state(r)) || !r.join(".delivered").exists());
            }
            if list.is_empty() {
                if !hinted {
                    eprintln!("delegate: no active or undelivered runs");
                }
                return Ok(0);
            }
            max = check_in(max, &list);
            if until_all {
                max = Some(max.unwrap_or(3000.0).min(3000.0));
            }
            if any || stream {
                return Ok(wait_each(
                    list,
                    pos.is_empty().then_some((pos.as_slice(), flags.as_slice())),
                    max,
                    stream,
                    has(&flags, "--progress"),
                    has(&flags, "--full"),
                    !has(&flags, "--no-result"),
                    &std::iter::once(script().to_string_lossy().into_owned())
                        .chain(std::iter::once("wait".into()))
                        .chain(rest.iter().cloned())
                        .map(|arg| shell_quote(&arg))
                        .collect::<Vec<_>>()
                        .join(" "),
                ));
            }
            Ok(collect(
                &list,
                max,
                has(&flags, "--progress"),
                has(&flags, "--full"),
                !has(&flags, "--no-result"),
                &std::iter::once(script().to_string_lossy().into_owned())
                    .chain(std::iter::once("wait".into()))
                    .chain(rest.iter().cloned())
                    .map(|arg| shell_quote(&arg))
                    .collect::<Vec<_>>()
                    .join(" "),
            ))
        }
        "config" => {
            if !rest.is_empty() { return Err("usage: delegate config".into()); }
            let cwd = env::current_dir().map_err(|e| e.to_string())?;
            let cfg = config::load(git_top(&cwd).as_deref())?;
            println!("{}", serde_json::to_string_pretty(&cfg.value).map_err(|e| e.to_string())?);
            Ok(0)
        }
        "status" | "list" => {
            let (pos, flags, _) = parse_simple(rest, &["--json", "--all", "--files"], &[])?;
            if has(&flags, "--json") {
                output::enable_json();
            }
            let include_files = has(&flags, "--files");
            let detail = !pos.is_empty();
            let show_all = has(&flags, "--all");
            if detail {
                let mut head_cache: std::collections::HashMap<PathBuf, Option<String>> =
                    std::collections::HashMap::new();
                let mut stale_repos = std::collections::BTreeSet::<PathBuf>::new();
                let mut eta_env: Option<(speed::EtaModel, eta::LiveContext)> = None;
                for x in &pos {
                    match runs::resolve_head(x) {
                        Ok(run) => {
                            let mut status = runs::status_detail(&run);
                            premerge::overlay_live_status(&mut status, &run, &mut head_cache);
                            premerge::collect_stale_repo(&run, &status, &mut stale_repos);
                            let (eta_model, eta_ctx) = eta_env.get_or_insert_with(|| {
                                let st = state_dir();
                                let now_ts = epoch() as i64;
                                let mut vis = runs::all_runs();
                                if !vis.contains(&run) {
                                    vis.push(run.clone());
                                }
                                (
                                    speed::load_or_refresh_eta_model_in(&st, now_ts, true),
                                    eta::LiveContext::capture(&st, now_ts, &vis),
                                )
                            });
                            eta::overlay_status_eta(&mut status, &run, eta_model, eta_ctx);
                            if output::json_enabled() {
                                if !include_files {
                                    if let Some(obj) = status.as_object_mut() {
                                        obj.remove("files");
                                    }
                                }
                                println!("{}", format_status(status));
                            } else {
                                println!("{}", output::human_detail(&run, &status));
                            }
                        }
                        Err(err) => {
                            if let Ok(chain) = notes::resolve_cleaned(x) {
                                if output::json_enabled() {
                                    println!("{}", format_status(output::cleaned_note_json(&chain)));
                                } else {
                                    println!("{}", output::human_cleaned_note(&chain, true));
                                }
                            } else {
                                return Err(err);
                            }
                        }
                    }
                }
                premerge::wake_repos(&stale_repos);
                return Ok(0);
            }

            let state_root = state_dir();
            let idx = index::load_index(&state_root);
            let wsig = index::work_file_sig(&state_root);
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let cur_repo_canon = {
                let raw = git_top_fast(&cwd).unwrap_or_else(|| cwd.clone());
                fs::canonicalize(&raw).unwrap_or(raw)
            };
            let all_with_repos = runs::all_machine_runs_sorted_with_index(&state_root, &idx);
            let mut repo_tag_by_run: std::collections::HashMap<PathBuf, String> =
                std::collections::HashMap::new();
            let all: Vec<PathBuf> = all_with_repos
                .into_iter()
                .map(|(repo_root, run_dir)| {
                    let repo_canon = fs::canonicalize(&repo_root).unwrap_or(repo_root);
                    if repo_canon != cur_repo_canon {
                        if let Some(name) = repo_canon.file_name().and_then(|n| n.to_str()) {
                            if !name.is_empty() {
                                repo_tag_by_run.insert(run_dir.clone(), name.to_string());
                            }
                        }
                    }
                    run_dir
                })
                .collect();
            let cleaned_notes = notes::cleaned_pending_chains(&all);
            let mut head_cache: std::collections::HashMap<PathBuf, Option<String>> =
                std::collections::HashMap::new();
            let mut stale_repos = std::collections::BTreeSet::<PathBuf>::new();
            if show_all {
                let eta_env = if !all.is_empty() {
                    let now_ts = epoch() as i64;
                    Some((
                        speed::load_or_refresh_eta_model_in(&state_root, now_ts, true),
                        eta::LiveContext::capture(&state_root, now_ts, &all),
                    ))
                } else {
                    None
                };
                let mut rows: Vec<(&PathBuf, Value)> = Vec::with_capacity(all.len());
                let mut superseded_parents = std::collections::HashSet::<String>::new();
                for run in &all {
                    let mut status = index::status_cached(run, &idx, &wsig);
                    premerge::overlay_live_status(&mut status, run, &mut head_cache);
                    premerge::collect_stale_repo(run, &status, &mut stale_repos);
                    if let Some((ref eta_model, ref eta_ctx)) = eta_env {
                        eta::overlay_status_eta(&mut status, run, eta_model, eta_ctx);
                    }
                    let parent_id = s(&status, "parent");
                    if !parent_id.is_empty() {
                        superseded_parents.insert(parent_id.to_string());
                    }
                    rows.push((run, status));
                }
                for (run, mut status) in rows {
                    if output::json_enabled() {
                        if !include_files {
                            if let Some(obj) = status.as_object_mut() {
                                obj.remove("files");
                            }
                        }
                        println!("{}", format_status(status));
                    } else {
                        let rid = s(&status, "run");
                        if !rid.is_empty() && superseded_parents.contains(rid) {
                            continue;
                        }
                        let repo_tag = repo_tag_by_run.get(run).map(String::as_str);
                        println!(
                            "{}",
                            output::human_line_with_repo(run, &status, true, repo_tag)
                        );
                    }
                }
                for chain in &cleaned_notes {
                    if output::json_enabled() {
                        println!("{}", format_status(output::cleaned_note_json(chain)));
                    } else {
                        println!("{}", output::human_cleaned_note(chain, false));
                    }
                }
                if !output::json_enabled() {
                    print_status_lanes()?;
                }
                premerge::wake_repos(&stale_repos);
                return Ok(0);
            }

            let hist_entries = history::load_entries(&history::current_history_key());
            let entries: Vec<(PathBuf, Value, Value)> = all
                .iter()
                .map(|run| {
                    let (mut st, m) = index::status_and_meta_cached(run, &idx, &wsig);
                    premerge::overlay_live_status(&mut st, run, &mut head_cache);
                    (run.clone(), st, m)
                })
                .collect();
            let replied_ids = runs::superseded_by_reply_cached(&entries, &hist_entries);
            let notes_exist = work::any_notes_exist();

            let now_ts = epoch() as i64;
            let open_work_json = work::load_open_work_json_readonly(&state_root);
            let all_statuses: Vec<&Value> = entries.iter().map(|(_, st, _)| st).collect();
            let pending_split =
                pulse::split_pending_runs_refs(&all_statuses, &open_work_json, now_ts);
            let active_pending_set: std::collections::HashSet<&str> = pending_split
                .active_pending
                .iter()
                .map(String::as_str)
                .collect();
            let stale_pending_set: std::collections::HashSet<&str> = pending_split
                .stale_pending
                .iter()
                .map(String::as_str)
                .collect();

            let mut running = 0usize;
            let mut pending_apply = 0usize;
            let mut pending_question = 0usize;
            let mut queued = 0usize;
            let mut unread = 0usize;
            let mut rejected = 0usize;
            let mut other_actionable = 0usize;
            let mut pending_todo = 0usize;
            let mut shown: Vec<(PathBuf, Value)> = Vec::new();
            let mut shown_ids = std::collections::HashSet::<String>::new();
            let mut hidden_ids = std::collections::HashSet::<String>::new();

            for (i, (run, status, idx_meta)) in entries.iter().enumerate() {
                let st = s(status, "state");
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
                let is_replied = replied_ids.contains(&id);
                let is_applied =
                    b(status, "applied") || !s(idx_meta, "appliedAt").is_empty();
                let unresolved_conflicts = status["conflictMarkers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .any(|path| {
                        let src = s(idx_meta, "worktreeSource");
                        !src.is_empty()
                            && !worktree::conflict_labels(&Path::new(src).join(path)).is_empty()
                    });
                let note_pending = if notes_exist {
                    notes::for_run(run, &all).map_or(0, |c| c.pending_count())
                } else {
                    0
                };

                let mut keep = if pulse::is_run_active_running_state(status) {
                    running += 1;
                    true
                } else if pulse::is_run_queued_state(status) {
                    queued += 1;
                    true
                } else if stale_pending_set.contains(id.as_str()) {
                    // >24h unattached pending runs are tracked in `stalePending` (same as `SnapshotPulse`)
                    false
                } else if active_pending_set.contains(id.as_str()) {
                    if b(status, "question") {
                        pending_question += 1;
                    } else if st == "rejected" {
                        rejected += 1;
                    } else if matches!(st, "delivered" | "answered") {
                        let has_worktree_changes =
                            runs::chain_pending_apply_files_cached(i, &entries) > 0;
                        let needs_ctrl = s(&status["review"], "verdict") == "NEEDS_CONTROLLER"
                            && !is_applied
                            && !is_replied
                            && !(status["sign"].is_object() && b(&status["sign"], "valid"));
                        if unresolved_conflicts
                            || (has_worktree_changes && !is_applied && !is_replied)
                        {
                            pending_apply += 1;
                        } else if needs_ctrl {
                            other_actionable += 1;
                        } else {
                            unread += 1;
                        }
                    } else {
                        other_actionable += 1;
                    }
                    true
                } else {
                    false
                };

                if !keep && !is_replied && note_pending > 0 {
                    keep = true;
                }

                if keep {
                    if !is_replied {
                        pending_todo += note_pending;
                    }
                    shown_ids.insert(id);
                    shown.push((run.clone(), status.clone()));
                } else {
                    hidden_ids.insert(id);
                }
            }
            pending_todo += cleaned_notes
                .iter()
                .map(|c| c.pending_count())
                .sum::<usize>();

            let mut finished_ids = hidden_ids;
            for e in &hist_entries {
                if !shown_ids.contains(&e.run) {
                    finished_ids.insert(e.run.clone());
                }
            }
            let finished_count = finished_ids.len();

            let arch_usage = archive::usage();
            let disk_health = disk::inspect_readonly(&state_root, &cache_dir(), now_ts);
            let concurrency_plan = lane::local_plan(None).ok();
            let concurrency_pool = concurrency_plan
                .as_ref()
                .map(|plan| jobserver::inspect_pool_readonly(plan.pool_tokens, plan.budget));

            if !shown.is_empty() {
                let shown_paths: Vec<PathBuf> = shown.iter().map(|(r, _)| r.clone()).collect();
                let eta_model = speed::load_or_refresh_eta_model_in(&state_root, now_ts, true);
                let eta_ctx = eta::LiveContext::capture(&state_root, now_ts, &shown_paths);
                for (run, status) in &mut shown {
                    eta::overlay_status_eta(status, run, &eta_model, &eta_ctx);
                }
            }

            if output::json_enabled() {
                for (_run, status) in &shown {
                    let mut status = status.clone();
                    if !include_files {
                        if let Some(obj) = status.as_object_mut() {
                            obj.remove("files");
                        }
                    }
                    println!("{}", format_status(status));
                }
                for chain in &cleaned_notes {
                    println!("{}", format_status(output::cleaned_note_json(chain)));
                }
                let mut summary = json!({
                    "running": running,
                    "pendingApply": pending_apply,
                    "pendingQuestion": pending_question,
                    "queued": queued,
                    "unread": unread,
                    "rejected": rejected,
                    "pendingNotes": pending_todo,
                    "otherActionable": other_actionable,
                    "controllerPending": pending_split.active_pending.len(),
                    "stalePending": pending_split.stale_pending.len(),
                    "finished": finished_count,
                    "archiveBytes": arch_usage.bytes,
                    "archiveCount": arch_usage.count,
                    "archiveFsCritical": arch_usage.fs_critical,
                    "archiveSizeWarning": arch_usage.size_warning,
                    "archiveWarnBytes": arch_usage.warn_bytes,
                    "diskHealth": disk_health.to_json(),
                    "history": "delegate history"
                });
                if let Some(ref plan) = concurrency_plan {
                    summary["concurrency"] = match concurrency_pool.as_ref() {
                        Some(pool) => plan.to_json_with_pool(pool),
                        None => plan.to_json(),
                    };
                }
                println!("{}", json!({"statusSummary": summary}));
            } else {
                if let Some(warn_line) = pulse::format_status_quota_alert_line_in(&state_root, now_ts) {
                    println!("{warn_line}");
                }
                let mut counts = format!(
                    "运行中 {running} · 待合入 {pending_apply} · 待回答 {pending_question} · 排队 {queued}"
                );
                if unread > 0 {
                    counts.push_str(&format!(" · 待读 {unread}"));
                }
                if rejected > 0 {
                    counts.push_str(&format!(" · 未通过 {rejected}"));
                }
                if pending_todo > 0 {
                    counts.push_str(&format!(" · 待办 {pending_todo}"));
                }
                if other_actionable > 0 {
                    counts.push_str(&format!(" · 异常 {other_actionable}"));
                }
                println!("{counts}");
                for (run, status) in &shown {
                    let repo_tag = repo_tag_by_run.get(run).map(String::as_str);
                    println!(
                        "{}",
                        output::human_line_with_repo(run, status, true, repo_tag)
                    );
                }
                for chain in &cleaned_notes {
                    println!("{}", output::human_cleaned_note(chain, false));
                }
                let mut footer_parts = Vec::new();
                if finished_count > 0 {
                    footer_parts.push(format!("另有 {finished_count} 条已结束：delegate history"));
                }
                if arch_usage.count > 0 {
                    let mut arch_text = format!(
                        "归档占用 {}（{} 份）",
                        format_bytes(arch_usage.bytes),
                        arch_usage.count
                    );
                    if arch_usage.size_warning {
                        arch_text.push_str(&format!(
                            "（归档总量异常偏大，超过阈值 {}，位于 {}）",
                            format_bytes(arch_usage.warn_bytes),
                            cache_dir().join("archive").display()
                        ));
                    }
                    if arch_usage.fs_critical {
                        if let Some(avail) = arch_usage.fs_avail_bytes {
                            arch_text.push_str(&format!(
                                "（文件系统剩余仅 {}，将按最久未读清理可重建字节）",
                                format_bytes(avail)
                            ));
                        }
                    }
                    footer_parts.push(arch_text);
                }
                if let Some(ref plan) = concurrency_plan {
                    if plan.heavy_jobs_explicit {
                        let summary_str = match concurrency_pool.as_ref() {
                            Some(pool) => plan.format_summary_with_pool(pool),
                            None => plan.format_summary(),
                        };
                        footer_parts.push(format!("编译并发：{summary_str}"));
                    }
                }
                if !footer_parts.is_empty() {
                    println!("{}", footer_parts.join(" · "));
                }
                if disk_health.should_show_in_status() {
                    if let Some(line) = disk_health.format_compact_line() {
                        println!("{line}");
                    }
                    for line in disk_health.format_stale_lines() {
                        println!("{line}");
                    }
                }
                print_status_lanes()?;
            }
            for (run, st, _) in &entries {
                premerge::collect_stale_repo(run, st, &mut stale_repos);
            }
            premerge::wake_repos(&stale_repos);
            Ok(0)
        }
        "history" => history::history_command(rest),
        "show" => history::show_command(rest),
        "board" => board::command(rest),
        "result" => result_command(rest),
        "work" => work::command(rest),
        "fill" => fill::command(rest),
        "agy" => agy_accounts::command(rest),
        "usage" => usage::command(rest),
        "speed" => speed::command(rest),
        "eta" => eta::command(rest),
        "hook" => hook::command(rest),
        "events" => events::command(rest),
        "queue" => dag_queue::command(rest),
        "sign" => review::sign_command(rest),
        "card" => card::card_command(rest),
        "merge" => merge::command(rest),
        "radar" => radar::command(rest),
        "diff" => diff(rest),
        "files" => files_command(rest),
        "patch" => patch(rest),
        "apply" => apply(rest),
        "mark-applied" => mark_applied_command(rest),
        "adopt" => adopt_command(rest),
        "lane" => lane::lane_command(rest),
        "busy" => {
            if rest.len() != 1 {
                return Err("busy requires one resource name".into());
            }
            Ok(resources::busy(&rest[0]))
        }
        "stop" => stop(rest),
        "clean" => clean(rest),
        "disk" => disk::command(rest),
        "tiers" => tiers(rest),
        "fit" => fit::command(rest),
        "handoff" => handoff::command(rest),
        "note" => notes::command(rest),
        "probe" => probe::command(rest),
        "protocol" => {
            if let Some(arg) = rest.first() {
                return Err(format!("unrecognized arguments: {arg}"));
            }
            println!("{}", crate::agents::protocol());
            Ok(0)
        }
        _ => Err(format!("invalid choice: {command}")),
    }
}
/// Show the model tiers, or write them to the user config.
fn tiers(args: &[String]) -> Res<i32> {
    let path = config::user_path();
    let mut user = config::read_file(&path)?.unwrap_or_else(|| json!({}));
    let mut changed = false;
    let mut i = 0;
    while i < args.len() {
        let (key, inline) = args[i]
            .split_once('=')
            .map_or((args[i].as_str(), None), |(k, v)| (k, Some(v.to_string())));
        let name = key
            .strip_prefix("--")
            .filter(|n| models::TIERS.contains(n) || ["alias", "thinking"].contains(n))
            .ok_or_else(|| format!("unrecognized arguments: {}", args[i]))?;
        let value = match inline {
            Some(v) => v,
            None => {
                i += 1;
                args.get(i)
                    .cloned()
                    .ok_or_else(|| format!("{key}: expected one argument"))?
            }
        };
        match name {
            "alias" => {
                let (alias, id) = value
                    .split_once('=')
                    .ok_or("--alias expects name=provider/id")?;
                if !user["models"].is_object() {
                    user["models"] = json!({});
                }
                user["models"][alias] = json!(id);
            }
            "thinking" => {
                models::validate_thinking(&value)?;
                user["thinking"] = json!(value);
            }
            tier => {
                models::validate_model(&value)?;
                if !user["tiers"].is_object() {
                    user["tiers"] = json!({});
                }
                user["tiers"][tier] = json!(value);
            }
        }
        changed = true;
        i += 1;
    }
    let listed = agents::pi_models();
    if changed {
        config::validate(&user, &path, false)?;
        // Validate before the atomic write, including every alias and inactive rule.
        if let Some(aliases) = user["models"].as_object() {
            for id in aliases.values().filter_map(models::alias_model) {
                models::resolve_model(id, &user, &listed)?;
            }
        }
        for tier in models::TIERS {
            match &user["tiers"][tier] {
                Value::String(_) => {
                    models::candidate(&user["tiers"][tier], &user, None, &listed)?;
                }
                Value::Array(rules) => {
                    for rule in rules {
                        models::candidate(rule, &user, None, &listed)?;
                    }
                }
                _ => {}
            }
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        atomic_write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&user).unwrap()),
        )?;
    }
    let top = git_top(&env::current_dir().unwrap_or_default());
    let config = config::load(top.as_deref())?;
    let mut missing = false;
    for tier in models::TIERS {
        let rules = models::tier_rules(&config.value, tier);
        if rules.is_empty() {
            println!("{tier}: (not set)");
            missing = true;
            continue;
        }
        let mut descriptions = Vec::new();
        for rule in rules {
            let pick = models::candidate(&rule, &config.value, None, &listed)?;
            let conditions = agents::describe_tier(&json!([rule.clone()])).unwrap_or_default();
            descriptions.push(format!(
                "{} → {} (thinking {}; {})",
                pick.name,
                pick.model,
                pick.thinking.as_deref().unwrap_or("Pi default"),
                conditions
            ));
        }
        println!("{tier}: {}", descriptions.join(", "));
    }
    println!("aliases: name → model | context | images | thinking");
    if let Some(aliases) = config.value["models"].as_object() {
        for (name, id) in aliases {
            let id = crate::models::alias_model(id).unwrap_or("");
            if let Some(model) = listed.iter().find(|m| m.name == id) {
                println!(
                    "  {name} → {id} | {} | {} | {}",
                    model.context, model.images, model.thinking
                );
            } else {
                println!("  {name} → {id} | unknown | unknown | unknown");
            }
        }
    }
    if missing {
        println!(
            "Pi 里还有 {} 个模型未登记",
            models::unregistered_count(&config.value, &listed)
        );
        eprintln!("{}", agents::tiers_guide(&config.value));
        return Ok(1);
    }
    println!("config: {}", path.display());
    println!(
        "Pi 里还有 {} 个模型未登记",
        models::unregistered_count(&config.value, &listed)
    );
    Ok(0)
}

fn result_command(rest: &[String]) -> Res<i32> {
    let (pos, flags, kv) = parse_simple(
        rest,
        &["--path", "--no-mark", "--all", "--rounds", "--json"],
        &["--round"],
    )?;
    if pos.len() > 1 {
        return Err("result takes one run".into());
    }
    let want_path = has(&flags, "--path");
    let no_mark = has(&flags, "--no-mark");
    let show_all = has(&flags, "--all");
    let list_rounds = has(&flags, "--rounds");
    let want_json = has(&flags, "--json") || output::json_enabled();
    let want_round_raw = value(&kv, "--round");

    let mode_count = usize::from(want_round_raw.is_some())
        + usize::from(show_all)
        + usize::from(list_rounds);
    if mode_count > 1 {
        return Err("result: --round, --all, and --rounds are mutually exclusive".into());
    }
    if want_path && want_json {
        return Err("result: --path and --json cannot be used together".into());
    }
    let want_round = match want_round_raw {
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if n >= 1 => Some(n),
            _ => {
                return Err(format!(
                    "result: invalid --round value {raw:?} (expected a positive 1-based integer)"
                ))
            }
        },
        None => None,
    };

    let run = runs::resolve_head(pos.first().map(String::as_str).unwrap_or("last"))?;
    let run_meta = json(run.join("meta.json"));
    let run_id = notes::run_id(&run, &run_meta);
    let rounds = runs::collect_result_rounds(&run, true);
    let result = run.join("result.md");

    if want_round.is_some() || show_all || list_rounds {
        if rounds.is_empty() {
            eprintln!(
                "delegate: no result for {} (state: {})",
                run_id,
                runs::state(&run)
            );
            return Ok(1);
        }
    } else if !result.exists() {
        eprintln!(
            "delegate: no result for {} (state: {})",
            run_id,
            runs::state(&run)
        );
        return Ok(1);
    }

    if let Some(k) = want_round {
        if k > rounds.len() {
            return Err(format!(
                "result: round {k} out of range for {run_id} (available: 1..={}; use --rounds to list or --all to print all)",
                rounds.len()
            ));
        }
    }

    if !want_json {
        if let Some(archive) = archive::status(&run) {
            eprintln!("archive: {}", s(&archive, "path"));
        }
        if want_round.is_none() && !show_all && !list_rounds && rounds.len() > 1 {
            eprintln!(
                "rounds: 共 {} 轮（当前最新；单轮 delegate result {run_id} --round <1..={}>，全部 --all，索引 --rounds）",
                rounds.len(),
                rounds.len()
            );
        }
    }

    let rounds_index_json = || -> Vec<Value> {
        rounds.iter().map(runs::ResultRound::to_json).collect()
    };
    let multi_run = rounds
        .iter()
        .map(|r| r.run_id.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        > 1;
    let round_label = |r: &runs::ResultRound| -> String {
        let mut parts = Vec::new();
        if multi_run && !r.run_id.is_empty() {
            parts.push(r.run_id.clone());
        }
        parts.push(r.kind_zh().to_string());
        if r.current {
            format!("{}，当前", parts.join(" · "))
        } else {
            parts.join(" · ")
        }
    };

    let mut should_mark_read = false;
    let mut wrote_full_current = false;

    if list_rounds {
        if want_json {
            let doc = json!({
                "run": run_id,
                "totalRounds": rounds.len(),
                "rounds": rounds_index_json(),
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&doc).unwrap_or_else(|_| doc.to_string())
            );
        } else if want_path {
            for r in &rounds {
                println!("{}", r.path.display());
            }
        } else {
            println!("共 {} 轮（{run_id}）：", rounds.len());
            for r in &rounds {
                println!(
                    "  {}. [{}] {}（共 {} 行 / {} 字）",
                    r.round,
                    round_label(r),
                    r.path.display(),
                    r.total_lines,
                    r.total_chars
                );
            }
            println!(
                "查看单轮：delegate result {run_id} --round <1..={}> · 全部：delegate result {run_id} --all",
                rounds.len()
            );
        }
    } else if show_all {
        if want_json {
            let items: Vec<Value> = rounds
                .iter()
                .map(|r| r.to_json_with_text(&read(&r.path)))
                .collect();
            let doc = json!({
                "run": run_id,
                "totalRounds": rounds.len(),
                "rounds": items,
            });
            let text = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| doc.to_string());
            let mut out = io::stdout().lock();
            if out
                .write_all(format!("{text}\n").as_bytes())
                .and_then(|()| out.flush())
                .is_ok()
            {
                wrote_full_current = true;
            }
        } else if want_path {
            for r in &rounds {
                println!("{}", r.path.display());
            }
        } else {
            let mut buf = String::new();
            let total = rounds.len();
            for (i, r) in rounds.iter().enumerate() {
                if i > 0 {
                    buf.push('\n');
                }
                buf.push_str(&format!(
                    "===== round {}/{total} ({} · {}) =====\n",
                    r.round,
                    round_label(r),
                    r.path.display()
                ));
                let body = read(&r.path);
                buf.push_str(body.trim_end_matches('\n'));
                buf.push('\n');
            }
            let mut out = io::stdout().lock();
            if out
                .write_all(buf.as_bytes())
                .and_then(|()| out.flush())
                .is_ok()
            {
                wrote_full_current = true;
            }
        }
        should_mark_read = !no_mark;
    } else if let Some(k) = want_round {
        let entry = &rounds[k - 1];
        if want_json {
            let body = read(&entry.path);
            let doc = json!({
                "run": run_id,
                "totalRounds": rounds.len(),
                "selectedRound": k,
                "round": entry.to_json_with_text(&body),
                "text": body,
                "rounds": rounds_index_json(),
            });
            let text = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| doc.to_string());
            let mut out = io::stdout().lock();
            if out
                .write_all(format!("{text}\n").as_bytes())
                .and_then(|()| out.flush())
                .is_ok()
                && entry.current
            {
                wrote_full_current = true;
            }
        } else if want_path {
            println!("{}", entry.path.display());
        } else {
            let mut out = io::stdout().lock();
            if out
                .write_all(read(&entry.path).as_bytes())
                .and_then(|()| out.flush())
                .is_ok()
                && entry.current
            {
                wrote_full_current = true;
            }
        }
        should_mark_read = !no_mark && entry.current;
    } else {
        if want_json {
            let body = read(&result);
            let doc = json!({
                "run": run_id,
                "totalRounds": rounds.len().max(1),
                "selectedRound": rounds.len().max(1),
                "path": result.to_string_lossy(),
                "totalLines": body.lines().count(),
                "totalChars": body.chars().count(),
                "text": body,
                "rounds": rounds_index_json(),
            });
            let text = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| doc.to_string());
            let mut out = io::stdout().lock();
            if out
                .write_all(format!("{text}\n").as_bytes())
                .and_then(|()| out.flush())
                .is_ok()
            {
                wrote_full_current = true;
            }
        } else if want_path {
            println!("{}", result.display());
        } else {
            let mut out = io::stdout().lock();
            // 全文确实写出后才解除保留，写入失败时 clean 仍会保留它。
            if out
                .write_all(read(&result).as_bytes())
                .and_then(|()| out.flush())
                .is_ok()
            {
                wrote_full_current = true;
            }
        }
        should_mark_read = !no_mark;
    }

    if should_mark_read {
        let is_active = runs::active(&runs::state(&run));
        if wrote_full_current {
            fs::remove_file(run.join(".truncated")).ok();
            if !is_active {
                mark_result_read(&run);
            }
        } else if want_path && !is_active && !run.join(".truncated").exists() {
            mark_result_read(&run);
        }
        if !is_active {
            touch(run.join(".delivered"));
            board::refresh_for_run(&run);
            events::mark_run_read(&events::run_id_for(&run, &run_meta));
            work::on_run_result_read(&run, &run_meta);
        }
    }
    Ok(0)
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let result = main_inner(&args);
    if result.is_ok()
        && args
            .first()
            .is_some_and(|command| matches!(command.as_str(), "status" | "list" | "wait"))
    {
        let wait_since_mode = args.first().is_some_and(|c| c == "wait")
            && args.iter().any(|a| a == "--since" || a.starts_with("--since="));
        if !wait_since_mode {
            output::disk_warnings();
            if args.first().is_some_and(|command| command == "wait") {
                wait_remaining_hint(&args);
            }
        }
    }
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("delegate: {e}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod check_in_tests {
    use super::*;

    #[test]
    fn default_check_in_enabled_for_claude_and_antigravity_sessions() {
        assert_eq!(default_check_in_with(|_| None), None);
        assert_eq!(
            default_check_in_with(|k| (k == "CLAUDE_CODE_SESSION_ID").then(|| "c-1".into())),
            Some(3000.0)
        );
        assert_eq!(
            default_check_in_with(|k| (k == "ANTIGRAVITY_CONVERSATION_ID").then(|| "a-1".into())),
            Some(3000.0)
        );
        assert_eq!(
            default_check_in_with(|k| (k == "ANTIGRAVITY_CONVERSATION_ID").then(|| "   ".into())),
            None
        );
    }
}
