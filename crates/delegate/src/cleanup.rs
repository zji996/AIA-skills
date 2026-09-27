use crate::common::{append, json, lock, read, setting, write_json};
use serde_json::{json as value, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static SYSTEMD: OnceLock<bool> = OnceLock::new();

pub fn probe_systemd() {
    SYSTEMD.get_or_init(|| {
        setting("CGROUP", "1") != "0"
            && std::env::var_os("XDG_RUNTIME_DIR").is_some()
            && Command::new("systemd-run")
                .args(["--user", "--scope", "--quiet", "--", "true"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
    });
}

pub fn scoped_command(run: &Path, role: &str, program: &str, args: &[String]) -> Command {
    probe_systemd();
    if SYSTEMD.get() != Some(&true) {
        let mut command = Command::new(program);
        command.args(args);
        return command;
    }
    let name: String = run
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .take(100)
        .collect();
    let raw = format!(
        "delegate-{name}-{role}-{:x}-{:x}",
        std::process::id(),
        crate::common::now_ns()
    );
    let unit: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let Ok(_guard) = lock(&run.join("scopes.lock"), true, false) else {
        let mut command = Command::new(program);
        command.args(args);
        return command;
    };
    if append(run.join("scopes"), &format!("{unit}.scope\n")).is_err() {
        let mut command = Command::new(program);
        command.args(args);
        return command;
    }
    let mut command = Command::new("systemd-run");
    command
        .args(["--user", "--scope", "--quiet", "--collect", "--unit"])
        .arg(unit)
        .arg("--")
        .arg(program)
        .args(args);
    command
}

fn scope_dirs(root: &Path, pids: &mut BTreeSet<i32>) {
    if let Ok(data) = fs::read_to_string(root.join("cgroup.procs")) {
        pids.extend(data.lines().filter_map(|line| line.parse::<i32>().ok()));
    }
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                scope_dirs(&entry.path(), pids);
            }
        }
    }
}

fn scopes(run: &Path) -> Vec<(String, PathBuf, BTreeSet<i32>)> {
    if !run.join("scopes").is_file() {
        return Vec::new();
    }
    let Ok(_guard) = lock(&run.join("scopes.lock"), true, false) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for unit in read(run.join("scopes")).lines() {
        if !unit.starts_with("delegate-")
            || !unit.ends_with(".scope")
            || !unit
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        {
            continue;
        }
        let Ok(output) = Command::new("systemctl")
            .args(["--user", "show", "-p", "ControlGroup", "--value", unit])
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let group = String::from_utf8_lossy(&output.stdout);
        let group = group.trim();
        let path = Path::new(group);
        if !path.is_absolute()
            || path.file_name().is_none_or(|name| name != unit)
            || !path
                .components()
                .skip(1)
                .all(|part| matches!(part, Component::Normal(_)))
        {
            continue;
        }
        let root = Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/'));
        let mut pids = BTreeSet::new();
        scope_dirs(&root, &mut pids);
        found.push((unit.to_string(), root, pids));
    }
    found
}

fn kill_scope(unit: &str, root: &Path, pids: &BTreeSet<i32>) {
    if pids.is_empty() {
        return;
    }
    let signal = |name| {
        let _ = Command::new("systemctl")
            .args(["--user", "kill", &format!("--signal={name}"), unit])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    };
    // Terminate gracefully first so agents can flush their sessions; SIGKILL only what remains.
    signal("SIGTERM");
    let start = Instant::now();
    while pids.iter().any(|pid| alive(*pid)) && start.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(50));
    }
    if pids.iter().any(|pid| alive(*pid)) && fs::write(root.join("cgroup.kill"), "1").is_err() {
        signal("SIGKILL");
    }
}

fn listening() -> BTreeMap<String, u16> {
    let mut ports = BTreeMap::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(data) = fs::read_to_string(file) {
            for line in data.lines().skip(1) {
                let fields: Vec<_> = line.split_whitespace().collect();
                if fields.len() > 9 && fields[3] == "0A" {
                    if let Some(hex) = fields[1].rsplit(':').next() {
                        if let Ok(port) = u16::from_str_radix(hex, 16) {
                            ports.insert(fields[9].to_string(), port);
                        }
                    }
                }
            }
        }
    }
    ports
}

fn alive(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(") ").map(|(_, tail)| !tail.starts_with('Z')))
        .unwrap_or(false)
}

fn members(
    run: &Path,
    pgid: i32,
    scope_pids: &BTreeSet<i32>,
) -> Vec<(i32, String, Vec<u16>, bool)> {
    let uid = unsafe { libc::geteuid() };
    let ports = listening();
    let marker = format!("DELEGATE_RUN_DIR={}", run.display());
    let supervisor = read(run.join("pid")).trim().parse::<i32>().unwrap_or(0);
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_string_lossy().parse::<i32>().ok() else {
            continue;
        };
        let path = entry.path();
        if pid == std::process::id() as i32
            || pid == supervisor
            || !path.metadata().is_ok_and(|m| m.uid() == uid)
            || !alive(pid)
        {
            continue;
        }
        let in_group = pgid > 0 && unsafe { libc::getpgid(pid) } == pgid;
        let tagged = fs::read(path.join("environ"))
            .ok()
            .is_some_and(|env| env.split(|b| *b == 0).any(|v| v == marker.as_bytes()));
        if !in_group && !tagged && !scope_pids.contains(&pid) {
            continue;
        }
        let command = fs::read(path.join("cmdline")).unwrap_or_default();
        let command = command
            .split(|b| *b == 0)
            .filter(|x| !x.is_empty())
            .map(|x| String::from_utf8_lossy(x).into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let command: String = command.chars().take(200).collect();
        let mut listening_ports = BTreeSet::new();
        if let Ok(fds) = fs::read_dir(path.join("fd")) {
            for fd in fds.flatten() {
                if let Ok(target) = fs::read_link(fd.path()) {
                    let text = target.to_string_lossy();
                    if let Some(inode) = text
                        .strip_prefix("socket:[")
                        .and_then(|x| x.strip_suffix(']'))
                    {
                        if let Some(port) = ports.get(inode) {
                            listening_ports.insert(*port);
                        }
                    }
                }
            }
        }
        found.push((
            pid,
            command,
            listening_ports.into_iter().collect(),
            !in_group,
        ));
    }
    found
}

pub fn record(run: &Path, pgid: i32) {
    let Ok(_guard) = lock(&run.join("cleanup.lock"), true, false) else {
        return;
    };
    let scopes = scopes(run);
    let scope_pids = scopes
        .iter()
        .flat_map(|(_, _, pids)| pids.iter().copied())
        .collect();
    let found = members(run, pgid, &scope_pids);
    let supervisor = read(run.join("pid")).trim().parse::<i32>().unwrap_or(0);
    for (unit, root, pids) in &scopes {
        if !pids.contains(&(std::process::id() as i32)) && !pids.contains(&supervisor) {
            kill_scope(unit, root, pids);
        }
    }
    if found.is_empty() {
        return;
    }
    let file = run.join("cleanup.json");
    let mut previous = json(&file);
    if !previous.is_object() {
        previous = value!({"processes":{}});
    }
    for (pid, command, ports, escaped) in &found {
        previous["processes"][pid.to_string()] = value!({"command":command,"ports":ports});
        if *escaped {
            unsafe {
                libc::kill(*pid, libc::SIGTERM);
            }
        }
    }
    let start = Instant::now();
    while found
        .iter()
        .any(|(pid, _, _, escaped)| *escaped && alive(*pid))
        && start.elapsed() < Duration::from_secs(2)
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    for (pid, _, _, escaped) in &found {
        if *escaped && alive(*pid) {
            unsafe {
                libc::kill(*pid, libc::SIGKILL);
            }
        }
    }
    let _ = write_json(file, &previous);
}

pub fn summary(run: &Path) -> Option<Value> {
    let saved = json(run.join("cleanup.json"));
    let processes = saved["processes"].as_object()?;
    if processes.is_empty() {
        return None;
    }
    let mut ports = BTreeSet::new();
    let mut commands = Vec::new();
    for (pid, item) in processes {
        commands.push(format!("{pid}: {}", item["command"].as_str().unwrap_or("")));
        if let Some(items) = item["ports"].as_array() {
            ports.extend(items.iter().filter_map(Value::as_u64));
        }
    }
    Some(value!({"terminated":processes.len(),"ports":ports,"commands":commands}))
}
