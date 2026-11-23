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
        ("forwardEnv", json!([])),
    ] {
        if v.get(k).is_none() {
            v[k] = default;
        }
    }
    v
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
    vars.insert("CARGO_BUILD_JOBS".into(), plan.heavy_jobs.to_string());
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
    let mut shown = false;
    let _lane = crate::lane::acquire_remote(
        &format!("{}: {program}", top.display()),
        Some(run),
        remote,
        |ahead, _| {
            if !shown {
                eprintln!("delegate remote: queued behind {ahead}");
                shown = true;
            }
        },
    )?;
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
        match lock(&locks.join(lock_name.as_str()), true, true) {
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
    let env = environment(meta)
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
