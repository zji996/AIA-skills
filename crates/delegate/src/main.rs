mod agents;
mod changes;
mod cleanup;
mod common;
mod help;
mod lane;
mod launch;
mod runs;
mod supervise;
mod worktree;
use common::*;
use serde_json::json;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn status_line(run: &Path) -> String {
    format_status(runs::status(run))
}
/// A finished run's status, plus how far the source moved since its worktree snapshot.
fn outcome_line(run: &Path) -> String {
    let mut status = runs::status(run);
    if runs::active(&runs::state(run)) {
        return format_status(status);
    }
    if let Some(drift) = worktree::source_drift(run) {
        let overlap = drift["overlap"].as_array().map_or(0, Vec::len)
            + drift["overlapMore"].as_u64().unwrap_or(0) as usize;
        if overlap > 0 {
            let name = run.file_name().unwrap_or_default().to_string_lossy();
            status["next"] = json!(format!(
                "the source changed {overlap} of its files since the snapshot; review {script} diff {name} --total, then {script} apply {name} (stops on conflicts; --merge writes markers), or {script} reply {name} --sync '<rebase onto the current source>' first",
                script = script().display()
            ));
        }
        status["sourceDrift"] = drift;
    }
    format_status(status)
}
fn format_status(status: serde_json::Value) -> String {
    let keys = [
        "run",
        "name",
        "state",
        "agent",
        "agentBin",
        "tier",
        "mode",
        "parent",
        "after",
        "worktree",
        "ageSeconds",
        "elapsedSeconds",
        "attempts",
        "model",
        "turns",
        "files",
        "changes",
        "shape",
        "accept",
        "readOnlyViolation",
        "protectViolation",
        "workspaceChanged",
        "escalatedFrom",
        "queuedSeconds",
        "graceSeconds",
        "tokens",
        "cleanup",
        "warnings",
        "warning",
        "error",
        "last",
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

type Parsed = (Vec<String>, Vec<String>, Vec<(String, String)>);
fn parse_simple(args: &[String], flags: &[&str], values: &[&str]) -> Res<Parsed> {
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
fn has(flags: &[String], key: &str) -> bool {
    flags.iter().any(|x| x == key)
}
fn value<'a>(kv: &'a [(String, String)], key: &str) -> Option<&'a str> {
    kv.iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}
/// Returns true when the answer was abbreviated.
fn print_answer(run: &Path, full: bool) -> bool {
    let result = read(run.join("result.md"));
    let limit = setting("RESULT_CHARS", "6000")
        .parse::<usize>()
        .unwrap_or(6000);
    println!(
        "\n===== result: {} ({} chars) =====",
        run.file_name().unwrap_or_default().to_string_lossy(),
        result.chars().count()
    );
    let truncated = !full && result.chars().count() > limit;
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
            run.join("result.md").display(),
            tail.trim_end_matches('\n')
        );
    }
    println!(
        "===== end: {} =====",
        run.file_name().unwrap_or_default().to_string_lossy()
    );
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
    let mut code = 0;
    for run in runs {
        let state = runs::state(run);
        println!("{}", outcome_line(run));
        if runs::active(&state) {
            code = 75;
            continue;
        }
        if !["delivered", "answered"].contains(&state.as_str()) && code == 0 {
            code = 1;
        }
        changes::print_changes(run);
        changes::print_shape(run);
        let summary = json(run.join("summary.json"));
        if let Some(cleanup) = summary.get("cleanup") {
            let ports = cleanup["ports"]
                .as_array()
                .map(|v| {
                    v.iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            println!(
                "note: 任务结束时终止了 {} 个后台进程{}；答复中提到的服务/地址已不可用",
                cleanup["terminated"],
                if ports.is_empty() {
                    String::new()
                } else {
                    format!("（端口 {ports}）")
                }
            );
        }
        if let Some(warnings) = summary["warnings"].as_array() {
            for warning in warnings {
                println!("warning: {}", warning.as_str().unwrap_or(""));
            }
        }
        let has_result = run.join("result.md").is_file();
        if has_result && show_result {
            // A truncated answer counts as reported, but clean keeps it until read in full.
            if print_answer(run, full) {
                touch(run.join(".truncated"));
            } else {
                fs::remove_file(run.join(".truncated")).ok();
            }
        }
        if show_result || !has_result {
            touch(run.join(".delivered"));
        }
    }
    if code == 75 {
        eprintln!("delegate: still running; call wait again (exit 75)");
    }
    code
}
/// The runs a `wait` collects; the flag is set when a hint about other sessions' runs was printed.
fn wait_list(pos: &[String], flags: &[String], quiet: bool) -> Res<(Vec<PathBuf>, bool)> {
    if has(flags, "--machine") {
        let mut v = launch::machine_runs(&state_dir());
        v.sort();
        v.dedup();
        return Ok((v, false));
    }
    if !pos.is_empty() {
        return Ok((
            pos.iter()
                .map(|x| runs::resolve_head(x))
                .collect::<Res<Vec<_>>>()?,
            false,
        ));
    }
    let mut v = runs::all_runs()
        .into_iter()
        .filter(|r| runs::active(&runs::state(r)) || !r.join(".delivered").exists())
        .collect::<Vec<_>>();
    let mut hinted = false;
    if !has(flags, "--all") {
        if let Some(caller) = caller() {
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
fn wait_each(
    mut list: Vec<PathBuf>,
    rescan: Option<(&[String], &[String])>,
    max: Option<f64>,
    stream: bool,
    show_progress: bool,
    full: bool,
    show_result: bool,
) -> i32 {
    let begin = Instant::now();
    let poll = setting("POLL", "1").parse::<f64>().unwrap_or(1.0).max(0.01);
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let mut watched = std::collections::HashSet::new();
    let mut reported = std::collections::HashSet::new();
    let mut code = 0;
    loop {
        if show_progress {
            for run in &list {
                progress(run, run.file_name().unwrap_or_default().to_str().unwrap_or(""));
            }
        }
        let done = list
            .iter()
            .filter(|r| !reported.contains(*r) && !runs::active(&runs::state(r)))
            .cloned()
            .collect::<Vec<_>>();
        if !done.is_empty() {
            if stream {
                for run in &done {
                    let state = runs::state(run);
                    let mut line = outcome_line(run);
                    if run.join("result.md").is_file() && !run.join(".delivered").exists() {
                        // The answer is not printed here; name the command that reports it.
                        line.pop();
                        line.push_str(&format!(
                            ",\"report\":{}}}",
                            json!(format!(
                                "{} wait {}",
                                script().display(),
                                run.file_name().unwrap_or_default().to_string_lossy()
                            ))
                        ));
                    }
                    println!("{line}");
                    if !["delivered", "answered"].contains(&state.as_str()) {
                        code = 1;
                    }
                }
            } else {
                code = collect(&done, None, false, full, show_result);
            }
            reported.extend(done);
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
            if stream || reported.is_empty() {
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
                    if !list.contains(&run) {
                        list.push(run);
                    }
                }
            }
        }
    }
    let left = list
        .iter()
        .filter(|r| !reported.contains(*r))
        .map(|r| r.file_name().unwrap_or_default().to_string_lossy().into_owned())
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
fn clean(args: &[String]) -> Res<i32> {
    let (pos, flags, _) = parse_simple(args, &["--finished", "--force"], &[])?;
    if pos.is_empty() && !has(&flags, "--finished") {
        return Err("clean requires runs or --finished".into());
    }
    let mut targets = pos
        .iter()
        .map(|p| runs::resolve(p))
        .collect::<Res<Vec<_>>>()?;
    if has(&flags, "--finished") {
        for run in runs::all_runs() {
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
    for run in targets {
        let st = runs::state(&run);
        let name = run.file_name().unwrap_or_default().to_string_lossy();
        if runs::active(&st) || runs::agent_alive(&run) {
            if runs::active(&st) {
                eprintln!("delegate: skip active run {name}");
            } else {
                eprintln!("delegate: skip active run {name} (its agent outlived the supervisor; stop it first: {} stop {name})",script().display());
            }
            continue;
        }
        let note = if runs::unmerged(&run) {
            "; its worktree was never applied"
        } else {
            ""
        };
        runs::remove(&run);
        println!("removed {name} ({st}{note})");
    }
    Ok(0)
}
fn stop(args: &[String]) -> Res<i32> {
    if args.is_empty() {
        return Err("stop requires at least one run".into());
    }
    for reference in args {
        let run = runs::resolve_head(reference)?;
        let old = runs::state(&run);
        if !runs::active(&old) && runs::agent_alive(&run) {
            let pid = read(run.join("agent.pid"))
                .trim()
                .parse::<i32>()
                .unwrap_or(0);
            cleanup::record(&run, pid);
            kill_group(pid, 1.0);
        }
        if runs::active(&old) {
            let pid = read(run.join("pid")).trim().parse::<i32>().unwrap_or(0);
            if pid > 0 {
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
            for _ in 0..60 {
                if run.join("exit_code").is_file() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
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
                write_json(run.join("summary.json"), &sum)?;
                write(run.join("exit_code"), "1\n")?;
            } else {
                let mut sum = json(run.join("summary.json"));
                if s(&sum, "state") != "stopped" {
                    sum["state"] = json!("stopped");
                    write_json(run.join("summary.json"), &sum)?;
                }
            }
        }
        if !run.join("result.md").exists() {
            touch(run.join(".delivered"));
        }
        println!("{}", status_line(&run));
    }
    Ok(0)
}
fn diff(args: &[String]) -> Res<i32> {
    let (options, separated_paths) = match args.iter().position(|arg| arg == "--") {
        Some(index) => (&args[..index], &args[index + 1..]),
        None => (args, &[][..]),
    };
    let (pos, flags, _) = parse_simple(options, &["--stat", "--total"], &[])?;
    let run = runs::resolve_head(pos.first().map(String::as_str).unwrap_or("last"))?;
    let (meta, top, mut before, after) = changes::chain_changes(&run)?;
    if has(&flags, "--total") {
        before = s(&meta, "chainBase").into();
    }
    let mut cmd = Command::new("git");
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
fn apply(args: &[String]) -> Res<i32> {
    let (pos, flags, _) = parse_simple(args, &["--dry-run", "--merge"], &[])?;
    if pos.len() > 1 {
        return Err("apply takes one run".into());
    }
    let run = runs::latest(runs::resolve(
        pos.first().map(String::as_str).unwrap_or("last"),
    )?);
    if runs::active(&runs::state(&run)) {
        return Err(format!(
            "{} is still running",
            run.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    let code = worktree::apply(&run, has(&flags, "--merge"), has(&flags, "--dry-run"))?;
    if run.join(".applied").exists() {
        let path = s(&json(run.join("meta.json"))["worktree"], "path").to_string();
        for other in runs::all_runs() {
            if s(&json(other.join("meta.json"))["worktree"], "path") == path {
                let _ = write(other.join(".applied"), read(run.join(".applied")));
                if run.join(".sync-base").is_file() {
                    let _ = write(other.join(".sync-base"), read(run.join(".sync-base")));
                }
            }
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
    if command == "_supervise" {
        let run = rest.first().ok_or("missing run")?;
        supervise::supervise(Path::new(run))?;
        return Ok(0);
    }
    match command.as_str() {
        "start" | "run" => {
            let o = launch::parse_launch(rest, false, command == "run")?;
            let max = o.max;
            let prog = o.progress;
            let full = o.full;
            let run = launch::start(o)?;
            hint_undelivered_other_runs(&run);
            if command == "start" {
                println!("{}", status_line(&run));
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
                Ok(collect(&[run], max, prog, full, true))
            }
        }
        "reply" => {
            let o = launch::parse_launch(rest, true, true)?;
            let max = o.max;
            let prog = o.progress;
            let full = o.full;
            let wait = o.wait;
            let parent = o.run.clone().unwrap_or_default();
            let run = launch::reply(o)?;
            hint_undelivered_other_runs(&run);
            let name = run.file_name().unwrap_or_default().to_string_lossy();
            if wait {
                eprintln!("delegate: started {name} (reply to {parent})");
                Ok(collect(&[run], max, prog, full, true))
            } else {
                println!("{}", status_line(&run));
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
                    "--no-result",
                    "--full",
                    "--progress",
                    "--any",
                    "--stream",
                ],
                &["--max"],
            )?;
            let max = value(&kv, "--max").map(seconds).transpose()?;
            if has(&flags, "--machine") && !pos.is_empty() {
                return Err("--machine does not take run arguments".into());
            }
            let (any, stream) = (has(&flags, "--any"), has(&flags, "--stream"));
            if any && stream {
                return Err("--any and --stream cannot be combined".into());
            }
            let (mut list, hinted) = wait_list(&pos, &flags, false)?;
            if any {
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
            if any || stream {
                return Ok(wait_each(
                    list,
                    (stream && pos.is_empty()).then_some((pos.as_slice(), flags.as_slice())),
                    max,
                    stream,
                    has(&flags, "--progress"),
                    has(&flags, "--full"),
                    !has(&flags, "--no-result"),
                ));
            }
            Ok(collect(
                &list,
                max,
                has(&flags, "--progress"),
                has(&flags, "--full"),
                !has(&flags, "--no-result"),
            ))
        }
        "status" | "list" => {
            let (pos, _, _) = parse_simple(rest, &[], &[])?;
            let list = if pos.is_empty() {
                runs::all_runs()
            } else {
                pos.iter()
                    .map(|x| runs::resolve_head(x))
                    .collect::<Res<Vec<_>>>()?
            };
            for run in list {
                println!("{}", status_line(&run));
            }
            Ok(0)
        }
        "result" => {
            let (pos, flags, _) = parse_simple(rest, &["--path"], &[])?;
            if pos.len() > 1 {
                return Err("result takes one run".into());
            }
            let run = runs::resolve_head(pos.first().map(String::as_str).unwrap_or("last"))?;
            let result = run.join("result.md");
            if !result.exists() {
                eprintln!(
                    "delegate: no result for {} (state: {})",
                    run.file_name().unwrap_or_default().to_string_lossy(),
                    runs::state(&run)
                );
                return Ok(1);
            }
            if has(&flags, "--path") {
                println!("{}", result.display());
            } else {
                let mut out = io::stdout().lock();
                // 全文确实写出后才解除保留，写入失败时 clean 仍会保留它。
                if out
                    .write_all(read(&result).as_bytes())
                    .and_then(|()| out.flush())
                    .is_ok()
                {
                    fs::remove_file(run.join(".truncated")).ok();
                }
            }
            if !runs::active(&runs::state(&run)) {
                touch(run.join(".delivered"));
            }
            Ok(0)
        }
        "diff" => diff(rest),
        "apply" => apply(rest),
        "lane" => lane::lane_command(rest),
        "stop" => stop(rest),
        "clean" => clean(rest),
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
fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let result = main_inner(&args);
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("delegate: {e}");
            std::process::exit(2);
        }
    }
}
