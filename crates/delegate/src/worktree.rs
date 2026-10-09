use crate::changes::{chain_changes, gitlink, snapshot, tree_changes};
use crate::common::*;
use crate::lane;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn work_config(val: &Value, path: &Path) -> Res<Value> {
    let work = val
        .get("worktree")
        .filter(|x| !x.is_null())
        .cloned()
        .unwrap_or(json!({}));
    if !work.is_object() {
        return Err(format!("{}: worktree must be an object", path.display()));
    }
    let mut out = json!({});
    for key in ["copy", "link", "setup", "writeSetup"] {
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
        if items
            .iter()
            .any(|x| x.trim().is_empty() || x.contains('\0'))
            || value.as_array().is_some_and(|a| a.len() != items.len())
        {
            return Err(format!(
                "{}: worktree.{key} must be a list of strings",
                path.display()
            ));
        }
        if !key.ends_with("etup")
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
            .map(|x| if key.ends_with("etup") {
                x.clone()
            } else {
                x.trim().trim_end_matches('/').to_string()
            })
            .collect::<Vec<_>>());
    }
    Ok(out)
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
pub fn generated(source: &Path) -> Res<(Vec<String>, String)> {
    let file = source.join(".delegate.json");
    let cfg = crate::config::read_file(&file)?.unwrap_or(json!({}));
    generated_config(&cfg, &file)
}
pub fn generated_config(cfg: &Value, file: &Path) -> Res<(Vec<String>, String)> {
    let Some(spec) = cfg.get("generated") else {
        return Ok((vec![], String::new()));
    };
    let rules = rule_list(spec, "paths", file)?.unwrap_or_default();
    rule_list(spec, "inputs", file)?;
    let command = spec["command"]
        .as_str()
        .filter(|s| !s.contains('\0'))
        .ok_or_else(|| {
            format!(
                "{}: generated.command must be a string without NUL",
                file.display()
            )
        })?;
    Ok((rules, command.to_string()))
}
/// Optional `generated.inputs`: when set, `apply` regenerates only if a merged path matches.
pub fn generated_inputs(source: &Path) -> Res<Vec<String>> {
    let file = source.join(".delegate.json");
    let cfg = crate::config::read_file(&file)?.unwrap_or(json!({}));
    match cfg.get("generated") {
        Some(spec) => Ok(rule_list(spec, "inputs", &file)?.unwrap_or_default()),
        None => Ok(vec![]),
    }
}
fn rule_list(spec: &Value, key: &str, file: &Path) -> Res<Option<Vec<String>>> {
    let Some(value) = spec.get(key) else {
        return if key == "paths" {
            Err(format!(
                "{}: generated.paths must be a list",
                file.display()
            ))
        } else {
            Ok(None)
        };
    };
    let items = value
        .as_array()
        .ok_or_else(|| format!("{}: generated.{key} must be a list", file.display()))?;
    let mut rules = vec![];
    for item in items {
        let path = item.as_str().ok_or_else(|| {
            format!(
                "{}: generated.{key} entries must be strings",
                file.display()
            )
        })?;
        let core = path.trim_end_matches('/');
        if core.is_empty()
            || core.contains('\0')
            || Path::new(core)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(format!(
                "{}: invalid generated.{key} entry: {path}",
                file.display()
            ));
        }
        let mut normalized = Path::new(core)
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if path.ends_with('/') {
            normalized.push('/');
        }
        rules.push(normalized);
    }
    Ok(Some(rules))
}
/// After a merged deletion, drop directories it left empty, stopping at the repository root.
fn remove_empty_parents(file: &Path, root: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}
pub fn matches_rule(path: &str, rules: &[String]) -> bool {
    rules.iter().any(|rule| {
        if rule.ends_with('/') {
            path.starts_with(rule)
        } else {
            path == rule
        }
    })
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
    write(run.join("worktree-base-commit"), &commit)?;
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
        let _ = git(&path, &["reset", "-q", "--soft", &head]);
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
        if !origin.exists() && !origin.is_symlink() {
            eprintln!(
                "delegate: worktree source missing, skipping {}",
                origin.display()
            );
            continue;
        }
        if target.exists() || target.is_symlink() {
            continue;
        }
        fs::create_dir_all(target.parent().unwrap_or(&path)).map_err(|e| e.to_string())?;
        if links.contains(item) {
            symlink(&origin, &target).map_err(|e| e.to_string())?;
        } else {
            copy_rec(&origin, &target)?;
        }
    }
    // Managed paths can replace tracked entries such as gitlinks with local links.
    for item in copies.iter().chain(links.iter()) {
        let _ = git(&path, &["update-index", "--skip-worktree", "--", item]);
    }
    for item in strings(&meta["share"]) {
        let Ok(relative) = Path::new(&item).strip_prefix(&source) else { continue };
        if relative.as_os_str().is_empty() { continue; }
        let target = path.join(relative);
        if target.exists() || target.is_symlink() {
            return Err(format!("--share target already exists: {}", target.display()));
        }
        fs::create_dir_all(target.parent().unwrap_or(&path)).map_err(|e| e.to_string())?;
        symlink(&item, &target).map_err(|e| e.to_string())?;
    }
    let mut setup = strings(&cfg["setup"]);
    // Heavy environments (e.g. a Python venv) only for tasks expected to build and test.
    if s(meta, "mode") == "write" {
        setup.extend(strings(&cfg["writeSetup"]));
    }
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
        Some(run),
        |_, _| {},
    )?;
    let log = run.join("setup.log");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| e.to_string())?;
    for (index, command) in setup.into_iter().enumerate() {
        writeln!(file, "$ {command}").map_err(|e| e.to_string())?;
        file.flush().map_err(|e| e.to_string())?;
        let (code, _) = run_shell(
            (&command, &format!("setup-{}", index + 1)),
            &path,
            run,
            &env,
            timeout,
            &mut file,
            None,
        )?;
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
pub fn remove(meta: &Value) -> crate::cleanup::Containers {
    let tree = &meta["worktree"];
    let path = Path::new(s(tree, "path"));
    if !path.is_absolute() || path == Path::new(s(tree, "source")) {
        return crate::cleanup::Containers::default();
    }
    let containers = crate::cleanup::containers(path, false);
    if !path.exists() {
        return containers;
    }
    // Ask the worktree which repository owns it: the recorded source may be an upstream worktree
    // (--in) that has already been cleaned, and pruning from there would leave this entry registered.
    let owner = git_text(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map(PathBuf::from)
    .unwrap_or_else(|_| PathBuf::from(s(tree, "source")));
    if git(
        &owner,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    )
    .is_err()
    {
        let _ = fs::remove_dir_all(path);
        let _ = git(&owner, &["worktree", "prune"]);
    }
    containers
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
fn applied_state(run: &Path, meta: &Value) -> Value {
    let state = json(run.join(".sync-base"));
    if !s(&state, "tree").is_empty() {
        return state;
    }
    let state = json(run.join(".applied"));
    if !s(&state, "tree").is_empty() {
        return state;
    }
    let mut parent = s(meta, "parent").to_string();
    while !parent.is_empty() {
        let path = run.parent().unwrap_or(Path::new("/")).join(&parent);
        let state = json(path.join(".sync-base"));
        if !s(&state, "tree").is_empty() {
            return state;
        }
        let state = json(path.join(".applied"));
        if !s(&state, "tree").is_empty() {
            return state;
        }
        parent = s(&json(path.join("meta.json")), "parent").to_string();
    }
    meta["appliedBase"].clone()
}
pub fn pending_changes(run: &Path, meta: &Value) -> Option<Value> {
    if !meta["worktree"].is_object() || s(meta, "mode") != "write" {
        return None;
    }
    let rec = json(run.join("changes.json"));
    let mut base = applied_state(run, meta);
    if s(&base, "tree").is_empty() {
        base = json!({"tree":s(meta,"chainBase"),"large":{}});
    }
    let after = json!({"tree":rec["after"],"large":rec["afterLarge"]});
    let changes = tree_changes(Path::new(s(&rec, "top")), &base, &after, true).ok()?;
    Some(
        json!({"files":changes.len(),"added":changes.iter().map(|c| n(c,"added")).sum::<i64>(),"deleted":changes.iter().map(|c| n(c,"deleted")).sum::<i64>()}),
    )
}
fn file_hash(path: &Path) -> Res<String> {
    let mut file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hash = sha1_smol::Sha1::new();
    let mut buffer = [0; 65536];
    loop {
        let size = file
            .read(&mut buffer)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if size == 0 {
            return Ok(hash.digest().to_string());
        }
        hash.update(&buffer[..size]);
    }
}
fn final_entry(root: &Path, path: &str) -> Option<Value> {
    use std::os::unix::ffi::OsStrExt;
    let target = root.join(path);
    if through_symlink(root, &target) {
        return None;
    }
    let info = match fs::symlink_metadata(&target) {
        Ok(info) => info,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Some(json!({"mode":"absent"}));
        }
        Err(_) => return None,
    };
    if info.file_type().is_symlink() {
        let link = fs::read_link(&target).ok()?;
        let bytes = link.as_os_str().as_bytes();
        return Some(
            json!({"mode":"120000","size":bytes.len(),"hash":sha1_smol::Sha1::from(bytes).digest().to_string()}),
        );
    }
    if !info.is_file() {
        return None;
    }
    Some(
        json!({"mode":if info.permissions().mode() & 0o111 != 0 {"100755"} else {"100644"},"size":info.len(),"hash":file_hash(&target).ok()?}),
    )
}
/// Freeze only changed paths; status compares these against the source working tree.
pub fn capture_final_entries(meta: &Value, run: &Path) {
    if s(meta, "mode") != "write" || !meta["worktree"].is_object() {
        return;
    }
    let mut rec = json(run.join("changes.json"));
    if s(&rec, "after").is_empty() {
        return;
    }
    let Some(changes) = final_changes(run, meta, &rec) else {
        return;
    };
    let root = Path::new(s(&meta["worktree"], "path"));
    let entries = changes
        .iter()
        .map(|change| {
            let path = s(change, "path");
            (
                path.to_string(),
                final_entry(root, path).unwrap_or(Value::Null),
            )
        })
        .collect::<serde_json::Map<String, Value>>();
    rec["finalEntries"] = json!(entries);
    let _ = write_json(run.join("changes.json"), &rec);
}
fn final_changes(run: &Path, meta: &Value, rec: &Value) -> Option<Vec<Value>> {
    let mut baseline = applied_state(run, meta);
    if s(&baseline, "tree").is_empty() {
        let mut first = meta.clone();
        while !s(&first, "parent").is_empty() {
            let parent = run.parent()?.join(s(&first, "parent"));
            let older = json(parent.join("meta.json"));
            if !older.is_object() {
                return None;
            }
            first = older;
        }
        baseline = json!({"tree":if s(meta,"chainBase").is_empty() {s(rec,"base")} else {s(meta,"chainBase")},"large":first["base"]["large"]});
    }
    tree_changes(
        Path::new(s(rec, "top")),
        &baseline,
        &json!({"tree":rec["after"],"large":rec["afterLarge"]}),
        true,
    )
    .ok()
}
/// Old records retain their Git snapshot; large files without frozen hashes cannot prove equality.
fn recorded_entries(
    run: &Path,
    meta: &Value,
    rec: &Value,
) -> Option<serde_json::Map<String, Value>> {
    if let Some(entries) = rec["finalEntries"].as_object() {
        return Some(entries.clone());
    }
    let top = Path::new(s(rec, "top"));
    final_changes(run, meta, rec)?.iter().map(|change| {
        let path = s(change, "path");
        if b(change, "large") || b(change, "submodule") {
            return None;
        }
        let (mode, content) = blob(top, s(rec, "after"), path).ok()?;
        let entry = if let (Some(mode), Some(bytes)) = (mode, content) {
            json!({"mode":mode,"size":bytes.len(),"hash":sha1_smol::Sha1::from(&bytes).digest().to_string()})
        } else {
            json!({"mode":"absent"})
        };
        Some((path.to_string(), entry))
    }).collect()
}
pub fn detect_applied(run: &Path, meta: &Value) -> bool {
    if run.join(".applied").exists() {
        return true;
    }
    if s(meta, "mode") != "write"
        || !meta["worktree"].is_object()
        || !run.join("exit_code").is_file()
        || run.join(".generate-pending").exists()
    {
        return false;
    }
    let rec = json(run.join("changes.json"));
    let Some(entries) = recorded_entries(run, meta, &rec) else {
        return false;
    };
    if entries.is_empty() {
        return false;
    }
    let root = Path::new(s(&meta["worktree"], "source"));
    if !root.is_dir()
        || !entries.iter().all(|(path, expected)| {
            if !expected.is_object() {
                return false;
            }
            // Check cheap metadata before reading and hashing the file.
            let target = root.join(path);
            if s(expected, "mode") != "absent"
                && s(expected, "mode") != "120000"
                && !fs::symlink_metadata(&target).is_ok_and(|info| {
                    info.is_file()
                        && !info.file_type().is_symlink()
                        && Some(info.len()) == expected["size"].as_u64()
                        && entry_mode(&target).as_deref() == Some(s(expected, "mode"))
                })
            {
                return false;
            }
            final_entry(root, path).is_some_and(|actual| actual == *expected)
        })
    {
        return false;
    }
    let mut large_hashes = serde_json::Map::new();
    for (path, entry) in &entries {
        if rec["afterLarge"].get(path).is_some() {
            large_hashes.insert(path.clone(), entry["hash"].clone());
        }
    }
    write_json(run.join(".applied"), &json!({"at":iso(),"tree":rec["after"],"large":rec["afterLarge"],"largeHashes":large_hashes,"appliedBy":"detected"})).is_ok()
}
/// Active write runs whose source tree is `top`, as (name, in place). Used to
/// keep the source free for `apply` (field notes 22).
pub fn active_writes_on(top: &str, except: Option<&Path>) -> Vec<(String, bool)> {
    crate::runs::all_runs()
        .into_iter()
        .filter(|run| Some(run.as_path()) != except)
        .filter(|run| crate::runs::active(&crate::runs::state(run)))
        .filter_map(|run| {
            let meta = json(run.join("meta.json"));
            if s(&meta, "mode") != "write" {
                return None;
            }
            let in_place = s(&meta["worktree"], "path").is_empty();
            let source = Some(s(&meta["worktree"], "source"))
                .filter(|x| !x.is_empty())
                .unwrap_or(s(&meta, "top"));
            (!top.is_empty() && source == top).then(|| (s(&meta, "name").to_string(), in_place))
        })
        .collect()
}
/// Running write tasks on the same source (field notes 16): what each has
/// changed so far, and which of those paths the new task's prompt names, so
/// the caller can choose to serialize before the overlap becomes a conflict.
pub fn concurrent_writes(created: &Path) -> Vec<Value> {
    let meta = json(created.join("meta.json"));
    if s(&meta, "mode") != "write" {
        return vec![];
    }
    let source_of = |m: &Value| {
        Some(s(&m["worktree"], "source"))
            .filter(|x| !x.is_empty())
            .unwrap_or(s(m, "top"))
            .to_string()
    };
    let ours = source_of(&meta);
    if ours.is_empty() {
        return vec![];
    }
    let prompt = read(created.join("prompt.md"));
    let generic = [
        "mod.rs",
        "lib.rs",
        "main.rs",
        "index.ts",
        "index.tsx",
        "README.md",
        "Cargo.toml",
        "package.json",
    ];
    let named = |path: &str| {
        let base = path.rsplit('/').next().unwrap_or(path);
        prompt.contains(path)
            || (base.len() >= 8
                && base.contains('.')
                && !generic.contains(&base)
                && prompt.contains(base))
    };
    let mut found = vec![];
    for other in crate::runs::all_runs() {
        if other == created || !crate::runs::active(&crate::runs::state(&other)) {
            continue;
        }
        let theirs = json(other.join("meta.json"));
        if s(&theirs, "mode") != "write" || source_of(&theirs) != ours {
            continue;
        }
        let top = Some(s(&theirs["worktree"], "path"))
            .filter(|x| !x.is_empty())
            .unwrap_or(s(&theirs, "top"));
        let base = s(&theirs, "chainBase");
        let changed = if base.is_empty() {
            vec![]
        } else {
            snapshot(Path::new(top), &other, &strings(&theirs["snapshotExclude"]))
                .and_then(|now| {
                    tree_changes(Path::new(top), &json!({"tree":base}), &now, false).ok()
                })
                .unwrap_or_default()
                .iter()
                .map(|c| s(c, "path").to_string())
                .collect::<Vec<_>>()
        };
        let mentioned = changed
            .iter()
            .filter(|p| named(p))
            .cloned()
            .collect::<Vec<_>>();
        let mut entry = json!({"name":s(&theirs,"name"),"run":other.file_name().unwrap_or_default().to_string_lossy(),
            "changed":changed.len(),"sample":changed.iter().take(5).collect::<Vec<_>>(),"named":mentioned});
        if theirs["protect"].as_array().is_some_and(|p| !p.is_empty()) {
            entry["protect"] = theirs["protect"].clone();
        }
        found.push(entry);
    }
    found
}
/// How far the source moved since a finished, unapplied worktree run took its snapshot:
/// the number of changed files, and which of them the run changed too.
pub fn source_drift(run: &Path) -> Option<Value> {
    let meta = json(run.join("meta.json"));
    if !meta["worktree"].is_object() || s(&meta, "mode") != "write" || run.join(".applied").exists()
    {
        return None;
    }
    let (_, top, _, after) = chain_changes(run).ok()?;
    let source = PathBuf::from(s(&meta["worktree"], "source"));
    let applied = applied_state(run, &meta);
    let baseline = if !s(&applied, "tree").is_empty() {
        applied
    } else {
        json!({"tree":s(&meta,"chainBase"),"large":{}})
    };
    let ours = tree_changes(&top, &baseline, &json!({"tree":after}), false).ok()?;
    if ours.is_empty() || !source.is_dir() {
        return None;
    }
    let now = snapshot(&source, run, &strings(&meta["snapshotExclude"]))?;
    let moved = tree_changes(&source, &baseline, &now, false).ok()?;
    if moved.is_empty() {
        return None;
    }
    let overlap = moved
        .iter()
        .map(|c| s(c, "path"))
        .filter(|p| ours.iter().any(|c| s(c, "path") == *p))
        .collect::<Vec<_>>();
    let mut drift =
        json!({"files":moved.len(),"overlap":overlap.iter().take(10).collect::<Vec<_>>()});
    if overlap.len() > 10 {
        drift["overlapMore"] = json!(overlap.len() - 10);
    }
    Some(drift)
}
pub fn sync(run: &Path, meta: &Value) -> Res<Vec<String>> {
    let source = PathBuf::from(s(&meta["worktree"], "source"));
    let worktree = PathBuf::from(s(&meta["worktree"], "path"));
    let exclude = strings(&meta["snapshotExclude"]);
    let current_source = snapshot(&source, run, &exclude)
        .ok_or_else(|| format!("cannot snapshot {} for --sync", source.display()))?;
    let applied = applied_state(run, meta);
    let baseline = if !s(&applied, "tree").is_empty() {
        applied
    } else {
        json!({"tree":s(meta,"chainBase"),"large":{}})
    };
    let changes = tree_changes(&source, &baseline, &current_source, false)?;
    let mut actions = vec![];
    let mut conflicts = vec![];
    let mut paths = vec![];
    for change in changes {
        let path = s(&change, "path").to_string();
        let target = worktree.join(&path);
        if b(&change, "submodule") {
            if gitlink(&source, s(&baseline, "tree"), &path)?
                != gitlink(&source, s(&current_source, "tree"), &path)?
            {
                conflicts.push(path);
            }
            continue;
        }
        if through_symlink(&worktree, &target) || entry_mode(&target).as_deref() == Some("dir") {
            conflicts.push(path);
            continue;
        }
        if b(&change, "large") {
            let origin = source.join(&path);
            let content = current(&origin);
            if content == current(&target) {
                continue;
            }
            if baseline["large"].get(&path).is_some() || target.exists() || target.is_symlink() {
                conflicts.push(path);
            } else if let Some(content) = content {
                paths.push(path.clone());
                actions.push(Action {
                    kind: "copied".into(),
                    path,
                    target,
                    mode: entry_mode(&origin),
                    content,
                });
            } else {
                conflicts.push(path);
            }
            continue;
        }
        let (old_mode, old) = blob(&source, s(&baseline, "tree"), &path)?;
        let (mode, new) = blob(&source, s(&current_source, "tree"), &path)?;
        let now = current(&target);
        let now_mode = entry_mode(&target);
        let valid = |x: &Option<String>| {
            x.as_deref()
                .is_none_or(|v| ["100644", "100755", "120000"].contains(&v))
        };
        if !valid(&old_mode) || !valid(&mode) {
            conflicts.push(path);
        } else if now == new && now_mode == mode {
            continue;
        } else if now == old && (now_mode == old_mode || now_mode == mode) {
            paths.push(path.clone());
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
            let scratch = run.join(format!(".sync-merge-{}", std::process::id()));
            fs::create_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
            let minefile = scratch.join("mine");
            write(&minefile, mine)?;
            let basefile = scratch.join("base");
            write(&basefile, base)?;
            let theirfile = scratch.join("theirs");
            write(&theirfile, theirs)?;
            let output = Command::new("git")
                .arg("merge-file")
                .args(["-L", "worktree", "-L", "base", "-L", "caller"])
                .args([&minefile, &basefile, &theirfile])
                .output()
                .map_err(|e| e.to_string())?;
            let merged = fs::read(&minefile).map_err(|e| e.to_string())?;
            let _ = fs::remove_dir_all(scratch);
            if output.status.success() {
                paths.push(path.clone());
                actions.push(Action {
                    kind: "merged".into(),
                    path,
                    target,
                    mode: if old_mode != mode { mode } else { None },
                    content: merged,
                });
            } else {
                conflicts.push(path);
            }
        } else {
            conflicts.push(path);
        }
    }
    if !conflicts.is_empty() {
        return Err(format!("--sync conflict in: {}", conflicts.join(", ")));
    }
    for action in actions {
        if action.kind == "deleted" {
            fs::remove_file(&action.target).map_err(|e| e.to_string())?;
        } else if let Some(mode) = action.mode.as_deref() {
            write_entry(&action.target, mode, &action.content)?;
        } else {
            write(&action.target, &action.content)?;
        }
    }
    let mut large_hashes = serde_json::Map::new();
    if let Some(large) = current_source["large"].as_object() {
        for path in large.keys() {
            large_hashes.insert(path.clone(), json!(file_hash(&source.join(path))?));
        }
    }
    let state = json!({"at":iso(),"tree":current_source["tree"],"large":current_source["large"],"largeHashes":large_hashes});
    let mut chain = run.to_path_buf();
    loop {
        write_json(chain.join(".sync-base"), &state)?;
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
    Ok(paths)
}
pub struct ApplyOutcome {
    pub code: i32,
    pub conclusion: Value,
}
fn apply_progress(message: impl std::fmt::Display) {
    eprintln!("delegate: {message}");
    let _ = std::io::stderr().flush();
}
fn generation_markers(run: &Path, meta: &Value) -> Vec<PathBuf> {
    let mut markers = vec![run.join(".generate-pending")];
    let path = s(&meta["worktree"], "path");
    markers.extend(
        crate::runs::all_runs()
            .into_iter()
            .filter(|other| s(&json(other.join("meta.json"))["worktree"], "path") == path)
            .map(|other| other.join(".generate-pending")),
    );
    markers.sort();
    markers.dedup();
    markers
}
pub fn apply(run: &Path, merge: bool, dry: bool, keep_commits: bool) -> Res<ApplyOutcome> {
    apply_progress("checking merge / 正在检查合并");
    let source = s(&json(run.join("meta.json"))["worktree"], "source").to_string();
    for (name, _) in active_writes_on(&source, Some(run))
        .into_iter()
        .filter(|(_, in_place)| *in_place)
    {
        eprintln!(
            "delegate: warning: in-place write task {name} is editing this tree; its commits may sweep up what apply writes. Wait for it, or commit the applied paths by path / 警告：原地写入任务 {name} 正在改这个工作区，应用的改动可能被它一并提交"
        );
    }
    let mut conclusion = json!({"run":run.file_name().unwrap_or_default().to_string_lossy(),
        "operation":"apply","apply":{"ok":false,"dryRun":dry}});
    if let Some(sources) = json(run.join("meta.json")).get("configSources") {
        conclusion["configSources"] = sources.clone();
    }
    let meta = json(run.join("meta.json"));
    let source = Path::new(s(&meta["worktree"], "source"));
    let worktree = Path::new(s(&meta["worktree"], "path"));
    let base = read(run.join("worktree-base-commit")).trim().to_string();
    let commits = if !base.is_empty() && worktree.is_dir() {
        let log = git_text(worktree, &["log", "--reverse", "--format=%H%x09%s", &format!("{base}..HEAD")])?;
        log.lines().filter_map(|line| line.split_once('\t')).map(|(hash, title)| {
            let files = git_text(worktree, &["diff-tree", "--no-commit-id", "--name-only", "-r", hash])
                .unwrap_or_default().lines().count();
            json!({"hash":hash,"title":title,"files":files})
        }).collect::<Vec<_>>()
    } else { Vec::new() };
    conclusion["commits"] = json!(commits);
    let blind = crate::output::blind_files(&json(run.join("summary.json"))["files"], &meta["acceptBlind"]);
    if !blind.is_empty() { conclusion["acceptBlind"] = json!(blind); }
    if !crate::output::json_enabled() {
        for item in &commits {
            println!(" commit {} {} ({} files)", &s(item,"hash")[..7], s(item,"title"), n(item,"files"));
        }
    }
    let code = if keep_commits && !commits.is_empty() {
        if !git_text(worktree, &["status", "--porcelain", "--untracked-files=all"])?.is_empty() {
            return Err("--keep-commits requires committed worktree changes; commit remaining edits first".into());
        }
        if dry { 0 } else {
            let mut code = 0;
            for item in &commits {
                let status = Command::new("git").arg("-C").arg(source).args(["cherry-pick", "--no-edit", s(item,"hash")])
                    .status().map_err(|e| e.to_string())?;
                if !status.success() {
                    eprintln!("delegate: cherry-pick stopped at {}; resolve conflicts in {}, then continue with git cherry-pick --continue", s(item,"hash"), source.display());
                    code = 1;
                    break;
                }
            }
            if code == 0 {
                let state = json!({"at":iso(),"tree":s(&meta["base"],"tree")});
                write_json(run.join(".applied"), &state)?;
            }
            code
        }
    } else { apply_inner(run, merge, dry, &mut conclusion)? };
    if keep_commits { conclusion["apply"]["keepCommits"] = json!(true); }
    conclusion["apply"]["ok"] = json!(code == 0);
    accept_validity(run, dry, &mut conclusion);
    let marked = strings(&conclusion["apply"]["conflictMarkers"]);
    apply_progress(if code == 0 {
        "apply complete / 应用完成".to_string()
    } else if !marked.is_empty() {
        format!(
            "applied with conflict markers / 已应用，{} 个文件有冲突标记待解决: {}",
            marked.len(),
            marked.join(", ")
        )
    } else {
        "apply failed / 应用失败".to_string()
    });
    Ok(ApplyOutcome { code, conclusion })
}
fn accept_validity(run: &Path, dry: bool, conclusion: &mut Value) {
    let meta = json(run.join("meta.json"));
    let sum = json(run.join("summary.json"));
    let accept = &sum["accept"];
    let excluded = strings(&accept["excluded"]);
    if !excluded.is_empty() {
        conclusion["excluded"] = json!(excluded);
    }
    conclusion["acceptValidityScope"] = json!("repository-snapshot");
    let mut valid = None;
    let reason = if dry {
        "dry run does not establish the final repository snapshot".to_string()
    } else if s(&meta, "accept").is_empty() || !accept.is_object() {
        "no recorded acceptance for this run".into()
    } else if accept["ok"] == json!(false) {
        valid = Some(false);
        "acceptance failed".into()
    } else if accept["ok"] != json!(true)
        || s(accept, "tree").is_empty()
        || s(accept, "treeAfter").is_empty()
        || !b(accept, "snapshotComplete")
    {
        format!(
            "incomplete acceptance evidence: {}",
            accept["snapshotReason"]
                .as_str()
                .unwrap_or("missing historical snapshots (including old runs)")
        )
    } else if s(accept, "tree") != s(accept, "treeAfter") {
        valid = Some(false);
        "acceptance changed the repository tree".into()
    } else if let Some(source) = crate::changes::repository_snapshot_excluding(
        Path::new(s(&meta["worktree"], "source")),
        run,
        &crate::changes::managed_paths(&meta),
    ) {
        if !b(&source, "complete") {
            format!(
                "incomplete source evidence: {}",
                s(&source, "incompleteReason")
            )
        } else if s(&source, "tree") != s(accept, "tree") {
            valid = Some(false);
            "source repository tree differs from the accepted tree".into()
        } else if generation_markers(run, &meta)
            .iter()
            .any(|marker| marker.exists())
        {
            "regeneration is still pending".into()
        } else {
            valid = Some(true);
            "acceptance passed without changing its tree; final source tree matches (ignored files, environment, databases and Git history are outside this scope)".into()
        }
    } else {
        "source repository snapshot failed".into()
    };
    if let Some(valid) = valid {
        conclusion["acceptStillValid"] = json!(valid);
    }
    conclusion["acceptValidityReason"] = json!(reason);
}
/// Post-merge acceptance on the source tree (field notes 16): parallel routes
/// each pass in their own worktree, yet type drift between them only shows
/// once merged. Opt-in through `.delegate.json` `applyVerify` (true runs the
/// top-level `accept`; a string is its own command) or `apply --verify`.
/// Returns false only when the verification ran and failed.
pub fn verify_after_apply(run: &Path, forced: Option<bool>, conclusion: &mut Value) -> Res<bool> {
    if conclusion["apply"]["ok"] != json!(true) || conclusion["apply"]["dryRun"] == json!(true) {
        return Ok(true);
    }
    let meta = json(run.join("meta.json"));
    let tree = &meta["worktree"];
    let source = s(tree, "source");
    if source.is_empty() {
        return Ok(true);
    }
    let file = Path::new(source).join(".delegate.json");
    let raw = if file.is_file() {
        serde_json::from_str::<Value>(&read(&file))
            .map_err(|e| format!("cannot read {}: {e}", file.display()))?
    } else {
        json!({})
    };
    let configured = match raw.get("applyVerify") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => None,
        Some(Value::Bool(true)) => Some(None),
        Some(Value::String(command)) if !command.trim().is_empty() => Some(Some(command.clone())),
        _ => {
            return Err(format!(
                "{}: applyVerify must be true, false or a command",
                file.display()
            ))
        }
    };
    if !forced.unwrap_or(configured.is_some()) {
        return Ok(true);
    }
    if forced != Some(true) && conclusion["acceptStillValid"] == json!(true) {
        conclusion["verify"] = json!({"skipped":"acceptStillValid"});
        return Ok(true);
    }
    let command = configured
        .flatten()
        .or_else(|| {
            raw.get("accept")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| Some(s(&meta, "accept").to_string()).filter(|c| !c.is_empty()));
    let Some(command) = command else {
        conclusion["verify"] = json!({"skipped":"no acceptance command"});
        return Ok(true);
    };
    let cwd = Some(s(tree, "sourceWorkdir"))
        .filter(|w| !w.is_empty())
        .unwrap_or(source)
        .to_string();
    let label = format!(
        "verify {}",
        run.file_name().unwrap_or_default().to_string_lossy()
    );
    apply_progress("verifying merged tree / 正在验收合并结果");
    let slot = lane::acquire(&label, None, Some(run), |ahead, _| {
        apply_progress(format!("verify queued / 排队中: {ahead} ahead"))
    });
    let mut result = json!({"command":command});
    let Ok(slot) = slot else {
        result["ok"] = json!(false);
        result["tail"] = json!("stopped while queued for the heavy lane");
        conclusion["verify"] = result;
        return Ok(false);
    };
    let path = run.join("verify.log");
    let mut log = fs::File::create(&path).map_err(|e| e.to_string())?;
    let _ = writeln!(log, "$ {command}");
    let (code, timed) = crate::common::run_shell(
        (&command, "verify"),
        Path::new(&cwd),
        run,
        &meta["env"],
        meta["acceptTimeoutSeconds"].as_f64().unwrap_or(600.0),
        &mut log,
        None,
    )
    .unwrap_or((1, false));
    drop(slot);
    let _ = writeln!(
        log,
        "{}\n[exit {code}]",
        if timed { "\n[verify timed out]" } else { "" }
    );
    result["ok"] = json!(code == 0);
    result["exitCode"] = json!(code);
    result["log"] = json!(path);
    if code != 0 {
        let text = read(&path);
        let chars: Vec<char> = text.trim().chars().collect();
        result["tail"] = json!(chars[chars.len().saturating_sub(1500)..]
            .iter()
            .collect::<String>());
        result["next"] = json!("the merge is already in the source tree: fix it there, or reply to the task with the failure");
    }
    apply_progress(if code == 0 {
        "verify passed / 验收通过"
    } else {
        "verify failed / 验收失败"
    });
    conclusion["verify"] = result;
    Ok(code == 0)
}
fn numbered_prefix(path: &str) -> Option<(String, String)> {
    let path = Path::new(path);
    let name = path.file_name()?.to_str()?;
    let (prefix, _) = name.split_once('_')?;
    if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_string_lossy()
            .into(),
        prefix.into(),
    ))
}
fn numbered_conflicts(
    source: &Path,
    additions: &[String],
    actions: &[Action],
    generated: &[String],
) -> Res<Vec<Value>> {
    use std::collections::{BTreeMap, BTreeSet};
    let deleted = actions
        .iter()
        .filter(|a| a.kind == "deleted")
        .map(|a| a.path.as_str())
        .collect::<BTreeSet<_>>();
    let mut groups = BTreeMap::<(String, String), BTreeSet<String>>::new();
    for path in additions {
        if let Some(key) = numbered_prefix(path) {
            groups.entry(key).or_default().insert(path.clone());
        }
    }
    for ((directory, prefix), paths) in &mut groups {
        let parent = source.join(directory);
        if through_symlink(source, &parent) || parent.is_symlink() {
            continue;
        }
        if parent.is_dir() {
            for entry in fs::read_dir(&parent).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                    continue;
                }
                let path = entry
                    .path()
                    .strip_prefix(source)
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into_owned();
                if deleted.contains(path.as_str()) || matches_rule(&path, generated) {
                    continue;
                }
                if numbered_prefix(&path).as_ref() == Some(&(directory.clone(), prefix.clone())) {
                    paths.insert(path);
                }
            }
        }
    }
    Ok(groups
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|((directory, prefix), paths)| {
            apply_progress(format!(
                "numbered prefix warning / 编号前缀冲突: {directory} [{prefix}]: {}",
                paths.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
            json!({"directory":directory,"prefix":prefix,"paths":paths})
        })
        .collect())
}
fn apply_inner(run: &Path, merge: bool, dry: bool, conclusion: &mut Value) -> Res<i32> {
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
    let applied = applied_state(run, &meta);
    let mut before = s(&applied, "tree");
    if before.is_empty() {
        before = s(&meta, "chainBase");
    }
    let mut before = before.to_string();
    let source = PathBuf::from(s(&meta["worktree"], "source"));
    let (generated_paths, generate_command) = generated(&source)?;
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
    let mut changes = tree_changes(
        &top,
        &json!({"tree":before,"large":applied["large"]}),
        &json!({"tree":after,"large":large}),
        false,
    )?;
    if changes.is_empty() && !s(&applied, "tree").is_empty() {
        // A repeated apply still detects drift in files merged in an earlier round.
        before = s(&meta, "chainBase").to_string();
        changes = tree_changes(&top, &json!({"tree":before}), &json!({"tree":after}), false)?;
    }
    let mut actions = vec![];
    let mut conflicts = vec![];
    let mut additions = vec![];
    for c in changes {
        if b(&c, "large") {
            continue;
        }
        let path = s(&c, "path").to_string();
        if matches_rule(&path, &generated_paths) {
            continue;
        }
        let target = source.join(&path);
        let (old_mode, old) = blob(&top, &before, &path)?;
        let (mode, new) = blob(&top, &after, &path)?;
        if old_mode.is_none() && mode.is_some() {
            additions.push(path.clone());
        }
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
            write(&minefile, mine).map_err(|e| format!("{}: {e}", minefile.display()))?;
            write(&basefile, base).map_err(|e| format!("{}: {e}", basefile.display()))?;
            write(&theirfile, theirs).map_err(|e| format!("{}: {e}", theirfile.display()))?;
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
            let merged = fs::read(&minefile).map_err(|e| format!("{}: {e}", minefile.display()))?;
            fs::remove_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
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
            if matches_rule(path, &generated_paths) {
                continue;
            }
            let origin = worktree.join(path);
            if applied["large"].get(path).is_none() && blob(&top, &before, path)?.0.is_none() {
                additions.push(path.clone());
            }
            let target = source.join(path);
            if !origin.is_file()
                || through_symlink(&source, &target)
                || target.is_symlink()
                || entry_mode(&target).as_deref() == Some("dir")
            {
                conflicts.push(path.clone());
            } else if !s(&applied, "tree").is_empty()
                && before != s(&meta, "chainBase")
                && applied["large"].get(path) == obj.get(path)
                && applied["largeHashes"].get(path).and_then(Value::as_str)
                    == Some(file_hash(&origin)?.as_str())
            {
                continue;
            } else if !target.exists()
                || applied["largeHashes"].get(path).and_then(Value::as_str)
                    == Some(file_hash(&target)?.as_str())
            {
                actions.push(Action {
                    kind: "copied".into(),
                    path: path.clone(),
                    target,
                    mode: Some("large".into()),
                    content: fs::read(&origin).map_err(|e| format!("{}: {e}", origin.display()))?,
                });
            } else if current(&target)
                != Some(fs::read(&origin).map_err(|e| format!("{}: {e}", origin.display()))?)
            {
                conflicts.push(path.clone());
            }
        }
    }
    additions.retain(|path| {
        actions
            .iter()
            .any(|a| &a.path == path && a.kind != "deleted")
    });
    let warnings = numbered_conflicts(&source, &additions, &actions, &generated_paths)?;
    if !warnings.is_empty() {
        conclusion["numberedPrefixConflicts"] = json!(warnings);
    }
    let markers = generation_markers(run, &meta);
    let pending_generation = markers.iter().any(|marker| marker.exists());
    if pending_generation && (generated_paths.is_empty() || generate_command.is_empty()) {
        return Err("pending regeneration requires generated.paths and generated.command".into());
    }
    if actions.is_empty() && conflicts.is_empty() && !pending_generation {
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
    let marked_paths = actions
        .iter()
        .filter(|x| x.kind == "conflict-markers")
        .map(|x| x.path.clone())
        .collect::<Vec<_>>();
    let marked = !marked_paths.is_empty();
    if marked && !dry {
        conclusion["apply"]["conflictMarkers"] = json!(marked_paths);
    }
    let generated_inputs = generated_inputs(&source)?;
    let touches_inputs = generated_inputs.is_empty()
        || actions
            .iter()
            .any(|a| matches_rule(&a.path, &generated_inputs));
    let regenerate = ((!actions.is_empty() && touches_inputs) || pending_generation)
        && !generated_paths.is_empty()
        && !generate_command.is_empty();
    let skipped_generation = !actions.is_empty()
        && !touches_inputs
        && !pending_generation
        && !generated_paths.is_empty();
    if regenerate && !dry {
        write(run.join(".generate-pending"), "pending\n")?;
    }
    for a in actions {
        if !dry {
            let operation = match a.kind.as_str() {
                "deleted" => fs::remove_file(&a.target)
                    .map_err(|e| e.to_string())
                    .map(|()| remove_empty_parents(&a.target, &source)),
                "copied" => {
                    fs::create_dir_all(a.target.parent().unwrap_or(&source))
                        .map_err(|e| format!("apply {}: {e}", a.target.display()))?;
                    write(&a.target, &a.content)
                }
                _ => {
                    if let Some(mode) = a.mode.as_deref() {
                        write_entry(&a.target, mode, &a.content)
                    } else {
                        write(&a.target, &a.content)
                    }
                }
            };
            operation.map_err(|e| format!("apply {}: {e}", a.target.display()))?;
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
    if skipped_generation {
        println!(
            " {:<16} no merged path under generated.inputs",
            "not regenerated"
        );
    }
    if dry {
        eprintln!("delegate: dry run; nothing written");
        if regenerate {
            println!(" regenerated      {} (dry run)", generated_paths.join(", "));
        }
    } else if regenerate {
        let _slot = lane::acquire(
            &format!(
                "regenerate {}",
                run.file_name().unwrap_or_default().to_string_lossy()
            ),
            None,
            Some(run),
            |ahead, labels| {
                apply_progress(format!(
                    "generator queued / 生成器排队: {ahead} ahead; {}",
                    labels.join(", ")
                ))
            },
        )?;
        let log = std::path::absolute(run.join("generate.log")).map_err(|e| e.to_string())?;
        let mut file =
            fs::File::create(&log).map_err(|e| format!("apply {}: {e}", log.display()))?;
        let timeout = seconds(&setting("GENERATE_TIMEOUT", "10m"))?;
        apply_progress(format!(
            "generating / 正在生成; log: {}; execution timeout: {timeout}s (queue excluded)",
            log.display()
        ));
        let (code, timed) = run_shell(
            (&generate_command, "generate"),
            &source,
            run,
            &meta["env"],
            timeout,
            &mut file,
            None,
        )
        .map_err(|e| format!("apply generated.command: {e}"))?;
        writeln!(file, "\n[exit {code}]").map_err(|e| format!("apply {}: {e}", log.display()))?;
        if code != 0 {
            let body = fs::read(&log).map_err(|e| format!("apply {}: {e}", log.display()))?;
            let tail = String::from_utf8_lossy(&body)
                .chars()
                .rev()
                .take(1500)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();
            eprintln!(
                "delegate: regeneration {} (exit {code}); {}\n{}",
                if timed { "timed out" } else { "failed" },
                log.display(),
                tail
            );
            return Ok(1);
        }
        for marker in markers.iter().filter(|marker| marker.exists()) {
            fs::remove_file(marker).map_err(|e| e.to_string())?;
        }
        apply_progress(format!(
            "generation complete / 生成完成; log: {}",
            log.display()
        ));
        println!(" regenerated      {}", generated_paths.join(", "));
    }
    // Conflict markers still land every change, so record it; only skipped files keep the run unapplied.
    if !dry && conflicts.is_empty() {
        let mut large_hashes = serde_json::Map::new();
        if let Some(obj) = large.as_object() {
            for path in obj.keys() {
                if !matches_rule(path, &generated_paths) {
                    large_hashes.insert(path.clone(), json!(file_hash(&worktree.join(path))?));
                }
            }
        }
        let state = json!({"at":iso(),"tree":after,"large":large,"largeHashes":large_hashes});
        let mut chain = run.to_path_buf();
        loop {
            write_json(chain.join(".applied"), &state)?;
            write_json(chain.join(".sync-base"), &state)?;
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
