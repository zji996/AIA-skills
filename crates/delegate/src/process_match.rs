//! Habit-compatible process matching, bounded by the run's recorded systemd scopes.
use crate::common::*;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

pub fn command(args: &[String]) -> Res<i32> {
    if args.len() < 3 {
        return Err("internal process matcher requires run, command and executable".into());
    }
    let run = Path::new(&args[0]);
    let tool = &args[1];
    let scopes = crate::cleanup::scopes(run)
        .into_iter()
        .filter(|(_, root, _)| root.join("cgroup.procs").is_file())
        .collect::<Vec<_>>();
    // A recorded scope alone is insufficient: fallback agents must not acquire
    // permission merely because an earlier attempt of this run had a scope.
    let own = read("/proc/self/cgroup");
    let names: Vec<_> = scopes.iter().map(|(unit, _, _)| unit.as_str()).collect();
    // procps matches the entire cgroup v2 name from /proc/PID/cgroup,
    // not the systemd unit basename.
    let groups = scopes
        .iter()
        .map(|(_, root, _)| {
            format!(
                "/{}",
                root.strip_prefix("/sys/fs/cgroup")
                    .unwrap_or(root)
                    .display()
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let supported = Command::new(&args[2])
        .arg("--help")
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("--cgroup"));
    if !supported
        || names.is_empty()
        || !own
            .lines()
            .any(|l| names.iter().any(|n| l.split('/').any(|p| p == *n)))
    {
        eprintln!("{tool}: 本次任务只能结束自己启动的进程；请用 kill <pid>（pid 取自你启动时的 $! 或 pid 文件）");
        return Ok(2);
    }
    let incoming = &args[3..];
    let mut forwarded = Vec::new();
    if tool == "killall" {
        let mut i = 0;
        let mut names = Vec::new();
        let mut positional = false;
        while i < incoming.len() {
            let arg = &incoming[i];
            if positional {
                names.push(arg.clone());
                i += 1;
                continue;
            }
            if arg == "--" {
                positional = true;
            } else if [
                "--cgroup", "--ns", "--nslist", "-P", "--parent", "-u", "--user",
            ]
            .contains(&arg.as_str())
            {
                forwarded.push(arg.clone());
                i += 1;
                forwarded.push(
                    incoming
                        .get(i)
                        .ok_or_else(|| format!("killall: {arg} requires a value"))?
                        .clone(),
                );
            } else if arg == "-n" {
                forwarded.push("--ns".into());
                i += 1;
                forwarded.push(
                    incoming
                        .get(i)
                        .ok_or("killall: -n requires a namespace pid")?
                        .clone(),
                );
            } else if arg == "-s" || arg == "--signal" {
                i += 1;
                forwarded.push(format!(
                    "--signal={}",
                    incoming.get(i).ok_or("killall: -s requires a signal")?
                ));
            } else if arg.starts_with('-') {
                forwarded.push(arg.clone());
            } else {
                names.push(arg.clone());
            }
            i += 1;
        }
        if names.is_empty() {
            return Err("killall: specify a process name; use kill <pid> for a known pid".into());
        }
        // Escape POSIX ERE metacharacters; killall names are literal, not regexes.
        let escape = |name: &String| {
            name.chars()
                .map(|c| {
                    if ".^$*+?()[]{}|\\".contains(c) {
                        format!("\\{c}")
                    } else {
                        c.to_string()
                    }
                })
                .collect::<String>()
        };
        forwarded.extend([
            "-x".into(),
            "--cgroup".into(),
            groups.clone(),
            "--".into(),
            format!(
                "({})",
                names.iter().map(escape).collect::<Vec<_>>().join("|")
            ),
        ]);
    } else {
        // Options before `--` so a caller's end-of-options marker cannot turn
        // our bound into a pattern. Own scope option is last (procps uses last).
        let end = incoming
            .iter()
            .position(|a| a == "--")
            .unwrap_or(incoming.len());
        forwarded.extend_from_slice(&incoming[..end]);
        forwarded.extend(["--cgroup".into(), groups]);
        forwarded.extend_from_slice(&incoming[end..]);
    }
    // Inverse matching in pgrep negates the cgroup predicate too. Fail closed.
    if tool == "pgrep"
        && incoming.iter().take_while(|a| a.as_str() != "--").any(|a| {
            (a.len() > 2 && a.starts_with("--") && "--inverse".starts_with(a.as_str()))
                || (a.starts_with('-') && !a.starts_with("--") && a[1..].contains('v'))
        })
    {
        eprintln!("pgrep: 反向匹配无法限定本次任务；请用 kill <pid> 操作已知进程");
        return Ok(2);
    }
    let error = Command::new(&args[2]).args(forwarded).exec();
    Err(format!("{tool}: {error}; 请用 kill <pid>"))
}

pub fn prepare(run: &Path, path: &std::ffi::OsStr) -> Res<()> {
    use std::os::unix::fs::PermissionsExt;
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    for tool in ["pkill", "killall", "pgrep"] {
        let actual = if tool == "killall" { "pkill" } else { tool };
        let binary = std::env::split_paths(path)
            .map(|dir| dir.join(actual))
            .find(|p| {
                p.is_file()
                    && std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
            })
            .unwrap_or_else(|| Path::new("/usr/bin").join(actual));
        let shim = run.join("agent-shims").join(tool);
        write(
            &shim,
            format!(
                "#!/bin/sh\nexec {} _process-match {} {} {} \"$@\"\n",
                shell_quote(&executable.to_string_lossy()),
                shell_quote(&run.to_string_lossy()),
                tool,
                shell_quote(&binary.to_string_lossy())
            ),
        )?;
        let hidden = run.join("agent-shims").join(format!(".process-{tool}"));
        std::fs::copy(&shim, &hidden).map_err(|e| e.to_string())?;
        std::fs::set_permissions(hidden, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        std::fs::set_permissions(shim, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
