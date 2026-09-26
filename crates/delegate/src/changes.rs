use crate::common::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn git_index(top: &Path, args: &[String], index: &Path) -> Res<Vec<u8>> {
    let o = Command::new("git")
        .arg("-C")
        .arg(top)
        .args(args)
        .env("GIT_INDEX_FILE", index)
        .output()
        .map_err(|e| e.to_string())?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).into());
    }
    Ok(o.stdout)
}
fn zstrings(data: &[u8]) -> Vec<String> {
    data.split(|x| *x == 0)
        .filter(|x| !x.is_empty())
        .map(|x| String::from_utf8_lossy(x).to_string())
        .collect()
}
fn preserve_times(src: &Path, dst: &Path) {
    use std::os::unix::ffi::OsStrExt;
    if let Ok(m) = fs::metadata(src) {
        let a = libc::timespec {
            tv_sec: m.atime(),
            tv_nsec: m.atime_nsec(),
        };
        let b = libc::timespec {
            tv_sec: m.mtime(),
            tv_nsec: m.mtime_nsec(),
        };
        if let Ok(c) = std::ffi::CString::new(dst.as_os_str().as_bytes()) {
            unsafe {
                libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [a, b].as_ptr(), 0);
            }
        }
    }
}
pub fn snapshot(top: &Path, scratch: &Path, exclude: &[String]) -> Option<Value> {
    let index = scratch.join(format!(".snapshot-index-{}", std::process::id()));
    let result = (|| -> Res<Value> {
        let real = git_text(top, &["rev-parse", "--git-path", "index"])?;
        let real = top.join(real);
        if real.is_file() {
            fs::copy(&real, &index).map_err(|e| e.to_string())?;
            preserve_times(&real, &index);
        }
        let limit = setting("SNAPSHOT_MAX_BYTES", "2097152")
            .parse::<u64>()
            .unwrap_or(2097152);
        let mut large = serde_json::Map::new();
        for name in zstrings(&git(
            top,
            &["ls-files", "-z", "--others", "--exclude-standard"],
        )?) {
            let p = top.join(&name);
            if let Ok(info) = fs::symlink_metadata(&p) {
                if info.is_file() && info.len() > limit {
                    large.insert(
                        name,
                        json!([info.len(), info.mtime_nsec() + info.mtime() * 1_000_000_000]),
                    );
                }
            }
        }
        let present: Vec<String> = exclude
            .iter()
            .filter(|x| top.join(x).exists() || top.join(x).is_symlink())
            .cloned()
            .collect();
        let mut ignored = BTreeSet::new();
        if !present.is_empty() {
            let mut child = Command::new("git")
                .arg("-C")
                .arg(top)
                .args(["check-ignore", "-z", "--stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| e.to_string())?;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(present.join("\0").as_bytes())
                .map_err(|e| e.to_string())?;
            ignored.extend(zstrings(
                &child.wait_with_output().map_err(|e| e.to_string())?.stdout,
            ));
        }
        let mut skip: BTreeSet<String> = large.keys().cloned().collect();
        skip.extend(present.into_iter().filter(|x| !ignored.contains(x)));
        let mut args = vec!["add".into(), "-A".into(), "--".into(), ".".into()];
        for name in skip {
            args.push(format!(":(exclude,literal){name}"));
        }
        git_index(top, &args, &index)?;
        let tree = String::from_utf8_lossy(&git_index(top, &["write-tree".into()], &index)?)
            .trim()
            .to_string();
        Ok(
            json!({"tree":tree,"large":large,"submodules":submodule_fingerprints(top,exclude).unwrap_or_default()}),
        )
    })();
    let _ = fs::remove_file(index);
    result.ok()
}
fn submodule_fingerprints(top: &Path, exclude: &[String]) -> Res<BTreeMap<String, String>> {
    let mut prints = BTreeMap::new();
    for entry in zstrings(&git(top, &["ls-files", "-s", "-z"])?) {
        if !entry.starts_with("160000 ") {
            continue;
        }
        let Some((_, path)) = entry.split_once('\t') else {
            continue;
        };
        let checkout = top.join(path);
        if exclude.iter().any(|x| x == path)
            || checkout.is_symlink()
            || !checkout.join(".git").exists()
        {
            continue;
        }
        let mut digest = sha1_smol::Sha1::new();
        for args in [
            &["rev-parse", "HEAD"][..],
            &["diff", "--binary", "HEAD"],
            &["ls-files", "-z", "--others", "--exclude-standard"],
        ] {
            digest.update(&git(&checkout, args)?);
        }
        for name in zstrings(&git(
            &checkout,
            &["ls-files", "-z", "--others", "--exclude-standard"],
        )?) {
            if let Ok(m) = fs::metadata(checkout.join(&name)) {
                digest.update(
                    format!("{}:{}", m.len(), m.mtime() * 1_000_000_000 + m.mtime_nsec())
                        .as_bytes(),
                );
            }
        }
        prints.insert(path.to_string(), digest.digest().to_string());
    }
    Ok(prints)
}
pub fn tree_changes(top: &Path, before: &Value, after: &Value) -> Res<Vec<Value>> {
    let a = s(before, "tree");
    let b = s(after, "tree");
    let num = git(top, &["diff", "--numstat", "-z", "--no-renames", a, b])?;
    let mut counts = BTreeMap::new();
    for row in zstrings(&num) {
        let parts: Vec<_> = row.splitn(3, '\t').collect();
        if parts.len() == 3 {
            counts.insert(
                parts[2].to_string(),
                if parts[0] == "-" {
                    (Value::Null, Value::Null)
                } else {
                    (
                        json!(parts[0].parse::<i64>().unwrap_or(0)),
                        json!(parts[1].parse::<i64>().unwrap_or(0)),
                    )
                },
            );
        }
    }
    let names = zstrings(&git(
        top,
        &["diff", "--name-status", "-z", "--no-renames", a, b],
    )?);
    let mut changes = vec![];
    for pair in names.chunks_exact(2) {
        let (added, deleted) = counts
            .get(&pair[1])
            .cloned()
            .unwrap_or((Value::Null, Value::Null));
        changes.push(json!({"path":pair[1],"status":pair[0].chars().next().unwrap_or('M').to_string(),"added":added,"deleted":deleted}));
    }
    let listed: BTreeSet<_> = changes.iter().map(|x| s(x, "path").to_string()).collect();
    for (kind, flag) in [("large", "large"), ("submodules", "submodule")] {
        let old = before.get(kind).and_then(Value::as_object);
        let new = after.get(kind).and_then(Value::as_object);
        let mut keys = BTreeSet::new();
        if let Some(m) = old {
            keys.extend(m.keys().cloned());
        }
        if let Some(m) = new {
            keys.extend(m.keys().cloned());
        }
        for path in keys {
            let x = old.and_then(|m| m.get(&path));
            let y = new.and_then(|m| m.get(&path));
            if x != y && (kind == "large" || !listed.contains(&path)) {
                let status = if x.is_none() {
                    "A"
                } else if y.is_none() {
                    "D"
                } else {
                    "M"
                };
                let mut c = json!({"path":path,"status":status,"added":null,"deleted":null});
                c[flag] = json!(true);
                changes.push(c);
            }
        }
    }
    changes.sort_by(|a, b| s(a, "path").cmp(s(b, "path")));
    Ok(changes)
}
pub fn record(meta: &Value, run: &Path) -> Option<(Vec<Value>, Value)> {
    let base = meta.get("base")?;
    if base.is_null() {
        return None;
    }
    let top = Path::new(s(meta, "top"));
    let excludes = meta["snapshotExclude"]
        .as_array()
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let after = snapshot(top, run, &excludes)?;
    let changes = tree_changes(top, base, &after).ok()?;
    let recorded = json!({"base":s(base,"tree"),"after":s(&after,"tree"),"top":top,"afterLarge":after["large"],"changes":changes});
    write_json(run.join("changes.json"), &recorded).ok()?;
    let patch = git(
        top,
        &[
            "diff",
            "--binary",
            "--no-renames",
            s(base, "tree"),
            s(&after, "tree"),
        ],
    )
    .ok()?;
    write(run.join("changes.patch"), patch).ok()?;
    let totals = json!({"files":changes.len(),"added":changes.iter().map(|x|n(x,"added")).sum::<i64>(),"deleted":changes.iter().map(|x|n(x,"deleted")).sum::<i64>(),"after":s(&after,"tree")});
    Some((changes, totals))
}
pub fn print_changes(run: &Path) {
    let rec = json(run.join("changes.json"));
    let meta = json(run.join("meta.json"));
    let name = run.file_name().unwrap_or_default().to_string_lossy();
    if rec.is_null() {
        if !s(&json(run.join("summary.json")), "warning").is_empty() {
            println!("\n===== changes: {name}: unknown (the working tree could not be snapshotted) =====");
        } else if s(&meta, "mode") == "write" {
            println!(
                "\n===== changes: {name}: not tracked (not a git repository; see files) ====="
            );
        }
        return;
    }
    let changes = rec["changes"].as_array().cloned().unwrap_or_default();
    if changes.is_empty() {
        if s(&meta, "mode") == "write" {
            println!("\n===== changes: {name}: none =====");
        }
        return;
    }
    let added: i64 = changes.iter().map(|x| n(x, "added")).sum();
    let deleted: i64 = changes.iter().map(|x| n(x, "deleted")).sum();
    let where_ = if meta["worktree"].is_object() {
        format!("; worktree {}", s(&meta["worktree"], "path"))
    } else {
        String::new()
    };
    let plural = if changes.len() == 1 { "" } else { "s" };
    println!(
        "\n===== changes: {name} ({} file{plural}, +{added} -{deleted}{where_}) =====",
        changes.len()
    );
    for c in changes.iter().take(40) {
        let label = if b(c, "large") {
            "large file".into()
        } else if b(c, "submodule") {
            "submodule contents".into()
        } else if c["added"].is_null() {
            "binary".into()
        } else {
            format!("+{} -{}", n(c, "added"), n(c, "deleted"))
        };
        println!(" {} {}  {label}", s(c, "status"), s(c, "path"));
    }
    if changes.len() > 40 {
        println!(
            " ... {} more in {}",
            changes.len() - 40,
            run.join("changes.json").display()
        );
    }
    let mut hint = format!("{} diff {name}", script().display());
    if meta["worktree"].is_object() {
        hint += &format!(
            "; merge into {}: {} apply {name}",
            s(&meta["worktree"], "source"),
            script().display()
        );
    }
    println!("diff: {hint}\n===== end changes: {name} =====");
}
pub fn chain_changes(run: &Path) -> Res<(Value, PathBuf, String, String)> {
    let meta = json(run.join("meta.json"));
    let rec = json(run.join("changes.json"));
    if rec.is_null() {
        return Err(format!("{} has no recorded changes (still running, not a git repository, or before delegate 4.1)",run.file_name().unwrap_or_default().to_string_lossy()));
    }
    let mut top = PathBuf::from(s(&rec, "top"));
    if !top.is_dir() {
        top = PathBuf::from(s(&meta["worktree"], "source"));
    }
    Ok((meta, top, s(&rec, "base").into(), s(&rec, "after").into()))
}
