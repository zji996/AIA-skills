mod agents;
mod changes;
mod common;
mod lane;
mod launch;
mod runs;
mod supervise;
mod worktree;
use common::*;
use serde_json::json;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn status_line(run: &Path) -> String {
    let mut status = runs::status(run);
    let id = status["run"].clone();
    status.as_object_mut().unwrap().remove("run");
    let rest = status.to_string();
    format!("{{\"run\":{},{}", id, &rest[1..])
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
fn print_answer(run: &Path, full: bool) {
    let result = read(run.join("result.md"));
    let limit = setting("RESULT_CHARS", "6000")
        .parse::<usize>()
        .unwrap_or(6000);
    println!(
        "\n===== result: {} ({} chars) =====",
        run.file_name().unwrap_or_default().to_string_lossy(),
        result.chars().count()
    );
    if full || result.chars().count() <= limit {
        println!("{}", result.trim_end_matches('\n'));
    } else {
        let tail = result
            .chars()
            .rev()
            .take(limit)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        println!(
            "[showing the last {limit} chars; full answer: {}]\n...{}",
            run.join("result.md").display(),
            tail.trim_end_matches('\n')
        );
    }
    println!(
        "===== end: {} =====",
        run.file_name().unwrap_or_default().to_string_lossy()
    );
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
        println!("{}", status_line(run));
        if runs::active(&state) {
            code = 75;
            continue;
        }
        if !["delivered", "answered"].contains(&state.as_str()) && code == 0 {
            code = 1;
        }
        changes::print_changes(run);
        let has_result = run.join("result.md").is_file();
        if has_result && show_result {
            print_answer(run, full);
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
            if run.join(".delivered").exists() || has(&flags, "--force") {
                targets.push(run);
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
        let run = runs::resolve(reference)?;
        let old = runs::state(&run);
        if !runs::active(&old) && runs::agent_alive(&run) {
            let pid = read(run.join("agent.pid"))
                .trim()
                .parse::<i32>()
                .unwrap_or(0);
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
                        kill_group(pid, 1.0);
                    }
                }
                write_json(run.join("summary.json"), &json!({"state":"stopped"}))?;
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
    let (pos, flags, _) = parse_simple(args, &["--stat", "--total"], &[])?;
    let run = runs::resolve(pos.first().map(String::as_str).unwrap_or("last"))?;
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
            }
        }
    }
    Ok(code)
}
fn main_inner(args: &[String]) -> Res<i32> {
    let Some((command, rest)) = args.split_first() else {
        return Err("the following arguments are required: command".into());
    };
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
            let parent = o.run.clone().unwrap_or_default();
            let run = launch::reply(o)?;
            eprintln!(
                "delegate: started {} (reply to {parent})",
                run.file_name().unwrap_or_default().to_string_lossy()
            );
            Ok(collect(&[run], max, prog, full, true))
        }
        "wait" => {
            let (pos, flags, kv) = parse_simple(
                rest,
                &["--all", "--no-result", "--full", "--progress"],
                &["--max"],
            )?;
            let max = value(&kv, "--max").map(seconds).transpose()?;
            let list = if has(&flags, "--all") || pos.is_empty() {
                let v = runs::all_runs()
                    .into_iter()
                    .filter(|r| runs::active(&runs::state(r)) || !r.join(".delivered").exists())
                    .collect::<Vec<_>>();
                if v.is_empty() {
                    eprintln!("delegate: no active or undelivered runs");
                    return Ok(0);
                }
                v
            } else {
                pos.iter()
                    .map(|x| runs::resolve(x))
                    .collect::<Res<Vec<_>>>()?
                    .into_iter()
                    .collect::<Vec<_>>()
            };
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
                    .map(|x| runs::resolve(x))
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
            let run = runs::resolve(pos.first().map(String::as_str).unwrap_or("last"))?;
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
                print!("{}", read(&result));
                io::stdout().flush().ok();
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
