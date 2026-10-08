use crate::common::{append, json, lock, read, s, setting, write_json};
use serde_json::{json as value, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static SYSTEMD: OnceLock<bool> = OnceLock::new();
static DOCKER_FAILURE: OnceLock<String> = OnceLock::new();
static GROUP_KEEP: OnceLock<Mutex<BTreeMap<i32, BTreeSet<String>>>> = OnceLock::new();

// These build services deliberately outlive a task; socket ports are not an identity.
const SHARED_EXECUTABLES: &[&str] = &["sccache"];

fn keep_executables(run: &Path) -> BTreeSet<String> {
    let meta = json(run.join("meta.json"));
    SHARED_EXECUTABLES
        .iter()
        .copied()
        .chain(
            meta["cleanupKeepExecutables"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        )
        .map(str::to_string)
        .collect()
}

fn executable(pid: i32) -> String {
    fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .map(|name| name.strip_suffix(" (deleted)").unwrap_or(&name).to_string())
        .unwrap_or_else(|| read(format!("/proc/{pid}/comm")).trim().to_string())
}

fn group_candidates(pgid: i32) -> Vec<i32> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<i32>().ok())
        .filter(|pid| alive(*pid) && unsafe { libc::getpgid(*pid) } == pgid)
        .collect()
}

fn group_keep(pgid: i32, pids: &[i32]) -> BTreeSet<String> {
    let mut keep: BTreeSet<_> = SHARED_EXECUTABLES
        .iter()
        .map(|name| name.to_string())
        .collect();
    if let Some(saved) = GROUP_KEEP.get().and_then(|groups| groups.lock().ok()) {
        if let Some(names) = saved.get(&pgid) {
            keep.extend(names.iter().cloned());
        }
    }
    // Stop may execute in another delegate process, so recover the task's config from its marker.
    for pid in pids {
        if let Ok(env) = fs::read(format!("/proc/{pid}/environ")) {
            for entry in env.split(|byte| *byte == 0) {
                if let Some(run) = entry.strip_prefix(b"DELEGATE_RUN_DIR=") {
                    if let Ok(run) = std::str::from_utf8(run) {
                        keep.extend(keep_executables(Path::new(run)));
                    }
                }
            }
        }
    }
    keep
}

pub fn group_has_cleanup_members(pgid: i32) -> bool {
    let pids = group_candidates(pgid);
    let keep = group_keep(pgid, &pids);
    pids.into_iter().any(|pid| !keep.contains(&executable(pid)))
}

pub fn signal_group(pgid: i32, signal: i32) {
    let pids = group_candidates(pgid);
    let keep = group_keep(pgid, &pids);
    if pids.iter().any(|pid| keep.contains(&executable(*pid))) {
        for pid in pids {
            if !keep.contains(&executable(pid)) {
                unsafe {
                    libc::kill(pid, signal);
                }
            }
        }
    } else {
        crate::common::killpg(pgid, signal);
    }
}

// Bound every bus request, including discovery: a wedged user manager must not hold up delivery.
fn systemctl(args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("systemctl");
    command
        .args(["--user", "--no-pager", "--no-ask-password"])
        .args(args)
        .env("LC_ALL", "C")
        .env("SYSTEMD_COLORS", "0")
        .env("SYSTEMD_URLIFY", "0");
    bounded_output(command, Duration::from_secs(3))
}

pub fn bounded_output(mut command: Command, timeout: Duration) -> Result<String, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::common::group(&mut command);
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut data = String::new();
        let _ = stdout.read_to_string(&mut data);
        data
    });
    let start = Instant::now();
    let result = loop {
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        if rc == 0 && unsafe { info.si_pid() } != 0 {
            break Ok(());
        }
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                break Err(error.to_string());
            }
        }
        if start.elapsed() >= timeout {
            break Err("timed out".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Keep the child unreaped until signalling its group, preventing PGID reuse.
    crate::common::killpg(child.id() as i32, libc::SIGKILL);
    let status = child.wait().map_err(|e| e.to_string());
    let data = reader.join().unwrap_or_default();
    result?;
    let status = status?;
    if status.success() {
        Ok(data)
    } else {
        Err(format!("exit {status}"))
    }
}

#[derive(Default)]
pub struct Containers {
    pub removed: usize,
    pub diagnostic: Option<String>,
}

impl Containers {
    pub fn json(&self) -> Value {
        value!({"containersRemoved": self.removed, "diagnostic": self.diagnostic})
    }
}

fn label_below(path: &Path, root: &Path) -> bool {
    path.is_absolute()
        && root.is_absolute()
        && !path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        && path.starts_with(root)
}

// Match recorded paths lexically: compose's working directory may already have been deleted.
pub fn containers(root: &Path, missing_only: bool) -> Containers {
    if let Some(message) = DOCKER_FAILURE.get() {
        return Containers {
            removed: 0,
            diagnostic: Some(message.clone()),
        };
    }
    let mut report = Containers::default();
    let deadline = Instant::now() + Duration::from_secs(20);
    let docker = |args: &[&str]| -> Result<String, String> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("timed out".into());
        }
        let mut command = Command::new("docker");
        command.args(args);
        bounded_output(command, remaining)
    };
    let mut collect = || -> Result<(), String> {
        let listed = docker(&[
            "ps",
            "-aq",
            "--filter",
            "label=com.docker.compose.project.working_dir",
        ])?;
        let ids: Vec<_> = listed.split_whitespace().collect();
        if ids.is_empty() {
            return Ok(());
        }
        let mut args = vec!["inspect"];
        args.extend(ids);
        let inspected: Value = serde_json::from_str(&docker(&args)?).map_err(|e| e.to_string())?;
        let items = inspected
            .as_array()
            .ok_or("invalid docker inspect response")?;
        for item in items {
            let path = Path::new(s(
                &item["Config"]["Labels"],
                "com.docker.compose.project.working_dir",
            ));
            if !label_below(path, root) || (missing_only && path.try_exists().ok() != Some(false)) {
                continue;
            }
            let id = s(item, "Id");
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            docker(&["rm", "-f", "-v", id])?;
            report.removed += 1;
        }
        Ok(())
    };
    if let Err(e) = collect() {
        let message = format!("docker: container cleanup skipped: {e}");
        if DOCKER_FAILURE.set(message.clone()).is_ok() {
            eprintln!("delegate: {message}");
        }
        report.diagnostic = Some(message);
    }
    report
}

// systemctl show uses C-escaped, optionally quoted words for Environment and ExecStart argv.
fn tokens(input: &str) -> Option<Vec<(String, bool)>> {
    let mut words = Vec::new();
    let mut word = Vec::new();
    let mut quote = None;
    let mut literal = false;
    let mut chars = input.bytes().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            b'\\' => {
                literal = true;
                let escaped = match chars.next()? {
                    b'x' => {
                        let a = (chars.next()? as char).to_digit(16)?;
                        let b = (chars.next()? as char).to_digit(16)?;
                        (a * 16 + b) as u8
                    }
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b's' => b' ',
                    b'\\' => b'\\',
                    b'\'' => b'\'',
                    b'"' => b'"',
                    _ => return None,
                };
                word.push(escaped);
            }
            b'\'' | b'"' if quote == Some(ch) => quote = None,
            b'\'' | b'"' if quote.is_none() => {
                quote = Some(ch);
                literal = true;
            }
            ch if ch.is_ascii_whitespace() && quote.is_none() => {
                if !word.is_empty() {
                    words.push((String::from_utf8(std::mem::take(&mut word)).ok()?, literal));
                }
                literal = false;
            }
            _ => word.push(ch),
        }
    }
    if quote.is_some() {
        return None;
    }
    if !word.is_empty() {
        words.push((String::from_utf8(word).ok()?, literal));
    }
    Some(words)
}

fn words(input: &str) -> Option<Vec<String>> {
    Some(tokens(input)?.into_iter().map(|(word, _)| word).collect())
}

fn below(path: &str, root: &Path) -> bool {
    let path = Path::new(path);
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        || !path.starts_with(root)
    {
        return false;
    }
    // Resolve the nearest existing ancestor too, so missing scripts cannot hide a source link.
    let mut ancestor = path;
    while !ancestor.exists() && !ancestor.is_symlink() {
        let Some(parent) = ancestor.parent() else {
            return false;
        };
        ancestor = parent;
    }
    ancestor
        .canonicalize()
        .is_ok_and(|path| path.starts_with(root))
}

fn unit_match(
    properties: &BTreeMap<&str, &str>,
    run: &Path,
    root: Option<&Path>,
) -> Option<&'static str> {
    let marker = format!("DELEGATE_RUN_DIR={}", run.display());
    if words(properties.get("Environment").copied().unwrap_or(""))
        .is_some_and(|env| env.iter().any(|item| item == &marker))
    {
        return Some("environment");
    }
    let root = root?;
    let directory = properties.get("WorkingDirectory").copied().unwrap_or("");
    // WorkingDirectory is a single property, and may contain literal spaces.
    if below(directory, root) {
        return Some("working-directory");
    }
    if words(directory).is_some_and(|items| items.len() == 1 && below(&items[0], root)) {
        return Some("working-directory");
    }
    let exec = properties.get("ExecStart").copied().unwrap_or("");
    let mut field_start = true;
    let mut argv = false;
    for (item, literal) in tokens(exec).unwrap_or_default() {
        if !literal && matches!(item.as_str(), "{" | "}" | ";") {
            field_start = true;
            argv = false;
            continue;
        }
        let path = if field_start {
            field_start = false;
            argv = item.starts_with("argv[]=");
            item.strip_prefix("path=")
                .or_else(|| item.strip_prefix("argv[]="))
        } else if argv {
            Some(item.as_str())
        } else {
            None
        };
        if path.is_some_and(|path| below(path, root)) {
            return Some("exec-start");
        }
    }
    None
}

fn stop_unit(unit: &str) -> Result<bool, String> {
    let stop = systemctl(&["stop", "--no-block", "--", unit]);
    let mut killed = false;
    if stop.as_ref().is_err_and(|e| e == "timed out") {
        systemctl(&["kill", "--signal=SIGKILL", "--", unit])?;
        killed = true;
    } else {
        stop?;
    }
    let mut start = Instant::now();
    loop {
        let state = systemctl(&["show", "-p", "ActiveState", "--value", "--", unit])?;
        if matches!(state.trim(), "inactive" | "failed" | "") {
            return Ok(killed);
        }
        if start.elapsed() >= Duration::from_secs(3) {
            if killed {
                return Err("still active after SIGKILL".into());
            }
            systemctl(&["kill", "--signal=SIGKILL", "--", unit])?;
            killed = true;
            start = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn user_units(run: &Path, saved: &mut Value) -> usize {
    let mut stopped = 0;
    let keep = keep_executables(run);
    let mut diagnostics = BTreeSet::new();
    if let Some(items) = saved["diagnostics"].as_array() {
        diagnostics.extend(items.iter().filter_map(Value::as_str).map(str::to_string));
    }
    let mut collect = || -> Result<(), String> {
        let listed = systemctl(&[
            "list-units",
            "--type=service,scope",
            "--all",
            "--plain",
            "--no-legend",
        ])?;
        let meta = json(run.join("meta.json"));
        let root = Path::new(s(&meta["worktree"], "path"));
        let root = if root.is_absolute()
            && root != Path::new(s(&meta["worktree"], "source"))
            && !crate::runs::all_runs().iter().any(|other| {
                other != run
                    && crate::runs::active(&crate::runs::state(other))
                    && s(&json(other.join("meta.json"))["worktree"], "path")
                        == root.to_string_lossy()
            }) {
            root.canonicalize().ok()
        } else {
            None
        };
        if root.is_none() {
            diagnostics.insert("systemd: path matching disabled (in-place, missing or shared active worktree); units without DELEGATE_RUN_DIR are skipped".to_string());
        }
        let owned: BTreeSet<_> = read(run.join("scopes"))
            .lines()
            .map(str::to_string)
            .collect();
        let units: BTreeSet<_> = listed
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        for unit in units {
            if owned.contains(unit)
                || !(unit.ends_with(".service") || unit.ends_with(".scope"))
                || unit.starts_with('-')
                || !unit
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.@:\\".contains(&b))
            {
                continue;
            }
            let data = match systemctl(&[
                "show",
                "-p",
                "WorkingDirectory",
                "-p",
                "ExecStart",
                "-p",
                "ControlGroup",
                "-p",
                "Environment",
                "-p",
                "ActiveState",
                "--",
                unit,
            ]) {
                Ok(data) => data,
                Err(e) => {
                    diagnostics.insert(format!("systemd: show {unit}: {e}"));
                    continue;
                }
            };
            let properties: BTreeMap<_, _> = data
                .lines()
                .filter_map(|line| line.split_once('='))
                .collect();
            if matches!(
                properties.get("ActiveState").copied(),
                Some("inactive" | "failed")
            ) {
                continue;
            }
            let Some(reason) = unit_match(&properties, run, root.as_deref()) else {
                continue;
            };
            // A service matching the task marker may itself be a shared build daemon.
            if unit_kept(&properties, &keep) || unit_contains_kept(&properties, &keep) {
                continue;
            }
            match stop_unit(unit) {
                Ok(killed) => {
                    stopped += 1;
                    if !saved["units"].is_object() {
                        saved["units"] = value!({});
                    }
                    saved["units"][unit] = value!({"matchedBy":reason,"killed":killed});
                }
                Err(e) => {
                    diagnostics.insert(format!("systemd: stop {unit}: {e}"));
                }
            }
        }
        Ok(())
    };
    if let Err(e) = collect() {
        diagnostics.insert(format!("systemd: user unit discovery skipped: {e}"));
    }
    saved["diagnostics"] = value!(diagnostics);
    stopped
}

fn unit_kept(properties: &BTreeMap<&str, &str>, keep: &BTreeSet<String>) -> bool {
    let mut field_start = true;
    for (token, literal) in
        tokens(properties.get("ExecStart").copied().unwrap_or("")).unwrap_or_default()
    {
        if !literal && matches!(token.as_str(), "{" | "}" | ";") {
            field_start = true;
            continue;
        }
        if field_start {
            field_start = false;
            if token.strip_prefix("path=").is_some_and(|path| {
                Path::new(path)
                    .file_name()
                    .is_some_and(|name| keep.contains(name.to_string_lossy().as_ref()))
            }) {
                return true;
            }
        }
    }
    false
}

fn unit_contains_kept(properties: &BTreeMap<&str, &str>, keep: &BTreeSet<String>) -> bool {
    let group = properties.get("ControlGroup").copied().unwrap_or("");
    let path = Path::new(group);
    if group == "/"
        || !path.is_absolute()
        || !path
            .components()
            .skip(1)
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return false;
    }
    let mut pids = BTreeSet::new();
    scope_dirs(
        &Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/')),
        &mut pids,
    );
    pids.iter().any(|pid| keep.contains(&executable(*pid)))
}

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

fn kill_scope(unit: &str, root: &Path, pids: &BTreeSet<i32>, keep: &BTreeSet<String>) {
    if pids.is_empty() {
        return;
    }
    if pids.iter().any(|pid| keep.contains(&executable(*pid))) {
        // Never signal the whole unit/cgroup if a shared daemon remains inside it.
        let owned: Vec<_> = pids
            .iter()
            .copied()
            .filter(|pid| !keep.contains(&executable(*pid)))
            .collect();
        for pid in &owned {
            unsafe {
                libc::kill(*pid, libc::SIGTERM);
            }
        }
        let start = Instant::now();
        while owned.iter().any(|pid| alive(*pid)) && start.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(50));
        }
        for pid in owned {
            if alive(pid) && !keep.contains(&executable(pid)) {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
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
    keep: &BTreeSet<String>,
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
        if keep.contains(&executable(pid)) {
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

pub fn record(run: &Path, pgid: i32) -> usize {
    let Ok(_guard) = lock(&run.join("cleanup.lock"), true, false) else {
        return 0;
    };
    let keep = keep_executables(run);
    if pgid > 0 {
        if let Ok(mut groups) = GROUP_KEEP
            .get_or_init(|| Mutex::new(BTreeMap::new()))
            .lock()
        {
            groups.insert(pgid, keep.clone());
        }
    }
    let scopes = scopes(run);
    let scope_pids = scopes
        .iter()
        .flat_map(|(_, _, pids)| pids.iter().copied())
        .collect();
    let found = members(run, pgid, &scope_pids, &keep);
    let executables: BTreeMap<_, _> = found
        .iter()
        .map(|(pid, _, _, _)| (*pid, executable(*pid)))
        .collect();
    let supervisor = read(run.join("pid")).trim().parse::<i32>().unwrap_or(0);
    for (unit, root, pids) in &scopes {
        if !pids.contains(&(std::process::id() as i32)) && !pids.contains(&supervisor) {
            kill_scope(unit, root, pids, &keep);
        }
    }
    let file = run.join("cleanup.json");
    let mut previous = json(&file);
    if !previous.is_object() {
        previous = value!({"processes":{}});
    }
    if !previous["processes"].is_object() {
        previous["processes"] = value!({});
    }
    for (pid, command, ports, escaped) in &found {
        previous["processes"][pid.to_string()] =
            value!({"command":command,"ports":ports,"executable":executables[pid]});
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
    let stopped = user_units(run, &mut previous);
    let _ = write_json(file, &previous);
    stopped
}

pub fn summary(run: &Path) -> Option<Value> {
    let saved = json(run.join("cleanup.json"));
    let processes = saved["processes"].as_object()?;
    let mut ports = BTreeSet::new();
    let mut commands = Vec::new();
    let mut executables = BTreeSet::new();
    for (pid, item) in processes {
        if let Some(name) = item["executable"].as_str().filter(|name| !name.is_empty()) {
            executables.insert(name.to_string());
        }
        commands.push(format!("{pid}: {}", item["command"].as_str().unwrap_or("")));
        if let Some(items) = item["ports"].as_array() {
            ports.extend(items.iter().filter_map(Value::as_u64));
        }
    }
    let units = saved["units"]
        .as_object()
        .map(|items| items.len())
        .unwrap_or(0);
    Some(
        value!({"terminated":processes.len(),"ports":ports,"commands":commands,"executables":executables,
        "systemdStopped":units,"units":saved["units"],"diagnostics":saved["diagnostics"]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Child;

    struct Fixture {
        root: PathBuf,
        children: Vec<Child>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "delegate-cleanup-{}-{}",
                std::process::id(),
                crate::common::now_ns()
            ));
            fs::create_dir_all(&root).unwrap();
            Self {
                root,
                children: Vec::new(),
            }
        }

        fn sleeper(&mut self, name: &str, tagged: bool) -> i32 {
            let binary = self.root.join(name);
            fs::copy("/bin/sleep", &binary).unwrap();
            let mut command = Command::new(binary);
            command
                .arg("60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if tagged {
                command.env("DELEGATE_RUN_DIR", &self.root);
            }
            crate::common::group(&mut command);
            let child = command.spawn().unwrap();
            let pid = child.id() as i32;
            self.children.push(child);
            pid
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for child in &mut self.children {
                crate::common::killpg(child.id() as i32, libc::SIGKILL);
                let _ = child.wait();
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn shared_daemons_survive_marker_and_group_cleanup() {
        let mut fixture = Fixture::new();
        write_json(
            fixture.root.join("meta.json"),
            &value!({"cleanupKeepExecutables":["custom-cache"]}),
        )
        .unwrap();
        let cache = fixture.sleeper("sccache", true);
        let custom = fixture.sleeper("custom-cache", true);
        let server = fixture.sleeper("preview-server", true);
        record(&fixture.root, cache);
        crate::common::end_group(cache, 0.01);
        crate::common::kill_group(custom, 0.01);
        assert!(alive(cache));
        assert!(alive(custom));
        assert!(!alive(server));
        let summary = summary(&fixture.root).unwrap();
        assert_eq!(summary["terminated"], 1);
        assert_eq!(summary["executables"], value!(["preview-server"]));
        let saved = json(fixture.root.join("cleanup.json"));
        assert_eq!(
            saved["processes"][server.to_string()]["executable"],
            "preview-server"
        );
        assert!(saved["processes"].get(cache.to_string()).is_none());
    }

    #[test]
    fn scopes_with_shared_daemons_kill_only_other_members() {
        let mut fixture = Fixture::new();
        let cache = fixture.sleeper("sccache", false);
        let server = fixture.sleeper("preview-server", false);
        kill_scope(
            "not-a-real-unit.scope",
            &fixture.root,
            &BTreeSet::from([cache, server]),
            &keep_executables(&fixture.root),
        );
        assert!(alive(cache));
        assert!(!alive(server));
        assert!(!fixture.root.join("cgroup.kill").exists());
    }

    #[test]
    fn mixed_process_group_keeps_daemon_and_terminates_other_members() {
        let mut fixture = Fixture::new();
        fs::copy("/bin/sleep", fixture.root.join("sccache")).unwrap();
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!(
            "{}/sccache 60 & echo $! > {}/cache.pid; /bin/sleep 60 & echo $! > {}/server.pid; wait",
            fixture.root.display(), fixture.root.display(), fixture.root.display()
        ))
            .env("DELEGATE_RUN_DIR", &fixture.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::common::group(&mut command);
        let child = command.spawn().unwrap();
        let pgid = child.id() as i32;
        fixture.children.push(child);
        let start = Instant::now();
        while read(fixture.root.join("server.pid"))
            .trim()
            .parse::<i32>()
            .is_err()
            && start.elapsed() < Duration::from_secs(3)
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let cache: i32 = read(fixture.root.join("cache.pid")).trim().parse().unwrap();
        let server: i32 = read(fixture.root.join("server.pid"))
            .trim()
            .parse()
            .unwrap();
        // Wait until both forked children have executed; the shell writes PID files before exec.
        while executable(cache) != "sccache" && start.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(10));
        }
        record(&fixture.root, pgid);
        crate::common::end_group(pgid, 0.1);
        assert!(alive(cache));
        assert!(!alive(server));
        assert!(!group_has_cleanup_members(pgid));
    }

    #[test]
    fn systemd_shared_executable_matches_only_the_program() {
        let keep = BTreeSet::from(["sccache".to_string(), "custom-cache".to_string()]);
        for exec in [
            "{ path=/usr/bin/sccache ; argv[]=/usr/bin/sccache ; }",
            "{ path=\"/path with spaces/custom-cache\" ; argv[]=custom-cache ; }",
        ] {
            assert!(unit_kept(&BTreeMap::from([("ExecStart", exec)]), &keep));
        }
        assert!(!unit_kept(
            &BTreeMap::from([(
                "ExecStart",
                "{ path=/usr/bin/sh ; argv[]=/usr/bin/sh -c \"echo ; path=/usr/bin/sccache\" ; }"
            )]),
            &keep
        ));
    }
}
