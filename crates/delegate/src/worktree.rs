use crate::changes::{chain_changes, snapshot, tree_changes};
use crate::common::*;
use crate::lane;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn config(top: &Path) -> Res<(Value, Value)> {
    let path = top.join(".delegate.json");
    let val = if path.is_file() {
        let v: Value = serde_json::from_str(&read(&path))
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if !v.is_object() {
            return Err(format!("{}: expected a JSON object", path.display()));
        }
        v
    } else {
        json!({})
    };
    let env = val
        .get("env")
        .filter(|x| !x.is_null())
        .cloned()
        .unwrap_or(json!({}));
    if !env.is_object() || !env.as_object().unwrap().iter().all(|(_, v)| v.is_string()) {
        return Err(format!("{}: env must map names to strings", path.display()));
    }
    let work = val
        .get("worktree")
        .filter(|x| !x.is_null())
        .cloned()
        .unwrap_or(json!({}));
    if !work.is_object() {
        return Err(format!("{}: worktree must be an object", path.display()));
    }
    let mut out = json!({});
    for key in ["copy", "link", "setup"] {
        let value = work.get(key).cloned().unwrap_or(json!([]));
        let items = if let Some(s) = value.as_str() {
            vec![s.to_string()]
        } else if let Some(a) = value.as_array() {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        } else {
            return Err(format!(
                "{}: worktree.{key} must be a list of strings",
                path.display()
            ));
        };
        if items.iter().any(|x| x.trim().is_empty())
            || value.as_array().is_some_and(|a| a.len() != items.len())
        {
            return Err(format!(
                "{}: worktree.{key} must be a list of strings",
                path.display()
            ));
        }
        if key != "setup"
            && items.iter().any(|x| {
                Path::new(x).is_absolute()
                    || Path::new(x).components().any(|c| c.as_os_str() == "..")
            })
        {
            return Err(format!(
                "{}: worktree.{key} entries must be paths inside the repository",
                path.display()
            ));
        }
        out[key] = json!(items
            .iter()
            .map(|x| if key == "setup" {
                x.clone()
            } else {
                x.trim().trim_end_matches('/').to_string()
            })
            .collect::<Vec<_>>());
    }
    Ok((env, out))
}
fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}
fn copy_rec(src: &Path, dst: &Path) -> Res<()> {
    let m = fs::symlink_metadata(src).map_err(|e| e.to_string())?;
    if m.file_type().is_symlink() {
        symlink(fs::read_link(src).map_err(|e| e.to_string())?, dst).map_err(|e| e.to_string())?;
    } else if m.is_dir() {
        fs::create_dir_all(dst).map_err(|e| e.to_string())?;
        for e in fs::read_dir(src).map_err(|e| e.to_string())?.flatten() {
            copy_rec(&e.path(), &dst.join(e.file_name()))?;
        }
    } else {
        fs::copy(src, dst).map_err(|e| e.to_string())?;
    }
    Ok(())
}
pub fn prepare(meta: &Value, run: &Path) -> Res<Option<String>> {
    let tree = &meta["worktree"];
    let path = PathBuf::from(s(tree, "path"));
    let source = PathBuf::from(s(tree, "source"));
    let cfg = &tree["config"];
    fs::create_dir_all(path.parent().unwrap_or(Path::new("/"))).map_err(|e| e.to_string())?;
    let head =
        git_text(&source, &["rev-parse", "-q", "--verify", "HEAD^{commit}"]).unwrap_or_default();
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(&source)
        .arg("commit-tree")
        .arg(s(&meta["base"], "tree"));
    if !head.is_empty() {
        c.args(["-p", &head]);
    }
    c.arg("-m").arg(format!(
        "delegate: working tree of {} for {}",
        source.display(),
        run.file_name().unwrap_or_default().to_string_lossy()
    ));
    for who in ["AUTHOR", "COMMITTER"] {
        c.env(format!("GIT_{who}_NAME"), "delegate")
            .env(format!("GIT_{who}_EMAIL"), "delegate@localhost");
    }
    let out = c.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Ok(Some(format!(
            "worktree setup failed: {}",
            clip(String::from_utf8_lossy(&out.stderr).trim(), 300)
        )));
    }
    let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if let Err(e) = git(
        &source,
        &[
            "worktree",
            "add",
            "--detach",
            &path.to_string_lossy(),
            &commit,
        ],
    ) {
        return Ok(Some(format!("worktree setup failed: {}", clip(&e, 300))));
    }
    if s(meta, "mode") == "read-only" && !head.is_empty() {
        let _ = git(&path, &["reset", "-q", &head]);
    }
    let copies = strings(&cfg["copy"]);
    let links = strings(&cfg["link"]);
    for item in copies.iter().chain(links.iter()) {
        let origin = source.join(item);
        let target = path.join(item);
        if target.is_dir()
            && !target.is_symlink()
            && fs::read_dir(&target).is_ok_and(|mut d| d.next().is_none())
        {
            let _ = fs::remove_dir(&target);
        }
        if !origin.exists() || target.exists() || target.is_symlink() {
            continue;
        }
        fs::create_dir_all(target.parent().unwrap_or(&path)).map_err(|e| e.to_string())?;
        if links.contains(item) {
            symlink(&origin, &target).map_err(|e| e.to_string())?;
        } else {
            copy_rec(&origin, &target)?;
        }
    }
    let setup = strings(&cfg["setup"]);
    if setup.is_empty() {
        return Ok(None);
    }
    let mut env = meta["env"].clone();
    if !env.is_object() {
        env = json!({});
    }
    let timeout = seconds(&setting("SETUP_TIMEOUT", "10m"))?;
    let _slot = lane::acquire(
        &format!(
            "setup {}",
            run.file_name().unwrap_or_default().to_string_lossy()
        ),
        None,
        |_, _| {},
    )?;
    let log = run.join("setup.log");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| e.to_string())?;
    for command in setup {
        writeln!(file, "$ {command}").map_err(|e| e.to_string())?;
        file.flush().map_err(|e| e.to_string())?;
        let (code, _) = run_shell(&command, &path, &env, timeout, &mut file, None)?;
        writeln!(file, "[exit {code}]").map_err(|e| e.to_string())?;
        if code != 0 {
            return Ok(Some(format!(
                "worktree setup command failed (exit {code}): {command}; see {}",
                log.display()
            )));
        }
    }
    Ok(None)
}
pub fn remove(meta: &Value) {
    let tree = &meta["worktree"];
    let path = Path::new(s(tree, "path"));
    if !path.exists() {
        return;
    }
    let source = Path::new(s(tree, "source"));
    if git(
        source,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    )
    .is_err()
    {
        let _ = fs::remove_dir_all(path);
        let _ = git(source, &["worktree", "prune"]);
    }
}
fn blob(top: &Path, tree: &str, path: &str) -> Res<(Option<String>, Option<Vec<u8>>)> {
    let entry = git(top, &["ls-tree", "-z", tree, "--", path])?;
    let Some(first) = entry.split(|x| *x == 0).next().filter(|x| !x.is_empty()) else {
        return Ok((None, None));
    };
    let text = String::from_utf8_lossy(first);
    let Some((attrs, _)) = text.split_once('\t') else {
        return Ok((None, None));
    };
    let f: Vec<_> = attrs.split_whitespace().collect();
    if f.len() < 3 {
        return Ok((None, None));
    }
    let mode = Some(f[0].to_string());
    let bytes = if f[1] == "blob" {
        Some(git(top, &["cat-file", "blob", f[2]])?)
    } else {
        None
    };
    Ok((mode, bytes))
}
fn entry_mode(path: &Path) -> Option<String> {
    if path.is_symlink() {
        Some("120000".into())
    } else if path.is_file() {
        let m = fs::metadata(path).ok()?;
        Some(
            if m.permissions().mode() & 0o111 != 0 {
                "100755"
            } else {
                "100644"
            }
            .into(),
        )
    } else if path.exists() {
        Some("dir".into())
    } else {
        None
    }
}
fn current(path: &Path) -> Option<Vec<u8>> {
    if path.is_symlink() {
        Some(
            fs::read_link(path)
                .ok()?
                .to_string_lossy()
                .as_bytes()
                .to_vec(),
        )
    } else if path.is_file() {
        fs::read(path).ok()
    } else {
        None
    }
}
fn through_symlink(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return true;
    };
    let mut here = root.to_path_buf();
    for component in rel
        .components()
        .take(rel.components().count().saturating_sub(1))
    {
        here.push(component);
        if here.is_symlink() {
            return true;
        }
    }
    false
}
fn write_entry(path: &Path, mode: &str, content: &[u8]) -> Res<()> {
    if path.is_symlink() || path.exists() {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(path.parent().unwrap_or(Path::new("/"))).map_err(|e| e.to_string())?;
    if mode == "120000" {
        symlink(String::from_utf8_lossy(content).as_ref(), path).map_err(|e| e.to_string())?;
    } else {
        write(path, content)?;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if mode == "100755" { 0o755 } else { 0o644 }),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}
#[derive(Clone)]
struct Action {
    kind: String,
    path: String,
    target: PathBuf,
    mode: Option<String>,
    content: Vec<u8>,
}
pub fn apply(run: &Path, merge: bool, dry: bool) -> Res<i32> {
    let (meta, mut top, _, mut after) = chain_changes(run)?;
    if meta["worktree"].is_null() {
        return Err(format!(
            "{} worked in place; its changes are already in {}",
            run.file_name().unwrap_or_default().to_string_lossy(),
            s(&meta, "workdir")
        ));
    }
    if s(&meta, "mode") == "read-only" {
        return Err(format!(
            "{} is read-only; its worktree is a snapshot to read, with nothing to apply",
            run.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    let before = s(&meta, "chainBase").to_string();
    let source = PathBuf::from(s(&meta["worktree"], "source"));
    let worktree = PathBuf::from(s(&meta["worktree"], "path"));
    let mut large = json(run.join("changes.json"))["afterLarge"].clone();
    let exclude = strings(&meta["snapshotExclude"]);
    if worktree.is_dir() {
        if let Some(now) = snapshot(&worktree, run, &exclude) {
            top = worktree.clone();
            after = s(&now, "tree").into();
            large = now["large"].clone();
        }
    }
    let changes = tree_changes(&top, &json!({"tree":before}), &json!({"tree":after}))?;
    let mut actions = vec![];
    let mut conflicts = vec![];
    for c in changes {
        let path = s(&c, "path").to_string();
        let target = source.join(&path);
        let (old_mode, old) = blob(&top, &before, &path)?;
        let (mode, new) = blob(&top, &after, &path)?;
        let now = current(&target);
        let now_mode = entry_mode(&target);
        let valid = |x: &Option<String>| {
            x.as_deref()
                .is_none_or(|v| ["100644", "100755", "120000"].contains(&v))
        };
        if through_symlink(&source, &target)
            || now_mode.as_deref() == Some("dir")
            || !valid(&old_mode)
            || !valid(&mode)
        {
            conflicts.push(path);
        } else if now == new && now_mode == mode {
            continue;
        } else if now == old && (now_mode == old_mode || now_mode == mode) {
            actions.push(Action {
                kind: if new.is_none() { "deleted" } else { "applied" }.into(),
                path,
                target,
                mode,
                content: new.unwrap_or_default(),
            });
        } else if let (Some(base), Some(theirs), Some(mine)) = (&old, &new, &now) {
            if [old_mode.as_deref(), mode.as_deref(), now_mode.as_deref()].contains(&Some("120000"))
                || base.contains(&0)
                || theirs.contains(&0)
                || mine.contains(&0)
            {
                conflicts.push(path);
                continue;
            }
            let scratch = run.join(format!(".merge-{}", std::process::id()));
            fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
            let minefile = scratch.join("mine");
            let basefile = scratch.join("base");
            let theirfile = scratch.join("theirs");
            write(&minefile, mine)?;
            write(&basefile, base)?;
            write(&theirfile, theirs)?;
            let status = Command::new("git")
                .arg("merge-file")
                .args([
                    "-L",
                    "current",
                    "-L",
                    "base",
                    "-L",
                    run.file_name().unwrap_or_default().to_str().unwrap_or(""),
                ])
                .args([&minefile, &basefile, &theirfile])
                .status()
                .map_err(|e| e.to_string())?;
            let merged = fs::read(&minefile).map_err(|e| e.to_string())?;
            let _ = fs::remove_dir_all(scratch);
            if status.success() {
                actions.push(Action {
                    kind: "merged".into(),
                    path,
                    target,
                    mode: if old_mode != mode { mode } else { None },
                    content: merged,
                });
            } else if merge {
                actions.push(Action {
                    kind: "conflict-markers".into(),
                    path,
                    target,
                    mode: None,
                    content: merged,
                });
            } else {
                conflicts.push(path);
            }
        } else {
            conflicts.push(path);
        }
    }
    if let Some(obj) = large.as_object() {
        for path in obj.keys() {
            let origin = worktree.join(path);
            let target = source.join(path);
            if !origin.is_file()
                || through_symlink(&source, &target)
                || entry_mode(&target).as_deref() == Some("dir")
            {
                conflicts.push(path.clone());
            } else if !target.exists() && !target.is_symlink() {
                actions.push(Action {
                    kind: "copied".into(),
                    path: path.clone(),
                    target,
                    mode: Some("large".into()),
                    content: fs::read(origin).map_err(|e| e.to_string())?,
                });
            } else if current(&target) != Some(fs::read(origin).map_err(|e| e.to_string())?) {
                conflicts.push(path.clone());
            }
        }
    }
    if actions.is_empty() && conflicts.is_empty() {
        eprintln!("delegate: no changes to apply");
        return Ok(0);
    }
    if !conflicts.is_empty() && !merge {
        for p in &conflicts {
            println!(" conflict         {p}");
        }
        eprintln!("delegate: nothing applied; {} file(s) were also changed in {} since the run started. Rerun with --merge to apply the rest and write conflict markers into text files, or inspect with: {} diff {} --total",conflicts.len(),source.display(),script().display(),run.file_name().unwrap_or_default().to_string_lossy());
        return Ok(1);
    }
    let marked = actions.iter().any(|x| x.kind == "conflict-markers");
    for a in actions {
        if !dry {
            match a.kind.as_str() {
                "deleted" => {
                    let _ = fs::remove_file(&a.target);
                }
                "copied" => {
                    fs::create_dir_all(a.target.parent().unwrap_or(&source))
                        .map_err(|e| e.to_string())?;
                    write(&a.target, &a.content)?;
                }
                _ => {
                    if let Some(mode) = a.mode.as_deref() {
                        write_entry(&a.target, mode, &a.content)?;
                    } else {
                        write(&a.target, &a.content)?;
                    }
                }
            }
        }
        println!(" {:<16} {}", a.kind, a.path);
    }
    for p in &conflicts {
        println!(
            " {:<16} {p}  (binary, symlink, type change or large; take it from {})",
            "skipped",
            worktree.display()
        );
    }
    if dry {
        eprintln!("delegate: dry run; nothing written");
    } else if conflicts.is_empty() {
        let mut chain = run.to_path_buf();
        loop {
            write(chain.join(".applied"), format!("{}\n", iso()))?;
            let parent = s(&json(chain.join("meta.json")), "parent").to_string();
            if parent.is_empty()
                || !chain
                    .parent()
                    .unwrap_or(Path::new("/"))
                    .join(&parent)
                    .is_dir()
            {
                break;
            }
            chain = chain.parent().unwrap().join(parent);
        }
    }
    Ok(if !conflicts.is_empty() || marked {
        1
    } else {
        0
    })
}
