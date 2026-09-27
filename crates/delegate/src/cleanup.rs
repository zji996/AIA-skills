use crate::common::{json, lock, write_json};
use serde_json::{json as value, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

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

fn members(run: &Path, pgid: i32) -> Vec<(i32, String, Vec<u16>, bool)> {
    let uid = unsafe { libc::geteuid() };
    let ports = listening();
    let marker = format!("DELEGATE_RUN_DIR={}", run.display());
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_string_lossy().parse::<i32>().ok() else {
            continue;
        };
        let path = entry.path();
        if !path.metadata().is_ok_and(|m| m.uid() == uid) || !alive(pid) {
            continue;
        }
        let in_group = pgid > 0 && unsafe { libc::getpgid(pid) } == pgid;
        let tagged = fs::read(path.join("environ"))
            .ok()
            .is_some_and(|env| env.split(|b| *b == 0).any(|v| v == marker.as_bytes()));
        if !in_group && !tagged {
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
    let found = members(run, pgid);
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
