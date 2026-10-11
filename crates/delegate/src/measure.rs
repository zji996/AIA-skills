use crate::common::*;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Seek;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

pub fn validate_argv(value: &Value) -> Res<()> {
    let argv = value
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or("must be a non-empty argv list")?;
    if !argv.iter().all(|v| {
        v.as_str()
            .is_some_and(|s| !s.is_empty() && !s.contains('\0'))
    }) {
        return Err("must contain non-empty strings without NUL".into());
    }
    let program = argv[0].as_str().unwrap();
    if program == "."
        || program == ".."
        || !program
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        return Err("argv[0] must be a program name (no path)".into());
    }
    Ok(())
}

fn online_cpus() -> Vec<usize> {
    let text = fs::read_to_string("/sys/devices/system/cpu/online")
        .or_else(|_| fs::read_to_string("/sys/devices/system/cpu/possible"))
        .unwrap_or_else(|_| {
            let n = std::thread::available_parallelism().map_or(1, |x| x.get());
            format!("0-{}", n.saturating_sub(1))
        });
    parse_cpus(&text).unwrap_or_else(|_| {
        (0..std::thread::available_parallelism().map_or(1, |x| x.get())).collect()
    })
}

pub fn parse_cpus(text: &str) -> Res<Vec<usize>> {
    let mut out = Vec::new();
    for item in text.split(',').map(str::trim).filter(|x| !x.is_empty()) {
        if let Some((a, b)) = item.split_once('-') {
            let start: usize = a.parse().map_err(|_| "invalid CPU list")?;
            let end: usize = b.parse().map_err(|_| "invalid CPU list")?;
            if start > end {
                return Err("CPU range is reversed".into());
            }
            out.extend(start..=end);
        } else {
            out.push(item.parse().map_err(|_| "invalid CPU list")?);
        }
    }
    out.sort_unstable();
    out.dedup();
    if out.is_empty() {
        return Err("CPU list is empty".into());
    }
    Ok(out)
}

fn format_cpus(cpus: &[usize]) -> String {
    let mut out = Vec::new();
    let mut i = 0;
    while i < cpus.len() {
        let start = cpus[i];
        let mut end = start;
        while i + 1 < cpus.len() && cpus[i + 1] == end + 1 {
            i += 1;
            end = cpus[i];
        }
        out.push(if end == start {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
        i += 1;
    }
    out.join(",")
}

pub fn configured_cpus(meta: &Value) -> String {
    if let Some(cpus) = meta["measureCpus"].as_str().filter(|x| !x.is_empty()) {
        return parse_cpus(cpus).map_or_else(|_| cpus.to_string(), |x| format_cpus(&x));
    }
    let all = online_cpus();
    let count = (all.len() / 3).max(2).min(all.len());
    format_cpus(&all[all.len().saturating_sub(count)..])
}

fn dir() -> PathBuf {
    state_dir().join("measure")
}

fn held(path: &Path) -> bool {
    let Ok(f) = OpenOptions::new().read(true).open(path) else {
        return false;
    };
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) != 0 }
}

fn live() -> Vec<(PathBuf, Value)> {
    let Ok(entries) = fs::read_dir(dir()) else {
        return vec![];
    };
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|x| x == "ticket") {
            if held(&path) {
                items.push((path.clone(), json(&path)));
            } else {
                let _ = fs::remove_file(path);
            }
        }
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items
}

pub struct Slot {
    ticket: Option<PathBuf>,
    _file: Option<File>,
    pub queued: f64,
    pub cpus: String,
    run: Option<PathBuf>,
    holders: Vec<TrackedHolder>,
}

#[derive(Clone)]
struct TrackedHolder {
    label: String,
    pid: i32,
}

struct MovedHolder {
    label: String,
    process_count: usize,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(path) = self.ticket.take() {
            // Serialize release with the next reservation's acquisition.  This
            // prevents a new reservation from moving a holder and then having
            // the old slot restore it to all CPUs afterward.
            let guard = path
                .parent()
                .and_then(|dir| lock(&dir.join(".lock"), true, false).ok());
            let _ = fs::remove_file(&path);
            let target = remaining_cpus().unwrap_or_else(|| format_cpus(&online_cpus()));
            // Lane commands started during the reservation were pinned to the
            // remaining CPUs at spawn; give them the freed cores back too.
            let mut holders = self.holders.clone();
            for (label, pid) in crate::lane::holder_processes().unwrap_or_default() {
                if !holders.iter().any(|holder| holder.pid == pid) {
                    holders.push(TrackedHolder { label, pid });
                }
            }
            restore_holders(&self.run, &holders, &target, self.queued, &self.cpus);
            drop(guard);
        }
    }
}

pub fn active_cpus() -> Option<String> {
    live()
        .first()
        .and_then(|(_, value)| value["cpus"].as_str().map(str::to_string))
}

pub fn remaining_cpus() -> Option<String> {
    let reserved = parse_cpus(active_cpus()?.as_str()).ok()?;
    let all = online_cpus();
    let rest = all
        .into_iter()
        .filter(|cpu| !reserved.contains(cpu))
        .collect::<Vec<_>>();
    (!rest.is_empty()).then(|| format_cpus(&rest))
}

fn loadavg() -> Option<f64> {
    read("/proc/loadavg")
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn systemd_usable() -> bool {
    Command::new("systemd-run")
        .args([
            "--quiet",
            "--user",
            "--scope",
            "-p",
            "CPUWeight=10000",
            "true",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn record(
    run: Option<&Path>,
    event: &str,
    label: &str,
    cpus: &str,
    queued: f64,
    diagnostic: Option<&str>,
    moved: &[MovedHolder],
) {
    let holders = crate::lane::holders()
        .unwrap_or_default()
        .into_iter()
        .map(|(label, _, _)| label)
        .collect::<Vec<_>>();
    let moved_values = moved
        .iter()
        .map(|holder| json!({"label":holder.label,"processCount":holder.process_count}))
        .collect::<Vec<_>>();
    let moved_count = moved
        .iter()
        .map(|holder| holder.process_count)
        .sum::<usize>();
    let value = json!({"event":event,"at":iso(),"label":label,"cpus":cpus,"loadavg":loadavg(),"queuedSeconds":(queued*1000.0).round()/1000.0,"heavyHolders":holders,"movedHolders":moved_values,"movedProcessCount":moved_count,"diagnostic":diagnostic});
    eprintln!(
        "delegate measure: {event} {label} cpus={cpus} loadavg={}",
        value["loadavg"]
    );
    if let Some(run) = run {
        let _ = append(run.join("measure.jsonl"), &format!("{value}\n"));
    }
}

fn process_parent(pid: i32) -> Option<i32> {
    let stat = read(format!("/proc/{pid}/stat"));
    let (_, tail) = stat.rsplit_once(") ")?;
    tail.split_whitespace().nth(1)?.parse().ok()
}

fn process_tree(roots: &[i32]) -> Vec<i32> {
    let mut tree = roots
        .iter()
        .copied()
        .filter(|pid| *pid > 0)
        .collect::<Vec<_>>();
    let mut seen = tree.iter().copied().collect::<HashSet<_>>();
    let processes = fs::read_dir("/proc")
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            entry
                .ok()?
                .file_name()
                .to_string_lossy()
                .parse::<i32>()
                .ok()
        })
        .collect::<Vec<_>>();
    loop {
        let mut added = false;
        for pid in &processes {
            if seen.contains(pid) {
                continue;
            }
            if process_parent(*pid).is_some_and(|parent| seen.contains(&parent)) {
                seen.insert(*pid);
                tree.push(*pid);
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    tree
}

fn set_affinity(pid: i32, cpus: &str) -> Result<(), String> {
    let output = Command::new("taskset")
        .args(["-a", "-p", "-c", cpus, &pid.to_string()])
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if detail.is_empty() {
            format!("taskset exited with {}", output.status)
        } else {
            detail
        })
    }
}

fn affinity_diagnostic(
    run: Option<&Path>,
    label: &str,
    cpus: &str,
    queued: f64,
    diagnostic: String,
) {
    record(
        run,
        "diagnostic",
        label,
        cpus,
        queued,
        Some(&diagnostic),
        &[],
    );
}

fn move_holders(
    run: Option<&Path>,
    cpus: &str,
    queued: f64,
) -> (Vec<TrackedHolder>, Vec<MovedHolder>) {
    let holders = match crate::lane::holder_processes() {
        Ok(holders) => holders,
        Err(error) => {
            affinity_diagnostic(
                run,
                "lane holders",
                cpus,
                queued,
                format!("cannot list lane holders: {error}"),
            );
            return (Vec::new(), Vec::new());
        }
    };
    let tracked = holders
        .iter()
        .map(|(label, pid)| TrackedHolder {
            label: label.clone(),
            pid: *pid,
        })
        .collect::<Vec<_>>();
    let mut moved = Vec::new();
    for (label, root) in holders {
        let tree = process_tree(&[root]);
        let mut process_count = 0;
        if tree.is_empty() {
            affinity_diagnostic(
                run,
                &label,
                cpus,
                queued,
                format!("lane holder process {root} exited before affinity update"),
            );
        }
        for pid in tree {
            match set_affinity(pid, cpus) {
                Ok(()) => process_count += 1,
                Err(error) => affinity_diagnostic(
                    run,
                    &label,
                    cpus,
                    queued,
                    format!("cannot set affinity for lane holder process {pid}: {error}"),
                ),
            }
        }
        moved.push(MovedHolder {
            label,
            process_count,
        });
    }
    (tracked, moved)
}

fn restore_holders(
    run: &Option<PathBuf>,
    holders: &[TrackedHolder],
    cpus: &str,
    queued: f64,
    reservation_cpus: &str,
) {
    for holder in holders {
        let tree = process_tree(&[holder.pid]);
        for pid in tree {
            if let Err(error) = set_affinity(pid, cpus) {
                affinity_diagnostic(
                    run.as_deref(),
                    &holder.label,
                    reservation_cpus,
                    queued,
                    format!("cannot restore affinity for lane holder process {pid}: {error}"),
                );
            }
        }
    }
}

pub fn waiting_ahead(run: &Path) -> Option<usize> {
    let path = fs::read_dir(run)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .starts_with("measure-waiting-")
                && held(p)
        })?;
    let value = json(&path);
    Some(value["ahead"].as_u64().unwrap_or(1) as usize)
}

pub fn queued_seconds(run: &Path) -> f64 {
    read(run.join("measure-wait"))
        .split_whitespace()
        .filter_map(|x| x.parse::<f64>().ok())
        .sum::<f64>()
        + fs::read_dir(run)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let name = p.file_name()?.to_string_lossy();
                if name.starts_with("measure-waiting-") && held(&p) {
                    Some((epoch() - json(&p)["epoch"].as_f64().unwrap_or(epoch())).max(0.0))
                } else {
                    None
                }
            })
            .sum::<f64>()
}

pub fn acquire(
    label: &str,
    run: Option<&Path>,
    meta: &Value,
    mut waiting: impl FnMut(usize, &[String]),
) -> Res<Slot> {
    let cpus = configured_cpus(meta);
    let dir = dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let me =
        json!({"pid":std::process::id(),"label":label,"cpus":cpus,"since":iso(),"epoch":epoch()});
    let ticket = dir.join(format!("{:020}-{}.ticket", now_ns(), std::process::id()));
    let file = {
        let _guard = lock(&dir.join(".lock"), true, false).map_err(|e| e.to_string())?;
        locked_file(&ticket, &me.to_string())?
    };
    let marker = run.map(|p| p.join(format!("measure-waiting-{}", std::process::id())));
    let markfile = marker
        .as_ref()
        .map(|p| locked_file(p, &me.to_string()))
        .transpose()?;
    let started = Instant::now();
    loop {
        let items = live();
        let pos = items.iter().position(|(p, _)| p == &ticket).unwrap_or(0);
        if pos == 0 {
            break;
        }
        let labels = items[..pos]
            .iter()
            .map(|(_, v)| s(v, "label").to_string())
            .collect::<Vec<_>>();
        waiting(pos, &labels);
        if let Some(marker) = &marker {
            if let Ok(mut f) = OpenOptions::new().write(true).open(marker) {
                let value = json!({"ticket":ticket,"ahead":pos,"epoch":json(marker)["epoch"].as_f64().unwrap_or(epoch())});
                let _ = f.set_len(0);
                let _ = f.seek(std::io::SeekFrom::Start(0));
                let _ = f.write_all(value.to_string().as_bytes());
                let _ = f.flush();
            }
        }
        if crate::lane::stopped() {
            drop(markfile);
            let _ = fs::remove_file(&ticket);
            return Err("stopped while queued for measurement reservation".into());
        }
        if let Some((path, _)) = items.first() {
            if let Ok(f) = OpenOptions::new().read(true).open(path) {
                unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH) };
            }
        }
    }
    let queued = started.elapsed().as_secs_f64();
    if let Some(marker) = marker {
        let _ = fs::remove_file(marker);
    }
    drop(markfile);
    if let Some(run) = run {
        let _ = append(run.join("measure-wait"), &format!("{queued:.3}\n"));
    }
    // Hold the measurement lock while changing existing lane holders.  A
    // concurrent Slot::drop uses the same lock, so a successor reservation
    // cannot be undone by the predecessor's restore step.
    let guard = lock(&dir.join(".lock"), true, false).ok();
    let holder_cpus = remaining_cpus().unwrap_or_else(|| format_cpus(&online_cpus()));
    let (holders, moved) = move_holders(run, &holder_cpus, queued);
    record(run, "start", label, &cpus, queued, None, &moved);
    drop(guard);
    Ok(Slot {
        ticket: Some(ticket),
        _file: Some(file),
        queued,
        cpus,
        run: run.map(Path::to_path_buf),
        holders,
    })
}

pub fn execute_argv(run: &Path, real: &str, args: &[String]) -> Res<i32> {
    let meta = json(run.join("meta.json"));
    let label = format!(
        "{} {}",
        real,
        args.iter()
            .map(|x| shell_quote(x))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let slot = acquire(&label, Some(run), &meta, |ahead, labels| {
        eprintln!(
            "delegate measure: 等待测量预约，前面 {ahead} 个{}",
            if labels.is_empty() {
                String::new()
            } else {
                format!("：{}", labels.join("；"))
            }
        );
    })?;
    let mut command = Command::new("taskset");
    command
        .args(["-c", &slot.cpus, real])
        .args(args)
        .env("DELEGATE_MEASURE_CPUS", &slot.cpus)
        .env(
            "DELEGATE_MEASURE_CORES",
            parse_cpus(&slot.cpus)?.len().to_string(),
        )
        .env("DELEGATE_MEASURE_HELD", "1")
        .current_dir(s(&meta, "workdir"));
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut diagnostic = None;
    let mut child = if systemd_usable() {
        Command::new("systemd-run")
            .args([
                "--quiet",
                "--user",
                "--scope",
                "-p",
                "CPUWeight=10000",
                "--",
            ])
            .arg("taskset")
            .args(["-c", &slot.cpus, real])
            .args(args)
            .env("DELEGATE_MEASURE_CPUS", &slot.cpus)
            .env(
                "DELEGATE_MEASURE_CORES",
                parse_cpus(&slot.cpus)?.len().to_string(),
            )
            .env("DELEGATE_MEASURE_HELD", "1")
            .current_dir(s(&meta, "workdir"))
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| e.to_string())?
    } else {
        diagnostic = Some("systemd-run unavailable; direct execution");
        command.spawn().map_err(|e| e.to_string())?
    };
    if diagnostic.is_some() {
        record(
            Some(run),
            "diagnostic",
            &label,
            &slot.cpus,
            slot.queued,
            diagnostic,
            &[],
        );
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    record(
        Some(run),
        "end",
        &label,
        &slot.cpus,
        slot.queued,
        diagnostic,
        &[],
    );
    Ok(status.code().unwrap_or(1))
}

pub fn run_lane(command: &str, cwd: &Path, run: Option<&Path>, meta: &Value) -> Res<i32> {
    let slot = acquire(command, run, meta, |ahead, labels| {
        eprintln!(
            "delegate lane --measure: 等待测量预约，前面 {ahead} 个{}",
            if labels.is_empty() {
                String::new()
            } else {
                format!("：{}", labels.join("；"))
            }
        );
    })?;
    let mut c = Command::new("taskset");
    c.args(["-c", &slot.cpus, "sh", "-c", command])
        .current_dir(cwd)
        .env("DELEGATE_MEASURE_CPUS", &slot.cpus)
        .env(
            "DELEGATE_MEASURE_CORES",
            parse_cpus(&slot.cpus)?.len().to_string(),
        )
        .env("DELEGATE_MEASURE_HELD", "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = if systemd_usable() {
        Command::new("systemd-run")
            .args([
                "--user",
                "--scope",
                "-p",
                "CPUWeight=10000",
                "--",
                "taskset",
                "-c",
                &slot.cpus,
                "sh",
                "-c",
                command,
            ])
            .current_dir(cwd)
            .env("DELEGATE_MEASURE_CPUS", &slot.cpus)
            .env(
                "DELEGATE_MEASURE_CORES",
                parse_cpus(&slot.cpus)?.len().to_string(),
            )
            .env("DELEGATE_MEASURE_HELD", "1")
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| e.to_string())?
    } else {
        eprintln!("delegate measure: systemd-run unavailable; direct execution");
        record(
            run,
            "diagnostic",
            command,
            &slot.cpus,
            slot.queued,
            Some("systemd-run unavailable; direct execution"),
            &[],
        );
        c.status().map_err(|e| e.to_string())?
    };
    record(run, "end", command, &slot.cpus, slot.queued, None, &[]);
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Child;

    fn affinity(pid: u32) -> Option<Vec<usize>> {
        let output = Command::new("taskset")
            .args(["-pc", &pid.to_string()])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let list = text.rsplit_once(':')?.1.trim();
        parse_cpus(list).ok()
    }

    fn finish_child(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn lane_holder_affinity_yields_reserved_cpu_and_restores() {
        let all = online_cpus();
        if all.len() < 2 {
            eprintln!("skipping affinity reservation test: fewer than two online CPUs");
            return;
        }
        if Command::new("taskset").arg("--version").output().is_err() {
            eprintln!("skipping affinity reservation test: taskset unavailable");
            return;
        }
        let state = std::env::temp_dir().join(format!(
            "delegate-measure-test-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&state).unwrap();
        let old_state = std::env::var_os("XDG_STATE_HOME");
        let old_max_heavy = std::env::var_os("DELEGATE_MAX_HEAVY");
        std::env::set_var("XDG_STATE_HOME", &state);
        std::env::set_var("DELEGATE_MAX_HEAVY", "1");

        let result = (|| -> Res<()> {
            let lane_dir = state.join("delegate/lane");
            fs::create_dir_all(&lane_dir).map_err(|e| e.to_string())?;
            let mut child = Command::new("sleep")
                .arg("30")
                .spawn()
                .map_err(|e| e.to_string())?;
            let pid = child.id();
            let before = match affinity(pid) {
                Some(affinity) if affinity.len() >= 2 => affinity,
                _ => {
                    eprintln!("skipping affinity reservation test: cannot inspect child affinity");
                    finish_child(&mut child);
                    return Ok(());
                }
            };
            if all.iter().any(|cpu| !before.contains(cpu)) {
                eprintln!("skipping affinity reservation test: process cannot use all online CPUs");
                finish_child(&mut child);
                return Ok(());
            }
            let reserved = before[0];
            let rest = before
                .iter()
                .copied()
                .filter(|cpu| *cpu != reserved)
                .collect::<Vec<_>>();
            if let Err(error) = set_affinity(pid as i32, &format_cpus(&rest)) {
                eprintln!(
                    "skipping affinity reservation test: cannot set child affinity ({error})"
                );
                finish_child(&mut child);
                return Ok(());
            }
            let _ = set_affinity(pid as i32, &format_cpus(&before));
            let ticket = lane_dir.join(format!("{:020}-{}.ticket", now_ns(), pid));
            let ticket_file = locked_file(
                &ticket,
                &json!({"pid":pid,"label":"fake sleep holder","since":iso()}).to_string(),
            )?;
            let meta = json!({"measureCpus":reserved.to_string()});
            let slot = acquire("affinity test", None, &meta, |_, _| {})?;
            let during = affinity(pid).ok_or("cannot inspect reserved child affinity")?;
            if during.contains(&reserved) {
                finish_child(&mut child);
                drop(ticket_file);
                return Err("lane holder still includes reserved CPU".into());
            }
            drop(slot);
            let after = affinity(pid).ok_or("cannot inspect restored child affinity")?;
            if all.iter().any(|cpu| !after.contains(cpu)) {
                finish_child(&mut child);
                drop(ticket_file);
                return Err("lane holder did not regain all online CPUs".into());
            }
            finish_child(&mut child);
            drop(ticket_file);
            Ok(())
        })();

        if let Some(value) = old_state {
            std::env::set_var("XDG_STATE_HOME", value);
        } else {
            std::env::remove_var("XDG_STATE_HOME");
        }
        if let Some(value) = old_max_heavy {
            std::env::set_var("DELEGATE_MAX_HEAVY", value);
        } else {
            std::env::remove_var("DELEGATE_MAX_HEAVY");
        }
        let _ = fs::remove_dir_all(&state);
        result.unwrap();
    }
}
