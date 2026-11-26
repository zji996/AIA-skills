//! SSH execution is fail-closed: no transport failure can run a heavy command locally.
use crate::common::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicI32, Ordering},
    Arc,
};
use std::time::Duration;

fn relative(s: &str) -> bool {
    !s.is_empty()
        && !s.contains(['\0', '\n', '\r'])
        && Path::new(s)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}
fn env_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
}
pub fn validate(value: Option<&Value>, repo: bool) -> Res<()> {
    let Some(v) = value else { return Ok(()) };
    if v == &json!(false) {
        return Ok(());
    }
    let map = v.as_object().ok_or("remote must be an object or false")?;
    if repo && map.contains_key("host") {
        return Err("remote.host is forbidden in repository config; use user config".into());
    }
    for (k, v) in map {
        let valid = match k.as_str() {
            "host" => v.as_str().is_some_and(|s| {
                !s.is_empty() && !s.starts_with('-') && !s.contains(['\0', '\n', '\r', ' ', '\t'])
            }),
            "root" | "profile" => v.as_str().is_some_and(relative),
            "slice" => v.as_str().is_some_and(|s| {
                !s.is_empty()
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            }),
            "maxHeavy" => v.as_u64().is_some_and(|n| usize::try_from(n).is_ok()),
            "maxLight" => v.as_u64().is_some_and(|n| usize::try_from(n).is_ok()),
            "heavyJobs" => v
                .as_u64()
                .is_some_and(|n| n >= 1 && usize::try_from(n).is_ok()),
            "programs" => v.as_array().is_some_and(|a| {
                a.iter().all(|v| {
                    v.as_str().is_some_and(|s| {
                        relative(s)
                            && !s.contains('/')
                            && s != "."
                            && s != ".."
                            && s.bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                    })
                })
            }),
            "light" => v.as_array().is_some_and(|a| {
                a.iter().all(|entry| parse_light_entry(entry).is_some())
            }),
            "forwardEnv" => v
                .as_array()
                .is_some_and(|a| a.iter().all(|v| v.as_str().is_some_and(env_name))),
            _ => false,
        };
        if !valid {
            return Err(format!("invalid remote.{k}"));
        }
    }
    Ok(())
}
pub fn enabled(v: &Value) -> bool {
    std::env::var("DELEGATE_REMOTE").ok().as_deref() != Some("0")
        && v["host"].as_str().is_some_and(|h| !h.is_empty())
}
pub fn effective(v: &Value) -> Value {
    if !enabled(v) {
        return json!(false);
    }
    let mut v = v.clone();
    if v.get("maxHeavy").is_some() {
        v["maxHeavyExplicit"] = json!(true);
    }
    for (k, default) in [
        ("root", json!("delegate")),
        ("programs", json!(["cargo"])),
        ("maxHeavy", json!(4)),
        ("maxLight", json!(4)),
        ("light", json!([])),
        ("forwardEnv", json!([])),
    ] {
        if v.get(k).is_none() {
            v[k] = default;
        }
    }
    v
}

pub const DEFAULT_MAX_LIGHT: usize = 4;

pub fn max_light(v: &Value) -> usize {
    v.get("maxLight")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(DEFAULT_MAX_LIGHT)
}

fn split_pattern_tokens(s: &str) -> Option<Vec<String>> {
    if s.contains(['\0', '\n', '\r']) {
        return None;
    }
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            Some('"') => {
                if c == '"' {
                    quote = None;
                } else if c == '\\' {
                    let next = chars.next()?;
                    cur.push(next);
                } else {
                    cur.push(c);
                }
            }
            Some(_) => unreachable!(),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_token = true;
                }
                '\\' => {
                    let next = chars.next()?;
                    cur.push(next);
                    in_token = true;
                }
                c if c.is_ascii_whitespace() => {
                    if in_token {
                        tokens.push(std::mem::take(&mut cur));
                        in_token = false;
                    }
                }
                _ => {
                    cur.push(c);
                    in_token = true;
                }
            },
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_token {
        tokens.push(cur);
    }
    if tokens.is_empty() || tokens[0].is_empty() {
        return None;
    }
    Some(tokens)
}

fn parse_light_entry(entry: &Value) -> Option<Vec<String>> {
    if let Some(s) = entry.as_str() {
        return split_pattern_tokens(s);
    }
    let arr = entry.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let mut tokens = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item.as_str()?;
        if s.is_empty() || s.contains(['\0', '\n', '\r']) {
            return None;
        }
        tokens.push(s.to_string());
    }
    Some(tokens)
}

pub fn light_patterns(v: &Value) -> Vec<Vec<String>> {
    if !enabled(v) {
        return vec![];
    }
    v["light"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_light_entry)
        .collect()
}

/// Iterative glob match on a single argv token (`*` matches 0+ chars, `?` matches 1 char).
fn token_glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn matches_light_argv(patterns: &[Vec<String>], argv: &[&str]) -> bool {
    patterns.iter().any(|pat| {
        argv.len() >= pat.len()
            && pat
                .iter()
                .zip(argv.iter())
                .all(|(p, a)| token_glob_match(p, a))
    })
}

pub fn is_light(remote: &Value, program: &str, args: &[String]) -> bool {
    let patterns = light_patterns(remote);
    if patterns.is_empty() {
        return false;
    }
    let mut argv = Vec::with_capacity(1 + args.len());
    argv.push(program);
    argv.extend(args.iter().map(String::as_str));
    matches_light_argv(&patterns, &argv)
}

fn is_env_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    env_name(name)
}

fn is_redirection_token(word: &str) -> bool {
    let trimmed = word.trim_start_matches(|c: char| c.is_ascii_digit());
    trimmed.starts_with('>') || trimmed.starts_with('<')
}

fn is_standalone_redirection_op(word: &str) -> bool {
    let trimmed = word.trim_start_matches(|c: char| c.is_ascii_digit());
    matches!(trimmed, ">" | ">>" | "<" | "<<" | "<>" | ">|" | "&>" | "&>>")
}

/// Split a shell command string into simple command token lists separated by `&&`, `||`, `;`, `|`, `\n`.
/// Returns `None` on unquoted subshells/command substitution/backgrounding/unclosed quotes (fail-closed to heavy).
fn split_shell_segments(cmd: &str) -> Option<Vec<Vec<String>>> {
    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut cur_seg: Vec<String> = Vec::new();
    let mut cur_tok = String::new();
    let mut in_tok = false;
    let mut quote: Option<char> = None;
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;

    let flush_tok = |in_tok: &mut bool, cur_tok: &mut String, cur_seg: &mut Vec<String>| {
        if *in_tok {
            cur_seg.push(std::mem::take(cur_tok));
            *in_tok = false;
        }
    };
    let flush_seg = |cur_seg: &mut Vec<String>, segments: &mut Vec<Vec<String>>| {
        if !cur_seg.is_empty() {
            segments.push(std::mem::take(cur_seg));
        }
    };

    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                } else {
                    cur_tok.push(c);
                }
                i += 1;
            }
            Some('"') => {
                if c == '"' {
                    quote = None;
                    i += 1;
                } else if c == '\\' {
                    if i + 1 >= chars.len() {
                        return None;
                    }
                    cur_tok.push(chars[i + 1]);
                    i += 2;
                } else if c == '`' || (c == '$' && chars.get(i + 1) == Some(&'(')) {
                    return None;
                } else {
                    cur_tok.push(c);
                    i += 1;
                }
            }
            Some(_) => unreachable!(),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_tok = true;
                    i += 1;
                }
                '\\' => {
                    if i + 1 >= chars.len() {
                        return None;
                    }
                    cur_tok.push(chars[i + 1]);
                    in_tok = true;
                    i += 2;
                }
                '`' | '(' | ')' | '{' | '}' => return None,
                '$' if chars.get(i + 1) == Some(&'(') => return None,
                ';' | '\n' => {
                    flush_tok(&mut in_tok, &mut cur_tok, &mut cur_seg);
                    flush_seg(&mut cur_seg, &mut segments);
                    i += 1;
                }
                '&' => {
                    if chars.get(i + 1) == Some(&'&') {
                        flush_tok(&mut in_tok, &mut cur_tok, &mut cur_seg);
                        flush_seg(&mut cur_seg, &mut segments);
                        i += 2;
                    } else if cur_tok.ends_with('>') || cur_tok.ends_with('<') {
                        // Part of redirection like `2>&1` or `>&2`.
                        cur_tok.push('&');
                        in_tok = true;
                        i += 1;
                    } else if !in_tok && chars.get(i + 1) == Some(&'>') {
                        // `&>` or `&>>` redirection.
                        cur_tok.push('&');
                        in_tok = true;
                        i += 1;
                    } else {
                        // Background operator `&`: fail-closed to heavy.
                        return None;
                    }
                }
                '|' => {
                    if chars.get(i + 1) == Some(&'|') {
                        flush_tok(&mut in_tok, &mut cur_tok, &mut cur_seg);
                        flush_seg(&mut cur_seg, &mut segments);
                        i += 2;
                    } else {
                        flush_tok(&mut in_tok, &mut cur_tok, &mut cur_seg);
                        flush_seg(&mut cur_seg, &mut segments);
                        i += 1;
                    }
                }
                c if c.is_ascii_whitespace() => {
                    flush_tok(&mut in_tok, &mut cur_tok, &mut cur_seg);
                    i += 1;
                }
                _ => {
                    cur_tok.push(c);
                    in_tok = true;
                    i += 1;
                }
            },
        }
    }
    if quote.is_some() {
        return None;
    }
    flush_tok(&mut in_tok, &mut cur_tok, &mut cur_seg);
    flush_seg(&mut cur_seg, &mut segments);
    Some(segments)
}

fn strip_redirections_and_env(seg: &[String]) -> Vec<&str> {
    let mut clean = Vec::new();
    let mut skip_next = false;
    for tok in seg {
        if skip_next {
            skip_next = false;
            continue;
        }
        if is_standalone_redirection_op(tok) {
            skip_next = true;
            continue;
        }
        if is_redirection_token(tok) {
            continue;
        }
        clean.push(tok.as_str());
    }
    let mut idx = 0;
    while idx < clean.len() && is_env_assignment(clean[idx]) {
        idx += 1;
    }
    if clean.get(idx) == Some(&"env") {
        idx += 1;
        while idx < clean.len() && is_env_assignment(clean[idx]) {
            idx += 1;
        }
    }
    clean[idx..].to_vec()
}

const BENIGN_SHELL_BUILTINS: &[&str] = &[
    "cd", "export", "unset", "set", "true", "false", "echo", "printf", "test", "[", "pwd",
    "mkdir", "rm", "cp", "mv", "touch", "cat", "head", "tail", "grep", "rg", "wc", "ls", "sleep",
];

/// Classify whether a shell step command (e.g. acceptance / verify / evidence) belongs on the
/// remote light queue. Returns `false` whenever `remote.light` is unset/empty or any command
/// segment cannot be proven light (fail-closed to heavy).
pub fn shell_command_is_light(remote: &Value, cmd: &str) -> bool {
    let patterns = light_patterns(remote);
    if patterns.is_empty() {
        return false;
    }
    let Some(segments) = split_shell_segments(cmd) else {
        return false;
    };
    if segments.is_empty() {
        return false;
    }
    let mut matched_any = false;
    for seg in &segments {
        let words = strip_redirections_and_env(seg);
        if words.is_empty() {
            continue;
        }
        if matches_light_argv(&patterns, &words) {
            matched_any = true;
        } else if BENIGN_SHELL_BUILTINS.contains(&words[0]) {
            continue;
        } else {
            return false;
        }
    }
    matched_any
}

pub fn shell_commands_are_light(remote: &Value, cmds: &[&str]) -> bool {
    !cmds.is_empty() && cmds.iter().all(|cmd| shell_command_is_light(remote, cmd))
}

const REMOTE_THREADS_PROBE_SH: &str = r#"n=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 0)
cg=""
if [ -r /proc/self/cgroup ]; then
  rel=$(sed -n 's/^0:://p' /proc/self/cgroup | head -n 1)
  p="/sys/fs/cgroup${rel}"
  while [ -n "$p" ]; do
    if [ -r "$p/cpu.max" ]; then
      read q per < "$p/cpu.max"
      if [ "$q" != "max" ] && [ -n "$q" ] && [ -n "$per" ] && [ "$per" -gt 0 ] 2>/dev/null; then
        lim=$(( (q + per - 1) / per ))
        [ "$lim" -lt 1 ] && lim=1
        if [ -z "$cg" ] || [ "$lim" -lt "$cg" ]; then cg="$lim"; fi
      fi
    fi
    [ "$p" = "/sys/fs/cgroup" ] && break
    p=$(dirname "$p")
  done
fi
if [ -n "$cg" ] && [ "$n" -gt 0 ] 2>/dev/null; then
  [ "$cg" -lt "$n" ] && echo "$cg" || echo "$n"
elif [ -n "$cg" ]; then
  echo "$cg"
else
  echo "$n"
fi"#;

fn remote_threads_cache_path(host: &str, slice: &str) -> PathBuf {
    let key = sha1_smol::Sha1::from(format!("{host}:{slice}"))
        .digest()
        .to_string();
    state_dir().join("remote-threads").join(format!("{key}.json"))
}

fn read_cached_remote_threads(host: &str, slice: &str) -> Option<usize> {
    let doc = json(remote_threads_cache_path(host, slice));
    doc.get("threads")
        .and_then(Value::as_u64)
        .filter(|&n| n > 0)
        .map(|n| n as usize)
}

fn write_cached_remote_threads(host: &str, slice: &str, threads: usize) {
    let path = remote_threads_cache_path(host, slice);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = write_json(
        &path,
        &json!({
            "host": host,
            "slice": slice,
            "threads": threads,
            "probedAt": iso(),
        }),
    );
}

fn probe_remote_threads_ssh(host: &str, slice: &str) -> Res<usize> {
    let script = if !slice.is_empty() {
        format!(
            "if command -v systemd-run >/dev/null 2>&1; then systemd-run --user --slice={} --wait --pipe --collect --quiet sh -c {} 2>/dev/null || sh -c {}; else sh -c {}; fi",
            shell_quote(slice),
            shell_quote(REMOTE_THREADS_PROBE_SH),
            shell_quote(REMOTE_THREADS_PROBE_SH),
            shell_quote(REMOTE_THREADS_PROBE_SH),
        )
    } else {
        REMOTE_THREADS_PROBE_SH.to_string()
    };
    let raw = capture(ssh(host, &script), 15.0, false)?;
    let parsed = raw
        .lines()
        .rev()
        .find_map(|ln| ln.trim().parse::<usize>().ok().filter(|&n| n > 0))
        .ok_or_else(|| format!("invalid remote thread count output: {:?}", clip(raw.trim(), 80)))?;
    Ok(parsed)
}

/// Derive concurrency plan for a remote host using its own thread count (probed once and cached).
/// When `allow_ssh` is false (e.g. non-blocking `delegate lane` / TUI polling), uses cached threads
/// if present and falls back to configured `remote.maxHeavy` without running SSH.
/// On SSH probe failure, prints a diagnostic and falls back to configured `remote.maxHeavy`.
pub fn remote_plan(remote: &Value, allow_ssh: bool) -> crate::lane::ConcurrencyPlan {
    let host = s(remote, "host");
    let slice = s(remote, "slice");
    let explicit_mh = if b(remote, "maxHeavyExplicit") {
        remote.get("maxHeavy").and_then(Value::as_u64).map(|n| n as usize)
    } else {
        None
    };
    let fallback_mh = remote.get("maxHeavy").and_then(Value::as_u64).unwrap_or(4) as usize;
    let explicit_hj = remote
        .get("heavyJobs")
        .and_then(Value::as_u64)
        .filter(|&n| n >= 1)
        .map(|n| n as usize);

    if let Some(threads) = read_cached_remote_threads(host, slice) {
        return crate::lane::derive_remote_concurrency(threads, explicit_mh, explicit_hj);
    }

    if allow_ssh && !host.is_empty() {
        match probe_remote_threads_ssh(host, slice) {
            Ok(threads) => {
                write_cached_remote_threads(host, slice, threads);
                return crate::lane::derive_remote_concurrency(threads, explicit_mh, explicit_hj);
            }
            Err(err) => {
                let msg = format!(
                    "无法探测远端 {host} 线程数（{}），回退到配置值 maxHeavy={fallback_mh}",
                    err.lines().next().unwrap_or(&err)
                );
                eprintln!("delegate remote: {msg}");
                return crate::lane::ConcurrencyPlan {
                    threads: 0,
                    budget: 0,
                    max_heavy: fallback_mh,
                    pool_tokens: 0,
                    heavy_jobs: explicit_hj.unwrap_or(1),
                    max_heavy_explicit: explicit_mh.is_some(),
                    heavy_jobs_explicit: explicit_hj.is_some(),
                    remote: true,
                    fallback: true,
                    diagnostic: Some(msg),
                };
            }
        }
    }

    crate::lane::ConcurrencyPlan {
        threads: 0,
        budget: 0,
        max_heavy: fallback_mh,
        pool_tokens: 0,
        heavy_jobs: explicit_hj.unwrap_or(1),
        max_heavy_explicit: explicit_mh.is_some(),
        heavy_jobs_explicit: explicit_hj.is_some(),
        remote: true,
        fallback: true,
        diagnostic: None,
    }
}
pub fn programs(v: &Value) -> Vec<&str> {
    if !enabled(v) {
        return vec![];
    }
    v["programs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}
fn ssh_cmd(host: &str, script: &str, no_stdin: bool) -> Command {
    let mut c = Command::new("ssh");
    if no_stdin {
        c.arg("-n");
    }
    c.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        host,
        script,
    ]);
    c
}
fn ssh(host: &str, script: &str) -> Command {
    ssh_cmd(host, script, false)
}
fn stdin_is_data_source() -> bool {
    fd_is_data_source(libc::STDIN_FILENO)
}
fn fd_is_data_source(fd: libc::c_int) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return false;
    }
    if unsafe { libc::isatty(fd) } != 0 {
        return false;
    }
    match st.st_mode & libc::S_IFMT {
        libc::S_IFREG => true,
        libc::S_IFIFO => fifo_has_data_source(fd),
        _ => false,
    }
}
fn fifo_has_data_source(fd: libc::c_int) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = loop {
        let r = unsafe { libc::poll(&mut pfd, 1, 0) };
        if r < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        break r;
    };
    if rc < 0 {
        return false;
    }
    // Unread bytes already buffered (e.g. `echo x | cmd`, `cat f | cmd`, short heredocs).
    if (pfd.revents & libc::POLLIN) != 0 {
        return true;
    }
    // Empty pipe whose write end is already closed (e.g. Pi/rpi bash tool) or error:
    // immediate EOF, identical to /dev/null.
    if (pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)) != 0 {
        return false;
    }
    // 0 bytes buffered and write end is still open: distinguish a slow sibling pipeline
    // producer from an unclosed idle harness pipe held by an ancestor. Fail closed when unsure.
    fifo_has_sibling_writer(fd)
}
#[cfg(target_os = "linux")]
fn parse_proc_stat_ppid_pgrp(pid: u32) -> Option<(u32, i32)> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = raw.rsplit_once(')')?.1;
    let mut fields = after.split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?.parse::<u32>().ok()?;
    let pgrp = fields.next()?.parse::<i32>().ok()?;
    Some((ppid, pgrp))
}
#[cfg(target_os = "linux")]
fn pid_holds_pipe_writer(pid: u32, target: &std::ffi::OsStr) -> bool {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(link) = fs::read_link(entry.path()) else {
            continue;
        };
        if link.as_os_str() != target {
            continue;
        }
        let Some(fd_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(info) = fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd_name}")) else {
            continue;
        };
        for line in info.lines() {
            if let Some(rest) = line.strip_prefix("flags:") {
                if let Ok(flags) = u32::from_str_radix(rest.trim(), 8) {
                    if (flags & (libc::O_ACCMODE as u32)) != (libc::O_RDONLY as u32) {
                        return true;
                    }
                }
                break;
            }
        }
    }
    false
}
#[cfg(target_os = "linux")]
fn fifo_has_sibling_writer(fd: libc::c_int) -> bool {
    let Ok(target) = fs::read_link(format!("/proc/self/fd/{fd}")) else {
        return false;
    };
    let my_pid = std::process::id();
    let my_pgrp = unsafe { libc::getpgrp() };
    if my_pgrp <= 0 || pid_holds_pipe_writer(my_pid, target.as_os_str()) {
        return false;
    }
    let mut ancestors = std::collections::HashSet::new();
    let mut cur = parse_proc_stat_ppid_pgrp(my_pid)
        .map(|(ppid, _)| ppid)
        .unwrap_or(0);
    while cur > 1 && ancestors.insert(cur) {
        if pid_holds_pipe_writer(cur, target.as_os_str()) {
            return false;
        }
        cur = parse_proc_stat_ppid_pgrp(cur)
            .map(|(ppid, _)| ppid)
            .unwrap_or(0);
    }
    let Ok(proc_entries) = fs::read_dir("/proc") else {
        return false;
    };
    for entry in proc_entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if pid == my_pid || ancestors.contains(&pid) {
            continue;
        }
        let Some((_, pgrp)) = parse_proc_stat_ppid_pgrp(pid) else {
            continue;
        };
        if pgrp != my_pgrp {
            continue;
        }
        if pid_holds_pipe_writer(pid, target.as_os_str()) {
            return true;
        }
    }
    false
}
#[cfg(not(target_os = "linux"))]
fn fifo_has_sibling_writer(_fd: libc::c_int) -> bool {
    false
}
fn output(c: Command) -> Res<String> {
    capture(c, 30.0, false)
}
fn capture(mut c: Command, timeout: f64, cancellable: bool) -> Res<String> {
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    group(&mut c);
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break Ok(status);
        }
        if start.elapsed().as_secs_f64() > timeout || (cancellable && crate::lane::stopped()) {
            killpg(child.id() as i32, libc::SIGKILL);
            let _ = child.wait();
            break Err("transport timed out or cancelled".to_string());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();
    if !status?.success() {
        return Err(if err.trim().is_empty() {
            "transport failed".into()
        } else {
            err.trim().into()
        });
    }
    Ok(out.trim().into())
}
fn destination(meta: &Value, top: &Path) -> Res<String> {
    let name = top
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("remote: worktree has no directory name")?;
    let key = crate::launch::repo_key_for_meta(meta);
    let hash = sha1_smol::Sha1::from(key).digest().to_string();
    Ok(format!(
        "{}/{}/{}",
        s(&meta["remote"], "root"),
        &hash[..12],
        name
    ))
}
fn top_for(meta: &Value, cwd: &Path) -> Res<PathBuf> {
    // cwd may be the merged source during verify, not the colleague's worktree.
    git_top(cwd)
        .or_else(|| {
            let top = PathBuf::from(s(meta, "top"));
            cwd.starts_with(&top).then_some(top)
        })
        .ok_or("remote: cwd must be inside a worktree".into())
}
fn copy_max_bytes() -> u64 {
    setting("REMOTE_COPY_MAX_BYTES", "52428800")
        .parse::<u64>()
        .unwrap_or(52_428_800)
}
fn managed_lists(meta: &Value, top: &Path) -> (Vec<String>, Vec<String>) {
    let copies = strings(&meta["worktree"]["config"]["copy"]);
    let mut links = strings(&meta["worktree"]["config"]["link"]);
    let source = Path::new(s(&meta["worktree"], "source"));
    if !source.as_os_str().is_empty() {
        for item in strings(&meta["share"]) {
            if let Ok(rel) = Path::new(&item).strip_prefix(source) {
                let rel = rel.to_string_lossy().trim_end_matches('/').to_string();
                if !rel.is_empty() && !copies.contains(&rel) && !links.contains(&rel) {
                    links.push(rel);
                }
            }
        }
    }
    let _ = top;
    (copies, links)
}
fn bounded_size(path: &Path, limit: u64, skip_build: bool) -> Option<u64> {
    fn walk(path: &Path, limit: u64, skip_build: bool, total: &mut u64) -> bool {
        let Ok(info) = fs::symlink_metadata(path) else {
            return true;
        };
        if info.file_type().is_symlink() {
            return true;
        }
        if info.is_file() {
            *total = total.saturating_add(info.len());
            return *total <= limit;
        }
        if info.is_dir() {
            let Ok(entries) = fs::read_dir(path) else {
                return false;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                if skip_build
                    && matches!(
                        name.to_str(),
                        Some(".git" | "target" | "node_modules")
                    )
                {
                    continue;
                }
                if !walk(&entry.path(), limit, skip_build, total) {
                    return false;
                }
            }
        }
        true
    }
    let mut total = 0u64;
    walk(path, limit, skip_build, &mut total).then_some(total)
}
fn sync_path(host: &str, src: &Path, remote_dst: &str, skip_build: bool) -> Res<()> {
    let mut c = Command::new("rsync");
    c.args(["-a", "--protect-args"]);
    if src.is_dir() {
        c.arg("--delete");
        if skip_build {
            c.args([
                "--exclude=/.git",
                "--exclude=/target",
                "--exclude=/node_modules",
            ]);
        }
        c.args(["-e", "ssh -o BatchMode=yes -o ConnectTimeout=10"])
            .arg(format!("{}/", src.display()))
            .arg(format!("{host}:{remote_dst}/"));
    } else {
        c.args(["-e", "ssh -o BatchMode=yes -o ConnectTimeout=10"])
            .arg(src)
            .arg(format!("{host}:{remote_dst}"));
    }
    capture(c, 1800.0, true).map(|_| ())
}
/// Materialize `worktree.copy` and `worktree.link` (plus in-repo `--share`) on remote
/// so remote acceptance sees the same workspace paths as the local worktree.
/// Returns `(path, reason)` entries that could not be materialized on remote.
fn sync_managed(
    host: &str,
    top: &Path,
    remote: &str,
    meta: &Value,
) -> Res<Vec<(String, String)>> {
    let (copies, links) = managed_lists(meta, top);
    if copies.is_empty() && links.is_empty() {
        return Ok(vec![]);
    }
    let limit = copy_max_bytes();
    let mut missing = Vec::new();
    let mut to_rsync: Vec<(String, PathBuf, bool)> = Vec::new();

    // 1. Probe and symlink worktree.link targets in one SSH round-trip when the remote
    //    host already has the same path; otherwise fall back to syncing if small.
    let mut link_entries: Vec<(String, PathBuf)> = Vec::new();
    let source = Path::new(s(&meta["worktree"], "source"));
    for item in &links {
        let local_path = top.join(item);
        if !local_path.exists() && !local_path.is_symlink() {
            continue;
        }
        let origin = if local_path.is_symlink() {
            fs::read_link(&local_path)
                .map(|target| {
                    if target.is_absolute() {
                        target
                    } else {
                        local_path.parent().unwrap_or(top).join(target)
                    }
                })
                .unwrap_or_else(|_| source.join(item))
        } else {
            local_path.clone()
        };
        if !origin.exists() {
            continue;
        }
        link_entries.push((item.clone(), origin));
    }
    if !link_entries.is_empty() {
        let mut script = String::new();
        for (idx, (item, origin)) in link_entries.iter().enumerate() {
            let dst = format!("{remote}/{item}");
            let parent = Path::new(&dst)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| remote.to_string());
            let q_origin = shell_quote(&origin.to_string_lossy());
            let q_dst = shell_quote(&dst);
            let q_parent = shell_quote(&parent);
            script.push_str(&format!(
                "if [ -e {q_origin} ]; then mkdir -p {q_parent} && ( [ \"$(readlink {q_dst} 2>/dev/null)\" = {q_origin} ] || ( rm -rf -- {q_dst} && ln -s {q_origin} {q_dst} ) ) && printf 'LINKED:%d\\n' {idx}; else printf 'MISSING:%d\\n' {idx}; fi\n"
            ));
        }
        let probe_out = output(ssh(host, &script))?;
        for line in probe_out.lines() {
            if let Some(rest) = line.strip_prefix("MISSING:") {
                if let Ok(idx) = rest.trim().parse::<usize>() {
                    if let Some((item, origin)) = link_entries.get(idx) {
                        if bounded_size(origin, limit, true).is_some() {
                            to_rsync.push((item.clone(), origin.clone(), true));
                        } else {
                            missing.push((
                                item.clone(),
                                format!(
                                    "worktree.link not found on remote host and exceeds {} MiB sync limit",
                                    limit / (1024 * 1024)
                                ),
                            ));
                        }
                    }
                }
            }
        }
    }

    // 2. Check worktree.copy paths: sync directly if within the size limit.
    for item in &copies {
        let local_path = top.join(item);
        if !local_path.exists() && !local_path.is_symlink() {
            continue;
        }
        if bounded_size(&local_path, limit, false).is_some() {
            to_rsync.push((item.clone(), local_path, false));
        } else {
            missing.push((
                item.clone(),
                format!(
                    "worktree.copy exceeds {} MiB remote sync limit",
                    limit / (1024 * 1024)
                ),
            ));
        }
    }

    // 3. Prepare remote parent dirs and rsync all small copy/link items.
    if !to_rsync.is_empty() {
        let mut prep = String::new();
        for (item, src, _) in &to_rsync {
            let dst = format!("{remote}/{item}");
            let parent = Path::new(&dst)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| remote.to_string());
            let q_dst = shell_quote(&dst);
            let q_parent = shell_quote(&parent);
            if src.is_dir() {
                prep.push_str(&format!(
                    "if [ -L {q_dst} ] || [ -f {q_dst} ]; then rm -f -- {q_dst}; fi; mkdir -p {q_dst}\n"
                ));
            } else {
                prep.push_str(&format!(
                    "mkdir -p {q_parent} && if [ -L {q_dst} ] || [ -d {q_dst} ]; then rm -rf -- {q_dst}; fi\n"
                ));
            }
        }
        output(ssh(host, &prep))?;
        for (item, src, skip_build) in &to_rsync {
            let dst = format!("{remote}/{item}");
            sync_path(host, src, &dst, *skip_build)?;
        }
    }
    Ok(missing)
}
fn sync(host: &str, local: &Path, remote: &str, down: bool, excludes: &[String]) -> Res<()> {
    let local = format!("{}/", local.display());
    let remote = format!("{host}:{remote}/");
    let mut c = Command::new("rsync");
    c.args([
        "-a",
        "--delete",
        "--protect-args",
        "--filter=:- .gitignore",
        "--exclude=/.git",
        "--exclude=/target",
        "--exclude=/.delegate-*.pid*",
    ]);
    for item in excludes {
        c.arg(format!("--exclude=/{item}"));
    }
    c.args(["-e", "ssh -o BatchMode=yes -o ConnectTimeout=10"]);
    if down {
        c.arg(remote).arg(local);
    } else {
        c.arg(local).arg(remote);
    }
    capture(c, 1800.0, true).map(|_| ())
}
fn environment(meta: &Value) -> BTreeMap<String, String> {
    environment_for(meta, false)
}
fn environment_for(meta: &Value, light: bool) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    for (k, v) in meta["env"].as_object().into_iter().flatten() {
        if let Some(v) = v.as_str() {
            vars.insert(k.clone(), v.into());
        }
    }
    for k in meta["remote"]["forwardEnv"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if let Ok(v) = std::env::var(k) {
            vars.insert(k.into(), v);
        }
    }
    // Never transplant the local machine's process/shim identity, search paths, or CARGO_BUILD_JOBS.
    vars.retain(|k, _| {
        ![
            "PATH",
            "HOME",
            "PWD",
            "OLDPWD",
            "SHELL",
            "TMPDIR",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
            "CARGO_BUILD_JOBS",
        ]
        .contains(&k.as_str())
            && !k.starts_with("DELEGATE_")
    });
    let plan = remote_plan(&meta["remote"], true);
    let jobs = if light && !plan.heavy_jobs_explicit && plan.budget > 0 {
        plan.budget
            .checked_div(max_light(&meta["remote"]))
            .map(|j| j.max(1))
            .unwrap_or(plan.heavy_jobs)
    } else {
        plan.heavy_jobs
    };
    vars.insert("CARGO_BUILD_JOBS".into(), jobs.to_string());
    vars
}
/// Sanitized display alias for a configured remote host (strips any `user@` prefix).
pub fn host_alias(host: &str) -> String {
    let trimmed = host.trim();
    match trimmed.rsplit_once('@') {
        Some((_, alias)) if !alias.trim().is_empty() => alias.trim().to_string(),
        _ => trimmed.to_string(),
    }
}

const REMOTE_LOG_MAX_BYTES: usize = 256 * 1024;
const REMOTE_LOG_KEEP_BYTES: usize = 32 * 1024;

struct TailLogWriter {
    path: PathBuf,
    file: Option<fs::File>,
    written: usize,
    ring: Vec<u8>,
}

impl TailLogWriter {
    fn create(path: PathBuf) -> Self {
        let file = fs::File::create(&path).ok();
        Self {
            path,
            file,
            written: 0,
            ring: Vec::new(),
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.ring.extend_from_slice(bytes);
        if self.ring.len() > REMOTE_LOG_KEEP_BYTES {
            let drain = self.ring.len() - REMOTE_LOG_KEEP_BYTES;
            self.ring.drain(..drain);
        }
        if let Some(ref mut f) = self.file {
            if f.write_all(bytes).is_ok() {
                self.written += bytes.len();
                if self.written > REMOTE_LOG_MAX_BYTES {
                    let tmp_path = self.path.with_file_name(format!(
                        ".{}.tmp.{}",
                        self.path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("remote-output.log"),
                        std::process::id()
                    ));
                    if let Ok(mut tmp) = fs::File::create(&tmp_path) {
                        if tmp.write_all(&self.ring).is_ok()
                            && tmp.flush().is_ok()
                            && fs::rename(&tmp_path, &self.path).is_ok()
                        {
                            self.written = self.ring.len();
                            self.file = Some(tmp);
                        } else {
                            let _ = fs::remove_file(&tmp_path);
                        }
                    }
                }
            }
        }
    }
}

/// Lock-free read of the trailing lines of `<run>/remote-output.log` for status/TUI display.
pub fn read_output_tail(run: &Path, max_lines: usize, max_bytes: usize) -> Vec<String> {
    if max_lines == 0 || max_bytes == 0 {
        return Vec::new();
    }
    let path = run.join("remote-output.log");
    let Ok(mut f) = fs::File::open(&path) else {
        return Vec::new();
    };
    let Ok(meta) = f.metadata() else {
        return Vec::new();
    };
    let len = meta.len();
    if len == 0 {
        return Vec::new();
    }
    use std::io::Seek;
    let read_len = len.min(max_bytes as u64);
    if len > read_len && f.seek(std::io::SeekFrom::End(-(read_len as i64))).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::with_capacity(read_len as usize);
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.len() > max_lines {
        lines.drain(..lines.len() - max_lines);
    }
    lines
}

struct ActiveRemoteGuard {
    path: PathBuf,
}

impl ActiveRemoteGuard {
    fn start(run: &Path, host: &str, queue: &str, cmd: &str, log_path: &Path) -> Self {
        let path = run.join("remote-active.json");
        let now_ep = epoch() as i64;
        let since = format_time(false, "%Y-%m-%dT%H:%M:%SZ");
        let _ = write_json(
            &path,
            &json!({
                "host": host_alias(host),
                "queue": queue,
                "cmd": cmd,
                "pid": std::process::id(),
                "startedEpoch": now_ep,
                "since": since,
                "log": log_path,
            }),
        );
        Self { path }
    }
}

impl Drop for ActiveRemoteGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn relay(
    reader: impl Read,
    stderr: bool,
    remote: String,
    local: String,
    marker: String,
    exit: Arc<AtomicI32>,
    log: Option<Arc<std::sync::Mutex<TailLogWriter>>>,
) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf).unwrap_or(0) == 0 {
            break;
        }
        if stderr {
            if let Some(pos) = buf
                .windows(marker.len())
                .rposition(|s| s == marker.as_bytes())
            {
                if let Ok(code) = std::str::from_utf8(&buf[pos + marker.len()..])
                    .unwrap_or("")
                    .trim()
                    .parse::<i32>()
                {
                    exit.store(code, Ordering::SeqCst);
                    buf.truncate(pos);
                }
            }
        }
        // Work on bytes so non-UTF8 diagnostics and unterminated last lines survive.
        let mut text = Vec::with_capacity(buf.len());
        let mut rest = buf.as_slice();
        while let Some(pos) = rest
            .windows(remote.len())
            .position(|s| s == remote.as_bytes())
        {
            text.extend_from_slice(&rest[..pos]);
            text.extend_from_slice(local.as_bytes());
            rest = &rest[pos + remote.len()..];
        }
        text.extend_from_slice(rest);
        if let Some(ref sink) = log {
            if let Ok(mut guard) = sink.lock() {
                guard.append(&text);
            }
        }
        if stderr {
            let _ = std::io::stderr().write_all(&text);
        } else {
            let _ = std::io::stdout().write_all(&text);
        }
    }
}
fn stop(host: &str, unit: &str, pidfile: &str, slice: bool) {
    let mut script = format!("touch {cancel}; if test -f {p}; then p=$(cat {p}); /bin/kill -TERM -- -\"$p\" 2>/dev/null; sleep 1; /bin/kill -KILL -- -\"$p\" 2>/dev/null; fi; :", p=shell_quote(pidfile), cancel=shell_quote(&format!("{pidfile}.cancel")));
    if slice {
        script.push_str(&format!("; systemctl --user stop {}", shell_quote(unit)));
    }
    let _ = capture(ssh(host, &script), 4.0, false);
}
fn shell_status(stage: &str, executed: bool) {
    if let Some(path) = std::env::var_os("DELEGATE_SHELL_STATUS") {
        let _ = write_json(PathBuf::from(path), &json!({"stage":stage,"executed":executed}));
    }
}
fn shell_status_missing(paths: &[String], reason: &str) {
    if let Some(path) = std::env::var_os("DELEGATE_SHELL_STATUS") {
        let _ = write_json(
            PathBuf::from(path),
            &json!({"stage":"missing_managed","executed":false,"paths":paths,"reason":reason}),
        );
    }
}
fn execute(run: &Path, meta: &Value, program: &str, args: &[String]) -> Res<i32> {
    shell_status("queued", false);
    let remote = &meta["remote"];
    let host = s(remote, "host");
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let top = top_for(meta, &cwd)?;
    let relative_cwd = cwd.strip_prefix(&top).map_err(|e| e.to_string())?;
    let dest = destination(meta, &top)?;
    crate::lane::install_signals();
    let light = is_light(remote, program, args);
    let cmd_preview = if args.is_empty() {
        program.to_string()
    } else {
        clip(&format!("{program} {}", args.join(" ")), 60)
    };
    let label = format!("{}: {cmd_preview}", top.display());
    let mut shown = false;
    let _lane = if light {
        crate::lane::acquire_remote_light(&label, Some(run), remote, |ahead, _| {
            if !shown {
                eprintln!("delegate remote light: queued behind {ahead}");
                shown = true;
            }
        })?
    } else {
        crate::lane::acquire_remote(&label, Some(run), remote, |ahead, _| {
            if !shown {
                eprintln!("delegate remote: queued behind {ahead}");
                shown = true;
            }
        })?
    };
    // Serialise upload/execute/download as one transaction for a warm slot.
    let locks = state_dir().join("lane-remote/slots");
    fs::create_dir_all(&locks).map_err(|e| e.to_string())?;
    let lock_name = sha1_smol::Sha1::from(format!("{host}:{dest}"))
        .digest()
        .to_string();
    let wait_started = std::time::Instant::now();
    let marker = run.join(format!("lane-waiting-{}", std::process::id()));
    let mut waiting = None;
    let _slot = loop {
        match crate::locks::try_acquire(
            crate::locks::LockKind::Lane,
            &locks.join(lock_name.as_str()),
            true,
        ) {
            Ok(f) => break f,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && !crate::lane::stopped() => {
                if waiting.is_none() {
                    waiting = Some(locked_file(&marker, &json!({"epoch":epoch()}).to_string())?);
                    eprintln!("delegate remote: queued for slot {dest}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    if waiting.is_some() {
        let _ = append(
            run.join("lane-wait"),
            &format!("{:.3}\n", wait_started.elapsed().as_secs_f64()),
        );
        let _ = fs::remove_file(marker);
        drop(waiting);
    }
    if crate::lane::stopped() {
        return Ok(128 + crate::lane::SIGNAL.load(Ordering::SeqCst));
    }
    let absolute = output(ssh(
        host,
        &format!(
            "mkdir -p \"$HOME\"/{} && cd \"$HOME\"/{} && pwd -P",
            shell_quote(&dest),
            shell_quote(&dest)
        ),
    ))?;
    if !absolute.starts_with('/') || absolute.contains('\n') {
        return Err("invalid remote slot path".into());
    }
    let (copies, links) = managed_lists(meta, &top);
    let mut managed_excludes = copies;
    managed_excludes.extend(links);
    sync(host, &top, &absolute, false, &managed_excludes)?;
    let missing_managed = sync_managed(host, &top, &absolute, meta)?;
    if !missing_managed.is_empty() {
        let paths: Vec<String> = missing_managed.iter().map(|(p, _)| p.clone()).collect();
        let reason = missing_managed
            .iter()
            .map(|(p, r)| format!("{p} ({r})"))
            .collect::<Vec<_>>()
            .join("; ");
        if std::env::var_os("DELEGATE_REMOTE_FALLBACK_ON_MISSING").is_some() {
            shell_status_missing(&paths, &reason);
            eprintln!("delegate remote: managed path(s) unavailable on {host}: {reason}; falling back to local acceptance");
            return Ok(75);
        }
        eprintln!("delegate remote: warning: managed path(s) unavailable on {host}: {reason}");
    }
    if crate::lane::stopped() {
        return Ok(128 + crate::lane::SIGNAL.load(Ordering::SeqCst));
    }
    let id = format!("delegate-{}-{}", std::process::id(), now_ns());
    let pidfile = format!("{absolute}/.{}.pid", id);
    let marker = format!("__{id}_exit=");
    let workdir = Path::new(&absolute)
        .join(relative_cwd)
        .to_string_lossy()
        .into_owned();
    let env = environment_for(meta, light)
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>();
    let mut words = vec!["env".to_string()];
    words.extend(env.clone());
    words.push(program.into());
    words.extend_from_slice(args);
    let invocation = words
        .iter()
        .map(|s| shell_quote(s))
        .collect::<Vec<_>>()
        .join(" ");
    let slice = !s(remote, "slice").is_empty();
    let profile = if s(remote, "profile").is_empty() {
        String::new()
    } else {
        let root = Path::new(&absolute)
            .parent()
            .and_then(Path::parent)
            .ok_or("invalid remote root")?;
        format!(
            ". {} && ",
            shell_quote(&root.join(s(remote, "profile")).to_string_lossy())
        )
    };
    let wrapped = if slice {
        let mut words = vec![
            "systemd-run".to_string(),
            "--user".into(),
            format!("--unit={id}"),
            format!("--slice={}", s(remote, "slice")),
            "--wait".into(),
            "--pipe".into(),
            "--collect".into(),
            "--quiet".into(),
            format!("--working-directory={workdir}"),
        ];
        for var in &env {
            words.extend(["-E".into(), var.clone()]);
        }
        format!(
            "{} sh -c {}",
            words
                .iter()
                .map(|s| shell_quote(s))
                .collect::<Vec<_>>()
                .join(" "),
            shell_quote(&format!("{profile}exec {invocation}"))
        )
    } else {
        invocation
    };
    // The outer shell always exits zero; a private stderr trailer preserves even exit 255,
    // without confusing the program's status with SSH transport failure.
    let profile = if slice { "" } else { profile.as_str() };
    let inner = format!(
        "echo $$ > {pid}; test ! -e {cancel} || exit 143; cd {} && {profile}exec {wrapped}",
        shell_quote(&workdir),
        pid = shell_quote(&pidfile),
        cancel = shell_quote(&format!("{pidfile}.cancel"))
    );
    let script = format!(
        "setsid --wait sh -c {}; rc=$?; rm -f {} {} ; printf '%s%s\\n' {} \"$rc\" >&2",
        shell_quote(&inner),
        shell_quote(&pidfile),
        shell_quote(&format!("{pidfile}.cancel")),
        shell_quote(&marker)
    );
    let inherit_stdin = stdin_is_data_source();
    let mut c = ssh_cmd(host, &script, !inherit_stdin);
    c.stdin(if inherit_stdin {
        Stdio::inherit()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    group(&mut c);
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    shell_status("executing", true);
    let log_path = run.join("remote-output.log");
    let queue_name = if light { "remote-light" } else { "remote-heavy" };
    let full_cmd = if args.is_empty() {
        program.to_string()
    } else {
        clip(&format!("{program} {}", args.join(" ")), 160)
    };
    let _active_remote = ActiveRemoteGuard::start(run, host, queue_name, &full_cmd, &log_path);
    let log_writer = Arc::new(std::sync::Mutex::new(TailLogWriter::create(log_path)));
    let code = Arc::new(AtomicI32::new(-1));
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let a = (
        absolute.clone(),
        top.to_string_lossy().into_owned(),
        marker.clone(),
        code.clone(),
        Some(log_writer.clone()),
    );
    let stdout = std::thread::spawn(move || relay(out, false, a.0, a.1, a.2, a.3, a.4));
    let a = (
        absolute.clone(),
        top.to_string_lossy().into_owned(),
        marker,
        code.clone(),
        Some(log_writer),
    );
    let stderr = std::thread::spawn(move || relay(err, true, a.0, a.1, a.2, a.3, a.4));
    let status = loop {
        if crate::lane::stopped() {
            stop(host, &id, &pidfile, slice);
            end_group(child.id() as i32, 1.0);
            let _ = child.wait();
            let _ = stdout.join();
            let _ = stderr.join();
            // Interrupted commands may still have made useful source edits.
            let _ = sync(host, &top, &absolute, true, &managed_excludes);
            return Ok(128 + crate::lane::SIGNAL.load(Ordering::SeqCst));
        }
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let _ = stdout.join();
    let _ = stderr.join();
    let code = code.load(Ordering::SeqCst);
    if !status.success() || code < 0 {
        stop(host, &id, &pidfile, slice);
        return Err(format!(
            "ssh exited {} without completion",
            status.code().unwrap_or(255)
        ));
    }
    sync(host, &top, &absolute, true, &managed_excludes)?;
    shell_status("completed", true);
    Ok(code)
}
pub fn command(args: &[String]) -> Res<i32> {
    let run = Path::new(args.first().ok_or("_remote needs run and program")?);
    let program = args.get(1).ok_or("_remote needs program")?;
    let mut meta = json(run.join("meta.json"));
    meta["remote"] = effective(&meta["remote"]);
    if !enabled(&meta["remote"]) {
        return Err("remote mode is disabled (existing remote shim cannot execute locally)".into());
    }
    match execute(run, &meta, program, &args[2..]) {
        Ok(code) => Ok(code),
        Err(reason) => {
            if crate::lane::stopped() {
                return Ok(128 + crate::lane::SIGNAL.load(Ordering::SeqCst));
            }
            let executed = std::env::var_os("DELEGATE_SHELL_STATUS")
                .map(|p| b(&json(PathBuf::from(p)), "executed")).unwrap_or(false);
            shell_status("infrastructure", executed);
            unreachable(s(&meta["remote"], "host"), &reason);
            Ok(75)
        }
    }
}
fn unreachable(host: &str, reason: &str) {
    eprintln!(
        "delegate remote: {host} unreachable ({}); remote mode does not fall back to local",
        reason.lines().next().unwrap_or("transport failed")
    );
}
pub fn clean(meta: &Value) {
    if !enabled(&meta["remote"]) {
        return;
    }
    let top = Path::new(s(&meta["worktree"], "path"));
    if !top.is_absolute() || crate::pool::is_slot(top) {
        return;
    }
    let result = destination(meta, top).and_then(|dest| {
        output(ssh(
            s(&meta["remote"], "host"),
            &format!("rm -rf -- \"$HOME\"/{}", shell_quote(&dest)),
        ))
    });
    if let Err(reason) = result {
        unreachable(s(&meta["remote"], "host"), &reason);
    }
}
pub fn check(args: &[String]) -> Res<i32> {
    if args != ["check"] {
        return Err("usage: delegate remote check".into());
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let cfg = crate::config::load(git_top(&cwd).as_deref())?;
    let remote = &cfg.value["remote"];
    if !enabled(remote) {
        println!("delegate remote: disabled");
        return Ok(0);
    }
    let dir = state_dir().join(format!("remote-check-{}-{}", std::process::id(), now_ns()));
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    crate::lane::install_signals();
    let dest = format!(
        "{}/.check-{}-{}",
        s(remote, "root"),
        std::process::id(),
        now_ns()
    );
    let result: Res<()> = (|| {
        let _lane = crate::lane::acquire_remote("remote check", None, remote, |_, _| {})?;
        let absolute = output(ssh(
            s(remote, "host"),
            &format!(
                "mkdir -p \"$HOME\"/{} && cd \"$HOME\"/{} && pwd -P",
                shell_quote(&dest),
                shell_quote(&dest)
            ),
        ))?;
        sync(s(remote, "host"), &dir, &absolute, false, &[])?;
        let root = Path::new(&absolute).parent().ok_or("invalid remote root")?;
        let profile = if s(remote, "profile").is_empty() {
            String::new()
        } else {
            format!(
                ". {} && ",
                shell_quote(&root.join(s(remote, "profile")).to_string_lossy())
            )
        };
        let env = environment(&json!({"remote":remote,"env":cfg.value["env"]}))
            .into_iter()
            .map(|(k, v)| shell_quote(&format!("{k}={v}")))
            .collect::<Vec<_>>()
            .join(" ");
        for program in programs(remote) {
            output(ssh(
                s(remote, "host"),
                &format!(
                    "cd {} && {profile}env {env} {} --version",
                    shell_quote(&absolute),
                    shell_quote(program)
                ),
            ))?;
        }
        sync(s(remote, "host"), &dir, &absolute, true, &[])?;
        Ok(())
    })();
    let _ = output(ssh(
        s(remote, "host"),
        &format!("rm -rf -- \"$HOME\"/{}", shell_quote(&dest)),
    ));
    let _ = fs::remove_dir_all(&dir);
    match result {
        Ok(()) => {
            let plan = remote_plan(remote, true);
            println!(
                "delegate remote: {} ok (ssh + rsync + programs) · {}",
                s(remote, "host"),
                plan.format_summary()
            );
            Ok(0)
        }
        Err(reason) => {
            unreachable(s(remote, "host"), &reason);
            Ok(75)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_light_patterns_and_max_light_and_rejects_invalid() {
        assert!(validate(
            Some(&json!({
                "host": "gpu",
                "programs": ["uv"],
                "maxHeavy": 1,
                "maxLight": 4,
                "light": [
                    "uv run --frozen pytest",
                    "uv run --frozen python scripts/verify_*",
                    ["uv", "run", "ruff", "check"]
                ]
            })),
            false
        )
        .is_ok());
        assert!(validate(Some(&json!({"maxLight": 0, "light": []})), true).is_ok());
        assert!(validate(Some(&json!({"light": "uv run pytest"})), true).is_err());
        assert!(validate(Some(&json!({"light": [""]})), true).is_err());
        assert!(validate(Some(&json!({"light": ["uv 'unclosed"]})), true).is_err());
        assert!(validate(Some(&json!({"light": [[]]})), true).is_err());
        assert!(validate(Some(&json!({"maxLight": -1})), true).is_err());
        assert!(validate(Some(&json!({"maxLight": "4"})), true).is_err());
    }

    #[test]
    fn is_light_matches_prefixes_and_wildcards_and_defaults_false_when_unset() {
        let unset = json!({"host": "gpu", "programs": ["uv"], "maxHeavy": 1});
        assert!(!is_light(
            &unset,
            "uv",
            &["run".into(), "--frozen".into(), "pytest".into(), "-q".into()]
        ));

        let cfg = json!({
            "host": "gpu",
            "programs": ["uv"],
            "maxHeavy": 1,
            "light": [
                "uv run --frozen pytest",
                "uv run --frozen python scripts/verify_*",
                ["uv", "run", "ruff", "check"]
            ]
        });
        assert!(is_light(
            &cfg,
            "uv",
            &["run".into(), "--frozen".into(), "pytest".into(), "-q".into()]
        ));
        assert!(is_light(
            &cfg,
            "uv",
            &[
                "run".into(),
                "--frozen".into(),
                "python".into(),
                "scripts/verify_cpu.py".into(),
                "--fast".into()
            ]
        ));
        assert!(is_light(
            &cfg,
            "uv",
            &["run".into(), "ruff".into(), "check".into(), "src/".into()]
        ));
        assert!(!is_light(
            &cfg,
            "uv",
            &["run".into(), "--frozen".into(), "python".into(), "scripts/train.py".into()]
        ));
        assert!(!is_light(
            &cfg,
            "uv",
            &["run".into(), "pytest".into()]
        ));
    }

    #[test]
    fn shell_command_classification_handles_chaining_env_redirs_and_fails_closed() {
        let cfg = json!({
            "host": "gpu",
            "programs": ["uv"],
            "maxHeavy": 1,
            "light": [
                "uv run --frozen pytest",
                "uv run --frozen python scripts/verify_*"
            ]
        });
        assert!(shell_command_is_light(&cfg, "uv run --frozen pytest -q"));
        assert!(shell_command_is_light(
            &cfg,
            "uv run --frozen pytest -q && uv run --frozen python scripts/verify_all.py"
        ));
        assert!(shell_command_is_light(
            &cfg,
            "cd sub && CUDA_VISIBLE_DEVICES='' uv run --frozen pytest -q 2>&1"
        ));
        // Mixed light + heavy must fail-closed to heavy.
        assert!(!shell_command_is_light(
            &cfg,
            "uv run --frozen pytest -q && uv run python train.py"
        ));
        // Arbitrary script or subshell must fail-closed to heavy.
        assert!(!shell_command_is_light(&cfg, "bash scripts/run.sh"));
        assert!(!shell_command_is_light(&cfg, "uv run --frozen pytest $(echo -q)"));
        assert!(!shell_command_is_light(&cfg, "true"));
        // Unset light config always returns false.
        let unset = json!({"host": "gpu", "programs": ["uv"], "maxHeavy": 1});
        assert!(!shell_command_is_light(&unset, "uv run --frozen pytest -q"));
    }

    #[test]
    fn fd_is_data_source_classifies_null_file_pipes_and_pty() {
        use std::os::fd::AsRawFd;

        // /dev/null -> false
        let devnull = fs::File::open("/dev/null").unwrap();
        assert!(!fd_is_data_source(devnull.as_raw_fd()));

        // Regular file -> true
        let tmp = std::env::temp_dir().join(format!(
            "delegate-remote-stdin-test-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::write(&tmp, b"hello").unwrap();
        let file = fs::File::open(&tmp).unwrap();
        assert!(fd_is_data_source(file.as_raw_fd()));
        drop(file);
        let _ = fs::remove_file(&tmp);

        // Pipe with unclosed idle writer held by self -> false; after writing -> true
        let mut fds = [0 as libc::c_int; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (r, w) = (fds[0], fds[1]);
        assert!(!fd_is_data_source(r), "empty unclosed pipe must return false");
        let msg = b"abc";
        assert_eq!(
            unsafe { libc::write(w, msg.as_ptr().cast(), msg.len()) },
            msg.len() as isize
        );
        assert!(fd_is_data_source(r), "pipe with buffered data must return true");
        let mut sink = [0u8; 8];
        assert_eq!(
            unsafe { libc::read(r, sink.as_mut_ptr().cast(), sink.len()) },
            msg.len() as isize
        );
        unsafe { libc::close(w) };
        assert!(!fd_is_data_source(r), "empty closed pipe (POLLHUP) must return false");
        unsafe { libc::close(r) };

        // PTY -> false
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            assert!(!fd_is_data_source(slave), "pty must return false");
            unsafe {
                libc::close(slave);
                libc::close(master);
            }
        }
    }

    #[test]
    fn host_alias_strips_user_and_tail_log_reads_last_lines() {
        assert_eq!(host_alias("gpu-1"), "gpu-1");
        assert_eq!(host_alias("dev@gpu-1"), "gpu-1");
        assert_eq!(host_alias("  worker-a  "), "worker-a");

        let base = std::env::temp_dir().join(format!(
            "delegate-remote-tail-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let mut writer = TailLogWriter::create(base.join("remote-output.log"));
        for i in 1..=20 {
            writer.append(format!("step {i} loss=0.{i:02}\n").as_bytes());
        }
        let tail = read_output_tail(&base, 3, 2048);
        assert_eq!(
            tail,
            vec!["step 18 loss=0.18", "step 19 loss=0.19", "step 20 loss=0.20"]
        );
        let _ = fs::remove_dir_all(&base);
    }
}
