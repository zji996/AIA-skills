use crate::common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn root() -> PathBuf { state_dir().join("resources") }

fn proc_start(pid: u32) -> String {
    let stat = read(format!("/proc/{pid}/stat"));
    stat.rsplit_once(") ").and_then(|(_, rest)| {
        let fields = rest.split_whitespace().collect::<Vec<_>>();
        if fields.first() == Some(&"Z") { None } else { fields.get(19).copied() }
    }).unwrap_or("").to_string()
}

pub fn command(args: &[String]) -> Res<i32> {
    if args.len() < 4 { return Err("_resource needs run, resource, executable and arguments".into()); }
    let mut child = Command::new(&args[2]).args(&args[3..])
        .stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit())
        .spawn().map_err(|e| e.to_string())?;
    let dir = root();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join(format!("{}-{}-{}.json", args[1], child.id(), now_ns()));
    let run = Path::new(&args[0]);
    write_json(&file, &json!({"resource":args[1],"run":run.file_name().unwrap_or_default().to_string_lossy(),
        "pid":child.id(),"procStart":proc_start(child.id()),"started":epoch(),
        "command":format!("{} {}", Path::new(&args[2]).file_name().unwrap_or_default().to_string_lossy(), args[3..].join(" "))}))?;
    let status = child.wait().map_err(|e| e.to_string())?;
    let _ = fs::remove_file(file);
    use std::os::unix::process::ExitStatusExt;
    Ok(status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
}

pub fn occupants(name: &str) -> Vec<Value> {
    let mut found = Vec::new();
    if let Ok(entries) = fs::read_dir(root()) {
        for entry in entries.flatten() {
            let path = entry.path();
            let record = json(&path);
            let pid = record["pid"].as_u64().unwrap_or(0) as u32;
            if pid == 0 || proc_start(pid).is_empty() || proc_start(pid) != s(&record,"procStart") {
                let _ = fs::remove_file(path);
            } else if s(&record,"resource") == name {
                found.push(record);
            }
        }
    }
    found
}

pub fn busy(name: &str) -> i32 {
    let found = occupants(name);
    for item in &found {
        println!("{} · {} · {} 秒", s(item,"run"), s(item,"command"),
            (epoch() - item["started"].as_f64().unwrap_or(epoch())).max(0.0) as u64);
    }
    if found.is_empty() { 1 } else { 0 }
}

pub fn occupied_for_run(run: &str) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(root()) {
        for entry in entries.flatten() {
            let name = s(&json(entry.path()), "resource").to_string();
            if !name.is_empty() && occupants(&name).iter().any(|item| s(item,"run") == run) && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}
