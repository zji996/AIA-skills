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
fn ssh(host: &str, script: &str) -> Command {
    let mut c = Command::new("ssh");
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
fn relay(
    reader: impl Read,
    stderr: bool,
    remote: String,
    local: String,
    marker: String,
    exit: Arc<AtomicI32>,
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
    let mut c = ssh(host, &script);
    c.stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    group(&mut c);
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    shell_status("executing", true);
    let code = Arc::new(AtomicI32::new(-1));
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let a = (
        absolute.clone(),
        top.to_string_lossy().into_owned(),
        marker.clone(),
        code.clone(),
    );
    let stdout = std::thread::spawn(move || relay(out, false, a.0, a.1, a.2, a.3));
    let a = (
        absolute.clone(),
        top.to_string_lossy().into_owned(),
        marker,
        code.clone(),
    );
    let stderr = std::thread::spawn(move || relay(err, true, a.0, a.1, a.2, a.3));
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
}
