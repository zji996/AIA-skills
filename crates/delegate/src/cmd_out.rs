use crate::common::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const DEFAULT_MAX_BYTES: usize = 12 * 1024;
pub const BLOCK_LINES: usize = 40;
pub const ACTIVE_ENV: &str = "DELEGATE_CMD_OUT_ACTIVE";

const ERROR_MARKERS: &[&str] = &["error[", "error:", "FAILED", "panicked"];

pub fn default_commands() -> Vec<Vec<String>> {
    vec![
        vec!["cargo".into()],
        vec!["pnpm".into()],
        vec!["npm".into()],
        vec!["pytest".into()],
        vec!["python".into(), "-m".into(), "pytest".into()],
        vec!["go".into(), "test".into()],
    ]
}

fn validate_program_name(program: &str) -> Res<()> {
    if program.is_empty()
        || program == "."
        || program == ".."
        || !program
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        return Err("cmdOut.commands argv[0] must be a program name (no path)".into());
    }
    Ok(())
}

fn parse_one_command(item: &Value) -> Res<Vec<String>> {
    match item {
        Value::String(s) => {
            if s.contains('\0') {
                return Err("cmdOut.commands must not contain NUL".into());
            }
            let parts: Vec<String> = s.split_whitespace().map(str::to_string).collect();
            if parts.is_empty() {
                return Err("cmdOut.commands entries must be non-empty".into());
            }
            validate_program_name(&parts[0])?;
            Ok(parts)
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                return Err("cmdOut.commands entries must be non-empty argv lists".into());
            }
            let mut parts = Vec::with_capacity(arr.len());
            for tok in arr {
                let Some(s) = tok.as_str() else {
                    return Err("cmdOut.commands argv items must be strings".into());
                };
                if s.is_empty() || s.contains('\0') {
                    return Err("cmdOut.commands argv items must be non-empty strings without NUL".into());
                }
                parts.push(s.to_string());
            }
            validate_program_name(&parts[0])?;
            Ok(parts)
        }
        _ => Err("cmdOut.commands entries must be a command string or argv string list".into()),
    }
}

/// Validate `cmdOut` config: `false | true | { enabled?: bool, commands?: (string | string[])[], maxBytes?: number }`.
pub fn validate_config(v: Option<&Value>) -> Res<()> {
    let Some(val) = v else {
        return Ok(());
    };
    if val.is_null() || val.is_boolean() {
        return Ok(());
    }
    let Some(obj) = val.as_object() else {
        return Err("cmdOut must be a boolean or object {enabled?, commands?, maxBytes?}".into());
    };
    for key in obj.keys() {
        if !["enabled", "commands", "maxBytes"].contains(&key.as_str()) {
            return Err(format!("cmdOut: unknown field '{key}'"));
        }
    }
    if let Some(enabled) = obj.get("enabled") {
        if !enabled.is_boolean() {
            return Err("cmdOut.enabled must be a boolean".into());
        }
    }
    if let Some(max_bytes) = obj.get("maxBytes") {
        if !max_bytes
            .as_u64()
            .is_some_and(|n| n >= 1 && usize::try_from(n).is_ok())
        {
            return Err("cmdOut.maxBytes must be a positive integer".into());
        }
    }
    if let Some(cmds) = obj.get("commands") {
        let Some(arr) = cmds.as_array() else {
            return Err("cmdOut.commands must be a list".into());
        };
        for item in arr {
            parse_one_command(item)?;
        }
    }
    Ok(())
}

/// Resolve effective `cmdOut` candidate configuration from merged config value.
/// Returns `None` when disabled (`cmdOut: false`, `cmdOut.enabled: false`, or empty `commands`).
pub fn resolve_config(config_value: &Value) -> Option<Value> {
    let spec = config_value.get("cmdOut");
    match spec {
        Some(Value::Bool(false)) => return None,
        Some(Value::Object(obj)) if obj.get("enabled") == Some(&json!(false)) => return None,
        _ => {}
    }
    let commands = match spec.and_then(Value::as_object).and_then(|o| o.get("commands")) {
        Some(Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                if let Ok(parts) = parse_one_command(item) {
                    out.push(parts);
                }
            }
            out
        }
        _ => default_commands(),
    };
    if commands.is_empty() {
        return None;
    }
    let max_bytes = spec
        .and_then(Value::as_object)
        .and_then(|o| o.get("maxBytes"))
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_MAX_BYTES);
    Some(json!({
        "commands": commands,
        "maxBytes": max_bytes,
    }))
}

pub fn parse_commands(v: &Value) -> Vec<Vec<String>> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| parse_one_command(item).ok())
        .collect()
}

/// Format truncated output when raw bytes exceed threshold.
pub fn format_truncated(raw: &[u8], log_path: &Path) -> String {
    let size_kb = raw.len().div_ceil(1024);
    let text = String::from_utf8_lossy(raw);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let tail_start = total.saturating_sub(BLOCK_LINES);

    let first_err = lines
        .iter()
        .position(|line| ERROR_MARKERS.iter().any(|marker| line.contains(marker)));

    let mut out = String::new();
    if let Some(err_idx) = first_err {
        out.push_str(&format!(
            "[输出 {size_kb} KB，全文在 {}；下面是首个错误起 40 行与末尾 40 行]\n",
            log_path.display()
        ));
        let err_end = (err_idx + BLOCK_LINES).min(total);
        if err_end >= tail_start {
            let start = err_idx.min(tail_start);
            for line in &lines[start..total] {
                out.push_str(line);
                out.push('\n');
            }
        } else {
            for line in &lines[err_idx..err_end] {
                out.push_str(line);
                out.push('\n');
            }
            out.push_str(&format!("... [省略 {} 行] ...\n", tail_start - err_end));
            for line in &lines[tail_start..total] {
                out.push_str(line);
                out.push('\n');
            }
        }
    } else {
        out.push_str(&format!(
            "[输出 {size_kb} KB，全文在 {}；下面是末尾 40 行]\n",
            log_path.display()
        ));
        for line in &lines[tail_start..total] {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn allocate_log_file(dir: &Path) -> Res<(u64, PathBuf, File)> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut seq = 1u64;
    if let Ok(rd) = fs::read_dir(dir) {
        for ent in rd.flatten() {
            if let Some(stem) = ent
                .file_name()
                .to_str()
                .and_then(|name| name.strip_suffix(".log"))
            {
                if let Ok(n) = stem.parse::<u64>() {
                    if n >= seq {
                        seq = n + 1;
                    }
                }
            }
        }
    }
    loop {
        let path = dir.join(format!("{seq:03}.log"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((seq, path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                seq += 1;
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

struct CapturedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    merged: Vec<u8>,
    code: i32,
}

fn cloexec_pipe() -> Res<(File, File)> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!("pipe2 failed: {}", io::Error::last_os_error()));
    }
    let reader = unsafe { File::from_raw_fd(fds[0]) };
    let writer = unsafe { File::from_raw_fd(fds[1]) };
    Ok((reader, writer))
}

fn run_captured(real: &str, args: &[String]) -> Res<CapturedOutput> {
    let (out_r, out_w) = cloexec_pipe()?;
    let (err_r, err_w) = cloexec_pipe()?;

    let mut child = Command::new(real)
        .args(args)
        .env(ACTIVE_ENV, "1")
        .stdout(Stdio::from(out_w))
        .stderr(Stdio::from(err_w))
        .spawn()
        .map_err(|e| format!("cmd-out spawn {real}: {e}"))?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut merged = Vec::new();
    let mut fds = [
        libc::pollfd {
            fd: out_r.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: err_r.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let mut buf = [0u8; 8192];
    while fds[0].fd >= 0 || fds[1].fd >= 0 {
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("poll failed: {err}"));
        }
        for (idx, pfd) in fds.iter_mut().enumerate() {
            if pfd.fd < 0 {
                continue;
            }
            if pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                let n = unsafe {
                    libc::read(
                        pfd.fd,
                        buf.as_mut_ptr().cast::<libc::c_void>(),
                        buf.len(),
                    )
                };
                if n > 0 {
                    let chunk = &buf[..n as usize];
                    if idx == 0 {
                        stdout.extend_from_slice(chunk);
                    } else {
                        stderr.extend_from_slice(chunk);
                    }
                    merged.extend_from_slice(chunk);
                } else if n == 0 {
                    pfd.fd = -1;
                } else {
                    let err = io::Error::last_os_error();
                    if err.kind() != io::ErrorKind::Interrupted {
                        pfd.fd = -1;
                    }
                }
            }
        }
    }
    drop(out_r);
    drop(err_r);

    let status = child.wait().map_err(|e| e.to_string())?;
    let code = status.code().unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|sig| 128 + sig).unwrap_or(1)
    });
    Ok(CapturedOutput {
        stdout,
        stderr,
        merged,
        code,
    })
}

/// Execute `real` with `args`, preserving stdout/stderr when <=maxBytes and spilling >maxBytes to `<run>/cmd-out/<seq>.log`.
pub fn execute_argv(run: &Path, real: &str, args: &[String]) -> Res<i32> {
    let meta = json(run.join("meta.json"));
    let max_bytes = meta["cmdOut"]["maxBytes"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_MAX_BYTES);
    execute_argv_with_limit(run, real, args, max_bytes)
}

pub fn execute_argv_with_limit(
    run: &Path,
    real: &str,
    args: &[String],
    max_bytes: usize,
) -> Res<i32> {
    let captured = run_captured(real, args)?;
    if captured.merged.len() <= max_bytes {
        if !captured.stdout.is_empty() {
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(&captured.stdout)
                .map_err(|e| e.to_string())?;
            stdout.flush().map_err(|e| e.to_string())?;
        }
        if !captured.stderr.is_empty() {
            let mut stderr = io::stderr().lock();
            stderr
                .write_all(&captured.stderr)
                .map_err(|e| e.to_string())?;
            stderr.flush().map_err(|e| e.to_string())?;
        }
        return Ok(captured.code);
    }

    let out_dir = run.join("cmd-out");
    let (seq, log_path, mut file) = allocate_log_file(&out_dir)?;
    file.write_all(&captured.merged)
        .map_err(|e| e.to_string())?;
    let _ = file.flush();

    let abs_path = std::path::absolute(&log_path).unwrap_or(log_path);
    let emitted = format_truncated(&captured.merged, &abs_path);
    {
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(emitted.as_bytes())
            .map_err(|e| e.to_string())?;
        stdout.flush().map_err(|e| e.to_string())?;
    }

    let truncated_bytes = captured.merged.len().saturating_sub(emitted.len()) as u64;
    let rec = json!({
        "seq": seq,
        "log": abs_path.to_string_lossy(),
        "rawBytes": captured.merged.len(),
        "emittedBytes": emitted.len(),
        "truncatedBytes": truncated_bytes,
        "exitCode": captured.code,
    });
    let _ = append_line(&out_dir.join("stats.jsonl"), &format!("{rec}\n"));
    Ok(captured.code)
}

fn append_line(path: &Path, line: &str) -> Res<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    f.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CmdOutRunStats {
    pub triggers: u64,
    pub truncated_bytes: u64,
}

pub fn count_stats(run: &Path) -> CmdOutRunStats {
    let stats_path = run.join("cmd-out").join("stats.jsonl");
    if !stats_path.is_file() {
        return CmdOutRunStats::default();
    }
    let mut out = CmdOutRunStats::default();
    for line in read(&stats_path).lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.is_object() {
            out.triggers += 1;
            out.truncated_bytes = out
                .truncated_bytes
                .saturating_add(v["truncatedBytes"].as_u64().unwrap_or(0));
        }
    }
    out
}

/// Check whether a bash command line invokes a compile or test command.
pub fn is_compile_test_command(cmd: &str, configured: &[Vec<String>]) -> bool {
    const BUILTIN_SINGLE: &[&str] = &[
        "cargo", "pnpm", "npm", "pytest", "rustc", "tsc", "vitest", "jest", "make", "ctest",
        "mvn", "gradle", "phpunit", "rspec",
    ];
    let normalized = cmd
        .replace("&&", "\n")
        .replace("||", "\n")
        .replace([';', '|'], "\n");
    for seg in normalized.lines() {
        let mut tokens: Vec<&str> = seg.split_whitespace().collect();
        while let Some(&first) = tokens.first() {
            if (first.contains('=') && !first.starts_with('-'))
                || matches!(first, "env" | "command" | "time" | "nice")
            {
                tokens.remove(0);
            } else if first == "timeout" {
                tokens.remove(0);
                // skip timeout flags/duration
                while tokens.first().is_some_and(|t| t.starts_with('-')) {
                    tokens.remove(0);
                }
                if !tokens.is_empty() {
                    tokens.remove(0);
                }
            } else {
                break;
            }
        }
        let Some(&first) = tokens.first() else {
            continue;
        };
        let prog = Path::new(first)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(first);

        if BUILTIN_SINGLE.contains(&prog) {
            return true;
        }
        if prog == "go" && tokens.get(1).is_some_and(|t| matches!(*t, "test" | "build" | "vet")) {
            return true;
        }
        if matches!(prog, "python" | "python3")
            && tokens.get(1) == Some(&"-m")
            && tokens
                .get(2)
                .is_some_and(|t| matches!(*t, "pytest" | "unittest"))
        {
            return true;
        }
        for prefix in configured {
            if prefix.is_empty() || tokens.len() < prefix.len() {
                continue;
            }
            if prog == prefix[0]
                && prefix[1..]
                    .iter()
                    .enumerate()
                    .all(|(i, arg)| tokens.get(i + 1) == Some(&arg.as_str()))
            {
                return true;
            }
        }
    }
    false
}

/// Scan `<run>/events.jsonl` once to count total author tool steps and compile/test bash steps.
pub fn count_author_steps(run: &Path, configured: &[Vec<String>]) -> (u64, u64) {
    let ev_path = run.join("events.jsonl");
    if !ev_path.is_file() {
        return (0, 0);
    }
    let mut author_steps = 0u64;
    let mut compile_test_steps = 0u64;
    for line in read(&ev_path).lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = s(&v, "e");
        if matches!(kind, "bash" | "read" | "write" | "edit" | "tool") {
            author_steps += 1;
            if kind == "bash" && is_compile_test_command(s(&v, "cmd"), configured) {
                compile_test_steps += 1;
            }
        }
    }
    (author_steps, compile_test_steps)
}

#[derive(Debug, Default, Clone)]
pub struct CmdOutGroupMetrics {
    pub runs: u64,
    pub triggers: u64,
    pub truncated_bytes: u64,
    pub author_steps: u64,
    pub author_input_tokens: i64,
    pub compile_test_steps: u64,
    pub accept_first_pass: u64,
    pub accept_total: u64,
}

impl CmdOutGroupMetrics {
    pub fn avg_input_per_step(&self) -> Option<f64> {
        (self.author_steps > 0).then_some(self.author_input_tokens as f64 / self.author_steps as f64)
    }

    pub fn avg_compile_test_steps(&self) -> Option<f64> {
        (self.runs > 0).then_some(self.compile_test_steps as f64 / self.runs as f64)
    }

    pub fn accept_first_pass_rate(&self) -> Option<f64> {
        (self.accept_total > 0).then_some(self.accept_first_pass as f64 / self.accept_total as f64)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "runs": self.runs,
            "triggers": self.triggers,
            "truncatedBytes": self.truncated_bytes,
            "authorSteps": self.author_steps,
            "authorInputTokens": self.author_input_tokens,
            "avgInputPerStep": self.avg_input_per_step().map(|v| (v * 10.0).round() / 10.0),
            "compileTestSteps": self.compile_test_steps,
            "avgCompileTestSteps": self.avg_compile_test_steps().map(|v| (v * 100.0).round() / 100.0),
            "acceptFirstPassRuns": self.accept_first_pass,
            "acceptTotalRuns": self.accept_total,
            "acceptFirstPassRate": self.accept_first_pass_rate().map(|v| (v * 1000.0).round() / 1000.0),
        })
    }
}

pub fn format_ab_summary_human(groups: &BTreeMap<String, CmdOutGroupMetrics>) -> String {
    if groups.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n大输出落盘 A/B（cmdOut）:\n");
    out.push_str(&format!(
        "  {:<6}  {:<5}  {:<12}  {:<14}  {:<14}  {:<12}\n",
        "分组", "Runs", "平均每步输入", "编译测试步数", "验收一次通过率", "触发/截断"
    ));
    for (group, m) in groups {
        let avg_in = m
            .avg_input_per_step()
            .map(|v| crate::usage::format_token_compact(v.round() as i64))
            .unwrap_or_else(|| "-".to_string());
        let ct_str = match m.avg_compile_test_steps() {
            Some(avg) => format!("{} ({avg:.1}/run)", m.compile_test_steps),
            None => "-".to_string(),
        };
        let acc_str = match m.accept_first_pass_rate() {
            Some(rate) => format!("{:.0}% ({}/{})", rate * 100.0, m.accept_first_pass, m.accept_total),
            None => "-".to_string(),
        };
        let trig_str = if m.triggers > 0 {
            format!("{}次/{}KB", m.triggers, m.truncated_bytes.div_ceil(1024))
        } else {
            "0".to_string()
        };
        out.push_str(&format!(
            "  {:<6}  {:<5}  {:<12}  {:<14}  {:<14}  {:<12}\n",
            group, m.runs, avg_in, ct_str, acc_str, trig_str
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "delegate-cmd-out-test-{}-{}-{}",
            name,
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn validate_and_resolve_config_variants() {
        assert!(validate_config(None).is_ok());
        assert!(validate_config(Some(&json!(false))).is_ok());
        assert!(validate_config(Some(&json!(true))).is_ok());
        assert!(validate_config(Some(&json!({"enabled": true, "commands": ["cargo", ["go", "test"]], "maxBytes": 4096}))).is_ok());
        assert!(validate_config(Some(&json!({"unknown": 1}))).is_err());
        assert!(validate_config(Some(&json!({"maxBytes": 0}))).is_err());
        assert!(validate_config(Some(&json!({"commands": ["/usr/bin/cargo"]}))).is_err());

        assert!(resolve_config(&json!({"cmdOut": false})).is_none());
        assert!(resolve_config(&json!({"cmdOut": {"enabled": false}})).is_none());
        let def = resolve_config(&json!({})).unwrap();
        assert_eq!(def["maxBytes"], json!(DEFAULT_MAX_BYTES));
        assert_eq!(parse_commands(&def["commands"]).len(), 6);
    }

    #[test]
    fn format_truncated_with_and_without_error_block() {
        let log_path = Path::new("/tmp/run/cmd-out/001.log");

        // Case 1: with error block well before tail
        let mut lines = Vec::new();
        for i in 1..=200 {
            if i == 10 {
                lines.push("error[E0308]: mismatched types".to_string());
            } else {
                lines.push(format!("line {i:03} padding padding padding padding padding padding padding padding"));
            }
        }
        let raw = lines.join("\n").into_bytes();
        assert!(raw.len() > DEFAULT_MAX_BYTES);
        let size_kb = raw.len().div_ceil(1024);
        let formatted = format_truncated(&raw, log_path);
        let expected_header = format!(
            "[输出 {size_kb} KB，全文在 /tmp/run/cmd-out/001.log；下面是首个错误起 40 行与末尾 40 行]\n"
        );
        assert!(formatted.starts_with(&expected_header));
        assert!(formatted.contains("error[E0308]: mismatched types"));
        assert!(formatted.contains("line 049")); // line 10..49 = 40 lines
        assert!(!formatted.contains("line 050"));
        assert!(formatted.contains("line 161")); // last 40 lines: 161..200
        assert!(formatted.contains("line 200"));

        // Case 2: without error block
        let mut ok_lines = Vec::new();
        for i in 1..=200 {
            ok_lines.push(format!("ok line {i:03} padding padding padding padding padding padding padding"));
        }
        let ok_raw = ok_lines.join("\n").into_bytes();
        let ok_kb = ok_raw.len().div_ceil(1024);
        let ok_fmt = format_truncated(&ok_raw, log_path);
        let ok_header = format!(
            "[输出 {ok_kb} KB，全文在 /tmp/run/cmd-out/001.log；下面是末尾 40 行]\n"
        );
        assert!(ok_fmt.starts_with(&ok_header));
        assert!(!ok_fmt.contains("ok line 001"));
        assert!(ok_fmt.contains("ok line 161"));
        assert!(ok_fmt.contains("ok line 200"));

        // Case 3: first error is inside the last 40 lines (err_idx > tail_start)
        // Must still include the full last 40 lines (line 161..200), not drop tail_start..err_idx!
        let mut late_lines = Vec::new();
        for i in 1..=200 {
            if i == 199 {
                late_lines.push("FAILED: assertion left == right".to_string());
            } else {
                late_lines.push(format!("late line {i:03} padding padding padding padding padding padding padding"));
            }
        }
        let late_raw = late_lines.join("\n").into_bytes();
        let late_kb = late_raw.len().div_ceil(1024);
        let late_fmt = format_truncated(&late_raw, log_path);
        let late_header = format!(
            "[输出 {late_kb} KB，全文在 /tmp/run/cmd-out/001.log；下面是首个错误起 40 行与末尾 40 行]\n"
        );
        assert!(late_fmt.starts_with(&late_header));
        assert!(!late_fmt.contains("late line 160"));
        assert!(late_fmt.contains("late line 161"));
        assert!(late_fmt.contains("late line 198"));
        assert!(late_fmt.contains("FAILED: assertion left == right"));
        assert!(late_fmt.contains("late line 200"));
    }

    #[test]
    fn cloexec_pipe_sets_fd_cloexec() {
        let (r, w) = cloexec_pipe().unwrap();
        let rf = unsafe { libc::fcntl(r.as_raw_fd(), libc::F_GETFD) };
        let wf = unsafe { libc::fcntl(w.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(rf & libc::FD_CLOEXEC, 0);
        assert_ne!(wf & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn execute_small_passthrough_and_large_spill_with_exit_code() {
        let dir = temp_dir("exec");
        let small_script = dir.join("small.sh");
        write(&small_script, "#!/bin/sh\necho 'hello stdout'\necho 'hello stderr' >&2\nexit 7\n").unwrap();
        fs::set_permissions(&small_script, fs::Permissions::from_mode(0o700)).unwrap();

        let cap = run_captured(small_script.to_str().unwrap(), &[]).unwrap();
        assert_eq!(cap.code, 7);
        assert_eq!(String::from_utf8_lossy(&cap.stdout).trim(), "hello stdout");
        assert_eq!(String::from_utf8_lossy(&cap.stderr).trim(), "hello stderr");
        let merged_text = String::from_utf8_lossy(&cap.merged);
        assert!(merged_text.contains("hello stdout"));
        assert!(merged_text.contains("hello stderr"));
        assert!(cap.merged.len() <= DEFAULT_MAX_BYTES);

        // Now test execute_argv_with_limit for small (no log file) vs large (spills to 001.log)
        let run_small = dir.join("run-small");
        fs::create_dir_all(&run_small).unwrap();
        let c1 = execute_argv_with_limit(&run_small, small_script.to_str().unwrap(), &[], DEFAULT_MAX_BYTES).unwrap();
        assert_eq!(c1, 7);
        assert!(!run_small.join("cmd-out").exists());
        assert_eq!(count_stats(&run_small), CmdOutRunStats::default());

        let large_script = dir.join("large.sh");
        write(
            &large_script,
            "#!/bin/sh\ni=1\nwhile [ \"$i\" -le 220 ]; do\n  if [ \"$i\" -eq 15 ]; then\n    echo 'thread main panicked at src/lib.rs:42' >&2\n  else\n    echo \"output line $i xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"\n  fi\n  i=$((i+1))\ndone\nexit 101\n",
        )
        .unwrap();
        fs::set_permissions(&large_script, fs::Permissions::from_mode(0o700)).unwrap();

        let run_large = dir.join("run-large");
        fs::create_dir_all(&run_large).unwrap();
        let c2 = execute_argv_with_limit(&run_large, large_script.to_str().unwrap(), &[], DEFAULT_MAX_BYTES).unwrap();
        assert_eq!(c2, 101);
        let log_file = run_large.join("cmd-out").join("001.log");
        assert!(log_file.is_file());
        let saved = fs::read_to_string(&log_file).unwrap();
        assert!(saved.len() > DEFAULT_MAX_BYTES);
        assert!(saved.contains("panicked at src/lib.rs:42"));
        let st = count_stats(&run_large);
        assert_eq!(st.triggers, 1);
        assert!(st.truncated_bytes > 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compile_test_command_detection_and_step_counting() {
        let cfg = default_commands();
        assert!(is_compile_test_command("cargo test --lib", &cfg));
        assert!(is_compile_test_command("RUST_LOG=debug cargo clippy -- -D warnings", &cfg));
        assert!(is_compile_test_command("cd web && pnpm test", &cfg));
        assert!(is_compile_test_command("python3 -m pytest tests/test_a.py", &cfg));
        assert!(is_compile_test_command("go test ./...", &cfg));
        assert!(!is_compile_test_command("git status", &cfg));
        assert!(!is_compile_test_command("rg -n foo src/", &cfg));

        let dir = temp_dir("steps");
        write(
            dir.join("events.jsonl"),
            concat!(
                "{\"e\":\"read\",\"path\":\"src/lib.rs\"}\n",
                "{\"e\":\"edit\",\"path\":\"src/lib.rs\"}\n",
                "{\"e\":\"bash\",\"cmd\":\"git status\"}\n",
                "{\"e\":\"bash\",\"cmd\":\"cargo test\"}\n",
                "{\"e\":\"bash\",\"cmd\":\"pnpm build\"}\n",
                "{\"e\":\"turn\",\"usage\":{\"input\":1000}}\n",
            ),
        )
        .unwrap();
        assert_eq!(count_author_steps(&dir, &cfg), (5, 2));
        let _ = fs::remove_dir_all(&dir);
    }
}
