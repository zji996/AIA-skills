use crate::changes::{chain_changes, gitlink, snapshot, tree_changes};
use crate::common::*;
use crate::lane;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

pub fn has_node_modules(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "node_modules")
}
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
        let normalized: Vec<String> = items
            .iter()
            .map(|x| if key.ends_with("etup") {
                x.clone()
            } else {
                x.trim().trim_end_matches('/').to_string()
            })
            .collect();
        if key == "link" {
            if let Some(bad) = normalized.iter().find(|x| has_node_modules(Path::new(x))) {
                return Err(format!(
                    "{}: worktree.link must not link node_modules ({bad}); package managers follow the symlink and overwrite the source checkout; use worktree.setup instead",
                    path.display()
                ));
            }
        }
        out[key] = json!(normalized);
    }
    match work.get("pool") {
        None | Some(Value::Null) => {}
        Some(value) => match value.as_u64() {
            Some(n) if n <= crate::pool::MAX_SLOTS => out["pool"] = json!(n),
            _ => {
                return Err(format!(
                    "{}: worktree.pool must be a number of slots from 0 to {}",
                    path.display(),
                    crate::pool::MAX_SLOTS
                ))
            }
        },
    }
    match work.get("cleanOnApply") {
        None | Some(Value::Null) => out["cleanOnApply"] = json!(true),
        Some(Value::Bool(flag)) => out["cleanOnApply"] = json!(*flag),
        Some(_) => {
            return Err(format!(
                "{}: worktree.cleanOnApply must be a boolean",
                path.display()
            ))
        }
    }
    Ok(out)
}
pub fn clean_on_apply(run: &Path) -> bool {
    let meta = json(run.join("meta.json"));
    let source = s(&meta["worktree"], "source");
    if !source.is_empty() {
        let file = Path::new(source).join(".delegate.json");
        if let Ok(Some(cfg)) = crate::config::read_file(&file) {
            if let Some(flag) = cfg["worktree"]["cleanOnApply"].as_bool() {
                return flag;
            }
        }
    }
    meta["worktree"]["config"]["cleanOnApply"]
        .as_bool()
        .unwrap_or(true)
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
/// Optional `appendOnly` paths: files both sides only add sections to (benchmark logs,
/// changelogs). A conflict there keeps both sides, current first, instead of stopping.
pub fn append_only(source: &Path) -> Res<Vec<String>> {
    let file = source.join(".delegate.json");
    let cfg = crate::config::read_file(&file)?.unwrap_or(json!({}));
    append_only_config(&cfg, &file)
}
pub fn append_only_config(cfg: &Value, file: &Path) -> Res<Vec<String>> {
    if cfg.get("appendOnly").is_none() {
        return Ok(vec![]);
    }
    rule_list(cfg, "appendOnly", file)
        .map(Option::unwrap_or_default)
        .map_err(|e| e.replace("generated.appendOnly", "appendOnly"))
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
pub(crate) fn remove_empty_parents(file: &Path, root: &Path) {
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
pub(crate) fn copy_rec(src: &Path, dst: &Path) -> Res<()> {
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
    let reconstructed = !s(tree, "reconstructedTree").is_empty();
    let head = if reconstructed {
        let rb = s(tree, "reconstructedBase");
        if !rb.is_empty() {
            git_text(&source, &["rev-parse", "-q", "--verify", &format!("{rb}^{{commit}}")])
                .unwrap_or_default()
        } else {
            String::new()
        }
    } else {
        git_text(&source, &["rev-parse", "-q", "--verify", "HEAD^{commit}"]).unwrap_or_default()
    };
    let mut c = git_command();
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
    crate::refs::pin_slot(run, meta, "worktree-base", &commit);
    let managed: Vec<String> = strings(&cfg["copy"])
        .into_iter()
        .chain(strings(&cfg["link"]))
        .collect();
    let reused = if b(tree, "pooled")
        && tree["claimedBy"].as_str() == run.file_name().and_then(|n| n.to_str())
    {
        match crate::pool::reset(&path, &commit, &managed) {
            Ok(reused) => reused,
            Err(e) => return Ok(Some(format!("worktree setup failed: {}", clip(&e, 300)))),
        }
    } else {
        false
    };
    if reused {
        eprintln!("delegate: reused pool slot {}", path.display());
    } else {
        if b(tree, "pooled") {
            // A slot removed by hand may still be registered; prune so add can take the path.
            let _ = git(&source, &["worktree", "prune"]);
        }
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
        if b(tree, "pooled") {
            if let Err(e) = crate::pool::attach_target(&path) {
                return Ok(Some(format!("worktree setup failed: {}", clip(&e, 300))));
            }
        }
    }
    if s(meta, "mode") == "read-only" && !head.is_empty() {
        let _ = git(&path, &["reset", "-q", "--soft", &head]);
    }
    if reconstructed {
        let arch_str = s(tree, "reconstructedArchive");
        if !arch_str.is_empty() {
            let arch_dir = Path::new(arch_str);
            let manifest = json(arch_dir.join("manifest.json"));
            for rel in strings(&manifest["files"]) {
                let rel_path = Path::new(&rel);
                if rel_path.is_absolute()
                    || !rel_path
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_)))
                {
                    continue;
                }
                let src = arch_dir.join("files").join(rel_path);
                let dst = path.join(rel_path);
                if src.is_file() && !dst.exists() && !dst.is_symlink() {
                    if let Some(parent) = dst.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::copy(&src, &dst);
                }
            }
        }
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
        let Ok(relative) = Path::new(&item).strip_prefix(&source) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = path.join(relative);
        if target.exists() || target.is_symlink() {
            return Err(format!(
                "--share target already exists: {}",
                target.display()
            ));
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
    let setup_cmds: Vec<&str> = setup.iter().map(String::as_str).collect();
    let _slot = lane::acquire_shell_for_commands(
        &format!(
            "setup {}",
            run.file_name().unwrap_or_default().to_string_lossy()
        ),
        &setup_cmds,
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
            Some(timeout),
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
    if crate::pool::is_slot(path) {
        // The slot keeps its files and build cache; the next claim resets it.
        crate::pool::release(path, Path::new(s(meta, "dir")));
        return containers;
    }
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
pub(crate) fn entry_mode(path: &Path) -> Option<String> {
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
pub(crate) fn through_symlink(root: &Path, path: &Path) -> bool {
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
pub(crate) fn write_entry(path: &Path, mode: &str, content: &[u8]) -> Res<()> {
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

/// Resolve the target file mode when applying or merging a change onto a target file:
/// - If the incoming side explicitly changed mode (`old_mode != incoming_mode`), use `incoming_mode`.
/// - Otherwise preserve the target file's current mode (`now_mode`), falling back to `incoming_mode`/`old_mode`.
fn resolved_mode(
    old_mode: Option<&str>,
    incoming_mode: Option<&str>,
    now_mode: Option<&str>,
) -> Option<String> {
    let chosen = if old_mode != incoming_mode {
        incoming_mode.or(now_mode)
    } else {
        now_mode.or(incoming_mode).or(old_mode)
    };
    chosen.map(str::to_string)
}

#[derive(Clone)]
struct Action {
    kind: String,
    path: String,
    target: PathBuf,
    mode: Option<String>,
    content: Vec<u8>,
}
pub fn effective_total_base(top: &Path, meta: &Value) -> (String, &'static str) {
    if let Some(tree) = crate::changes::net_base_tree(top, meta) {
        return (tree, "net-base");
    }
    (s(meta, "chainBase").to_string(), "chain-base")
}

fn net_base_state(meta: &Value) -> Option<Value> {
    let top_str = Some(s(&meta["worktree"], "path"))
        .filter(|p| !p.is_empty() && Path::new(p).is_dir())
        .or_else(|| {
            Some(s(&meta["worktree"], "source"))
                .filter(|p| !p.is_empty() && Path::new(p).is_dir())
        })
        .unwrap_or_else(|| s(meta, "top"));
    crate::changes::net_base_tree(Path::new(top_str), meta)
        .map(|tree| json!({"tree": tree, "large": {}}))
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
    if b(meta, "netBaseAdvanced") {
        if let Some(state) = net_base_state(meta) {
            return state;
        }
    }
    let mut parent = s(meta, "parent").to_string();
    let mut visited = std::collections::BTreeSet::new();
    while !parent.is_empty() && visited.insert(parent.clone()) {
        let path = run.parent().unwrap_or(Path::new("/")).join(&parent);
        let state = json(path.join(".sync-base"));
        if !s(&state, "tree").is_empty() {
            return state;
        }
        let state = json(path.join(".applied"));
        if !s(&state, "tree").is_empty() {
            return state;
        }
        let parent_meta = json(path.join("meta.json"));
        if b(&parent_meta, "netBaseAdvanced") {
            if let Some(state) = net_base_state(&parent_meta) {
                return state;
            }
        }
        parent = s(&parent_meta, "parent").to_string();
    }
    if !s(&meta["appliedBase"], "tree").is_empty() {
        return meta["appliedBase"].clone();
    }
    if let Some(state) = net_base_state(meta) {
        return state;
    }
    Value::Null
}

pub fn effective_chain_base(run: &Path, meta: &Value, top: &Path) -> Value {
    let base = applied_state(run, meta);
    if !s(&base, "tree").is_empty() {
        return base;
    }
    let (tree, _) = effective_total_base(top, meta);
    json!({"tree": tree, "large": {}})
}

pub fn pending_changes(run: &Path, meta: &Value) -> Option<Value> {
    if !meta["worktree"].is_object() || s(meta, "mode") != "write" {
        return None;
    }
    let rec = json(run.join("changes.json"));
    let top = Path::new(s(&rec, "top"));
    let base = effective_chain_base(run, meta, top);
    let after = json!({"tree":rec["after"],"large":rec["afterLarge"]});
    let changes = tree_changes(top, &base, &after, true).ok()?;
    Some(
        json!({"files":changes.len(),"added":changes.iter().map(|c| n(c,"added")).sum::<i64>(),"deleted":changes.iter().map(|c| n(c,"deleted")).sum::<i64>()}),
    )
}
/// Net changed paths of this worktree chain relative to its effective chain baseline
/// (`chainBase` / `.sync-base` / `netBase`), excluding unrelated source-branch advances.
pub fn pending_changed_paths(run: &Path, meta: &Value) -> Option<Vec<String>> {
    if !meta["worktree"].is_object() || s(meta, "mode") != "write" {
        return None;
    }
    let rec = json(run.join("changes.json"));
    let top = Path::new(s(&rec, "top"));
    let base = effective_chain_base(run, meta, top);
    let after = json!({"tree":rec["after"],"large":rec["afterLarge"]});
    let changes = tree_changes(top, &base, &after, true).ok()?;
    Some(changes.iter().map(|c| s(c, "path").to_string()).collect())
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
    if let Some(wt_head) = read_git_head(root) {
        let base_head = s(meta, "netBase");
        let base_commit = read(run.join("worktree-base-commit"));
        let base_commit = base_commit.trim();
        if wt_head != base_head && wt_head != base_commit {
            rec["worktreeHead"] = json!(wt_head);
        }
    }
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
    if crate::locks::holding_state_lock() {
        return None;
    }
    let top = {
        let p = Path::new(s(rec, "top"));
        if p.is_dir() {
            p
        } else {
            Path::new(s(&meta["worktree"], "source"))
        }
    };
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
fn entries_match_working_tree(
    root: &Path,
    entries: &serde_json::Map<String, Value>,
) -> bool {
    if !root.is_dir() || entries.is_empty() {
        return false;
    }
    entries.iter().all(|(path, expected)| {
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
}

fn entries_match_head(root: &Path, entries: &serde_json::Map<String, Value>) -> bool {
    if !root.is_dir() || entries.is_empty() || crate::locks::holding_state_lock() {
        return false;
    }
    entries.iter().all(|(path, expected)| {
        if !expected.is_object() {
            return false;
        }
        let Ok((mode, bytes)) = blob(root, "HEAD", path) else {
            return false;
        };
        if s(expected, "mode") == "absent" {
            return mode.is_none();
        }
        let (Some(mode), Some(bytes)) = (mode, bytes) else {
            return false;
        };
        let hash = sha1_smol::Sha1::from(&bytes).digest().to_string();
        mode == s(expected, "mode")
            && Some(bytes.len() as u64) == expected["size"].as_u64()
            && hash == s(expected, "hash")
    })
}

fn commit_contained_in_head(root: &Path, wt_head: &str, base_head: &str) -> bool {
    if !root.is_dir() || wt_head.is_empty() || crate::locks::holding_state_lock() {
        return false;
    }
    // 1. Direct merge / fast-forward: wt_head is an ancestor of HEAD.
    if git(root, &["merge-base", "--is-ancestor", wt_head, "HEAD"]).is_ok() {
        return true;
    }
    // 2. Cherry-pick / rebase: every commit in base_head..wt_head has patch-id in HEAD.
    if !base_head.is_empty() && base_head != wt_head {
        if let Ok(out) = git_text(root, &["cherry", "HEAD", wt_head, base_head]) {
            let mut saw_minus = false;
            let mut saw_plus = false;
            for line in out.lines() {
                if line.starts_with("- ") {
                    saw_minus = true;
                } else if line.starts_with("+ ") {
                    saw_plus = true;
                }
            }
            if saw_minus && !saw_plus {
                return true;
            }
        }
    }
    false
}

/// Strictly read-only check: are this run's changes already present in the source repository
/// (matching working tree, matching HEAD tree, or worktree commits contained in HEAD)?
/// Never writes files or acquires locks.
pub fn is_applied_in_repo_readonly(run: &Path, meta: &Value) -> bool {
    if run.join(".applied").exists() || run.join(".applied-cleaned").exists() {
        return true;
    }
    if !s(meta, "appliedAt").is_empty() {
        return true;
    }
    if s(meta, "mode") != "write"
        || !meta["worktree"].is_object()
        || !run.join("exit_code").is_file()
        || run.join(".generate-pending").exists()
    {
        return false;
    }
    let root = Path::new(s(&meta["worktree"], "source"));
    if !root.is_dir() {
        return false;
    }
    let rec = json(run.join("changes.json"));
    let wt_head = {
        let recorded = s(&rec, "worktreeHead").to_string();
        if !recorded.is_empty() {
            recorded
        } else {
            let wt_dir = Path::new(s(&meta["worktree"], "path"));
            read_git_head(wt_dir)
                .filter(|h| h != s(meta, "netBase") && h != read(run.join("worktree-base-commit")).trim())
                .unwrap_or_default()
        }
    };
    let base_head = {
        let wt_base = read(run.join("worktree-base-commit")).trim().to_string();
        if !wt_base.is_empty() {
            wt_base
        } else {
            s(meta, "netBase").to_string()
        }
    };
    if !wt_head.is_empty() && commit_contained_in_head(root, &wt_head, &base_head) {
        return true;
    }
    let Some(entries) = recorded_entries(run, meta, &rec) else {
        return false;
    };
    if entries.is_empty() {
        return false;
    }
    entries_match_working_tree(root, &entries) || entries_match_head(root, &entries)
}

/// Fail-closed check for `_gc-worker`: returns true only if an `.applied-cleaned` run's
/// applied changes are now committed in source `HEAD` (HEAD has advanced past the uncommitted
/// apply's `head` AND all recorded changed entries match `HEAD`).
pub fn is_applied_committed_in_head(run: &Path, meta: &Value) -> bool {
    if !run.join(".applied-cleaned").exists() || !run.join(".applied").exists() {
        return false;
    }
    if s(meta, "mode") != "write" || !meta["worktree"].is_object() {
        return false;
    }
    let root = Path::new(s(&meta["worktree"], "source"));
    if !root.is_dir() {
        return false;
    }
    let applied = json(run.join(".applied"));
    let applied_head = s(&applied, "head");
    let Some(current_head) = read_git_head(root) else {
        return false;
    };
    if current_head.is_empty() || (!applied_head.is_empty() && current_head == applied_head) {
        return false;
    }
    let rec = json(run.join("changes.json"));
    if !rec.is_object() {
        return false;
    }
    let entries = if let Some(fe) = rec["finalEntries"].as_object() {
        fe.clone()
    } else {
        // After apply, `.applied` already records `after` and the worktree may be gone;
        // reconstruct expected entries directly from `rec["changes"]` + pinned `after` tree in `root`.
        let after = s(&rec, "after");
        if after.is_empty() {
            return false;
        }
        let Some(changes) = rec["changes"].as_array() else {
            return false;
        };
        let mut map = serde_json::Map::new();
        for change in changes {
            let path = s(change, "path");
            if path.is_empty() || b(change, "large") || b(change, "submodule") {
                return false;
            }
            let Ok((mode, content)) = blob(root, after, path) else {
                return false;
            };
            let entry = if let (Some(mode), Some(bytes)) = (mode, content) {
                json!({
                    "mode": mode,
                    "size": bytes.len(),
                    "hash": sha1_smol::Sha1::from(&bytes).digest().to_string()
                })
            } else {
                json!({"mode": "absent"})
            };
            map.insert(path.to_string(), entry);
        }
        map
    };
    if entries.is_empty() {
        return false;
    }
    entries_match_head(root, &entries)
}

/// Write-path detection: checks if `run`'s changes are already in the source repo (working tree
/// or HEAD), and if so persists `.applied`, updates `meta.json`, history, events, work table,
/// and board. Must ONLY be called from write paths (`clean`, `apply`, `merge`, workers) — never
/// from read paths (`status`, `work`, `events`, `snapshot`).
pub fn detect_applied(run: &Path, meta: &Value) -> bool {
    if run.join(".applied").exists() {
        return true;
    }
    if !is_applied_in_repo_readonly(run, meta) {
        return false;
    }
    let rec = json(run.join("changes.json"));
    let entries = recorded_entries(run, meta, &rec).unwrap_or_default();
    let root = Path::new(s(&meta["worktree"], "source"));
    let mut large_hashes = serde_json::Map::new();
    for (path, entry) in &entries {
        if rec["afterLarge"].get(path).is_some() {
            large_hashes.insert(path.clone(), entry["hash"].clone());
        }
    }
    let at = iso();
    let head = read_git_head(root)
        .or_else(|| git_text(root, &["rev-parse", "HEAD"]).ok())
        .unwrap_or_default();
    let ok = write_json(
        run.join(".applied"),
        &json!({"at":at,"head":head,"tree":rec["after"],"large":rec["afterLarge"],"largeHashes":large_hashes,"appliedBy":"detected"}),
    )
    .is_ok();
    if ok {
        let mut updated = if run.join("meta.json").is_file() && !crate::archive::is_record(run) {
            let mut m = json(run.join("meta.json"));
            m["appliedAt"] = json!(at);
            if !head.is_empty() {
                m["appliedHead"] = json!(head);
            }
            let _ = write_json(run.join("meta.json"), &m);
            m
        } else {
            meta.clone()
        };
        if s(&updated, "appliedAt").is_empty() {
            updated["appliedAt"] = json!(at);
        }
        crate::refs::pin_slot(run, &updated, "applied", s(&rec, "after"));
        crate::history::record_apply(run, &updated, &at, &head, "detected", None);
        crate::events::emit(
            &crate::events::run_id_for(run, &updated),
            &crate::events::repo_for_meta(&updated),
            "applied",
            json!({
                "name": s(&updated, "name"),
                "appliedBy": "detected",
                "head": head,
            }),
        );
        let applied_doc = json(run.join(".applied"));
        let _ = crate::work::on_runs_applied(
            &[(run.to_path_buf(), updated.clone(), applied_doc)],
            None,
        );
        crate::board::refresh_for_run(run);
    }
    ok
}

pub struct MarkAppliedResult {
    pub marked_runs: Vec<(PathBuf, Value, Value)>,
    pub source: PathBuf,
    pub commit_sha: String,
    pub applied_at: String,
    pub checked_files: Vec<String>,
    pub missing_files: Vec<String>,
}

/// Manually mark a run (and any unapplied same-worktree ancestors) as applied against a verified
/// commit in the source repository (defaulting to source `HEAD`).
pub fn mark_applied(
    run: &Path,
    commit_arg: Option<&str>,
    check_files: bool,
) -> Res<MarkAppliedResult> {
    use std::collections::BTreeSet;
    let meta = json(run.join("meta.json"));
    if !meta.is_object() {
        return Err(format!("run metadata not found: {}", run.display()));
    }
    let source_str = Some(s(&meta["worktree"], "source"))
        .filter(|x| !x.is_empty())
        .or_else(|| Some(s(&meta, "top")).filter(|x| !x.is_empty()))
        .or_else(|| Some(s(&meta, "workdir")).filter(|x| !x.is_empty()))
        .ok_or_else(|| "cannot determine source repository for run".to_string())?;
    let source = PathBuf::from(source_str);
    if !source.is_dir() {
        return Err(format!(
            "source repository does not exist: {}",
            source.display()
        ));
    }

    let commit_sha = if let Some(raw_sha) = commit_arg {
        let trimmed = raw_sha.trim();
        if trimmed.is_empty() {
            return Err("--commit requires a non-empty commit SHA".into());
        }
        git_text(
            &source,
            &["rev-parse", "--verify", &format!("{trimmed}^{{commit}}")],
        )
        .map_err(|_| {
            format!(
                "commit '{trimmed}' not found in source repository {}",
                source.display()
            )
        })?
    } else {
        git_text(&source, &["rev-parse", "--verify", "HEAD^{commit}"]).map_err(|_| {
            format!(
                "cannot resolve HEAD commit in source repository {}",
                source.display()
            )
        })?
    };

    let checked_files: Vec<String> = pending_changed_paths(run, &meta)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| run_files(run));

    let commit_files: BTreeSet<String> = git_text(
        &source,
        &[
            "log",
            "-1",
            "--first-parent",
            "-m",
            "--root",
            "--name-only",
            "--pretty=format:",
            &commit_sha,
        ],
    )
    .map(|out| {
        out.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default();

    let missing_files: Vec<String> = checked_files
        .iter()
        .filter(|p| !commit_files.contains(p.as_str()))
        .cloned()
        .collect();

    if check_files && !missing_files.is_empty() {
        let short = &commit_sha[..commit_sha.len().min(7)];
        return Err(format!(
            "commit {short} does not contain all net changed files of this run (missing: {})",
            missing_files.join(", ")
        ));
    }

    // Collect this run and any unapplied ancestors sharing the same worktree.
    let wt_path = s(&meta["worktree"], "path").to_string();
    let mut chain: Vec<PathBuf> = vec![run.to_path_buf()];
    let mut cur = run.to_path_buf();
    let mut cur_meta = meta.clone();
    let mut seen = BTreeSet::new();
    seen.insert(crate::events::run_id_for(run, &meta));
    while !s(&cur_meta, "parent").is_empty() {
        let parent_id = s(&cur_meta, "parent").to_string();
        if !seen.insert(parent_id.clone()) {
            break;
        }
        let Some(pdir) = cur.parent().map(|d| d.join(&parent_id)) else {
            break;
        };
        if !pdir.join("meta.json").is_file() || pdir.join(".applied").exists() {
            break;
        }
        let pmeta = json(pdir.join("meta.json"));
        if !wt_path.is_empty() && s(&pmeta["worktree"], "path") != wt_path {
            break;
        }
        chain.push(pdir.clone());
        cur = pdir;
        cur_meta = pmeta;
    }

    let at = iso();
    let mut marked_runs = Vec::new();
    for rdir in chain {
        let _ = crate::premerge::cancel_for_run(&rdir, &source);
        let _ = fs::remove_file(crate::premerge::premerge_path(&rdir));
        let rec = json(rdir.join("changes.json"));
        let applied_doc = json!({
            "at": at,
            "head": commit_sha,
            "commit": commit_sha,
            "tree": rec["after"],
            "large": rec["afterLarge"],
            "appliedBy": "manual",
        });
        write_json(rdir.join(".applied"), &applied_doc)?;
        let updated_meta = if rdir.join("meta.json").is_file() && !crate::archive::is_record(&rdir) {
            let mut m = json(rdir.join("meta.json"));
            m["appliedAt"] = json!(at);
            m["appliedHead"] = json!(commit_sha);
            let _ = write_json(rdir.join("meta.json"), &m);
            m
        } else {
            json(rdir.join("meta.json"))
        };
        if !s(&rec, "after").is_empty() {
            crate::refs::pin_slot(&rdir, &updated_meta, "applied", s(&rec, "after"));
        }
        marked_runs.push((rdir, updated_meta, applied_doc));
    }

    Ok(MarkAppliedResult {
        marked_runs,
        source,
        commit_sha,
        applied_at: at,
        checked_files,
        missing_files,
    })
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
fn is_hotspot_file(path: &str) -> bool {
    if path.starts_with("migrations/") || path.contains("/migrations/") {
        return true;
    }
    let base = path.rsplit('/').next().unwrap_or(path);
    matches!(
        base,
        "Cargo.toml"
            | "Cargo.lock"
            | "CHANGELOG.md"
            | "SKILL.md"
            | "package.json"
            | "pyproject.toml"
            | "VERSION"
            | "version.txt"
            | "bin.sha256"
    )
}

fn is_version_file(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    matches!(
        base,
        "Cargo.toml"
            | "CHANGELOG.md"
            | "SKILL.md"
            | "package.json"
            | "pyproject.toml"
            | "VERSION"
            | "version.txt"
            | "bin.sha256"
    )
}

fn mentions_standalone_digits(text: &str, digits: &str) -> bool {
    if digits.is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    text.match_indices(digits).any(|(at, _)| {
        let before_ok = at == 0 || !bytes[at - 1].is_ascii_digit();
        let after = at + digits.len();
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_digit();
        before_ok && after_ok
    })
}

fn extract_semvers(text: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'.' {
            continue;
        }
        i += 1;
        let m_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == m_start || i >= bytes.len() || bytes[i] != b'.' {
            continue;
        }
        i += 1;
        let p_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == p_start {
            continue;
        }
        let prev_ok = start == 0 || (!bytes[start - 1].is_ascii_digit() && bytes[start - 1] != b'.');
        let next_ok = i >= bytes.len() || (!bytes[i].is_ascii_digit() && bytes[i] != b'.');
        if prev_ok && next_ok {
            out.insert(text[start..i].to_string());
        }
    }
    out
}

/// Running and unapplied write tasks on the same source (field notes 16): what each has
/// changed so far, which of those paths the new task's prompt names, and any hotspot
/// (`migrations/`, `Cargo.toml`, `CHANGELOG.md`, version files) or version/migration number
/// collisions, so the caller can serialize (`--after <run>`) before overlap becomes a conflict.
pub fn concurrent_writes(created: &Path) -> Vec<Value> {
    use std::collections::BTreeSet;
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
    let extracted_prompt = crate::history::extract_user_prompt(created, &meta);
    let prompt = if !extracted_prompt.trim().is_empty() {
        extracted_prompt
    } else {
        read(created.join("prompt.md"))
    };
    let prompt_lower = prompt.to_lowercase();
    let our_versions = extract_semvers(&prompt);
    let prompt_mentions_migrations = prompt_lower.contains("migrations/")
        || prompt_lower.contains("migration")
        || prompt.contains("迁移");
    let generic = [
        "mod.rs",
        "lib.rs",
        "main.rs",
        "index.ts",
        "index.tsx",
        "README.md",
    ];
    let named = |path: &str| {
        let base = path.rsplit('/').next().unwrap_or(path);
        prompt.contains(path)
            || (is_hotspot_file(path) && prompt.contains(base))
            || (base.len() >= 8
                && base.contains('.')
                && !generic.contains(&base)
                && prompt.contains(base))
    };

    let all = crate::runs::all_runs();
    let superseded_parents: BTreeSet<String> = all
        .iter()
        .filter_map(|r| {
            let p = s(&json(r.join("meta.json")), "parent").to_string();
            (!p.is_empty()).then_some(p)
        })
        .collect();

    let mut found = vec![];
    for other in all {
        if other == created {
            continue;
        }
        let state = crate::runs::state(&other);
        let is_active = crate::runs::active(&state);
        let theirs = json(other.join("meta.json"));
        if s(&theirs, "mode") != "write" || source_of(&theirs) != ours {
            continue;
        }
        let other_id = other
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let is_unapplied = !is_active
            && !other.join(".applied").exists()
            && !other.join(".applied-cleaned").exists()
            && !b(&theirs, "appliedCleaned")
            && !superseded_parents.contains(&other_id)
            && matches!(
                state.as_str(),
                "delivered" | "answered" | "rejected" | "stalled" | "timeout" | "malformed"
            );
        if !is_active && !is_unapplied {
            continue;
        }
        let top = Some(s(&theirs["worktree"], "path"))
            .filter(|x| !x.is_empty())
            .unwrap_or(s(&theirs, "top"));
        let base_state = effective_chain_base(&other, &theirs, Path::new(top));
        let base = s(&base_state, "tree").to_string();
        let changed = if is_active {
            if base.is_empty() {
                vec![]
            } else {
                snapshot(Path::new(top), &other, &strings(&theirs["snapshotExclude"]))
                    .and_then(|now| {
                        tree_changes(Path::new(top), &base_state, &now, false).ok()
                    })
                    .unwrap_or_default()
                    .iter()
                    .map(|c| s(c, "path").to_string())
                    .collect::<Vec<_>>()
            }
        } else {
            let sum_files = strings(&json(other.join("summary.json"))["files"]);
            if !sum_files.is_empty() {
                sum_files
            } else {
                let ch_files = strings(&json(other.join("changes.json"))["files"]);
                if !ch_files.is_empty() {
                    ch_files
                } else if !base.is_empty() && Path::new(top).is_dir() {
                    snapshot(Path::new(top), &other, &strings(&theirs["snapshotExclude"]))
                        .and_then(|now| {
                            tree_changes(Path::new(top), &base_state, &now, false).ok()
                        })
                        .unwrap_or_default()
                        .iter()
                        .map(|c| s(c, "path").to_string())
                        .collect::<Vec<_>>()
                } else {
                    vec![]
                }
            }
        };
        if is_unapplied && changed.is_empty() {
            continue;
        }
        let mentioned = changed
            .iter()
            .filter(|p| named(p))
            .cloned()
            .collect::<Vec<_>>();

        let mut collisions = Vec::<String>::new();
        let mut seen_coll = BTreeSet::<String>::new();
        let mut push_coll = |msg: String| {
            if seen_coll.insert(msg.clone()) {
                collisions.push(msg);
            }
        };

        // 1. Migration / numbered-file prefix collisions (ignore files already committed on source HEAD).
        let source_path = Path::new(&ours);
        let mut saw_migration_dir = false;
        for p in &changed {
            let in_mig = p.starts_with("migrations/") || p.contains("/migrations/");
            if in_mig && path_exists_in_rev(source_path, "HEAD", p) {
                continue;
            }
            if let Some((dir, prefix)) = numbered_prefix(p) {
                if mentions_standalone_digits(&prompt, &prefix) {
                    push_coll(format!("同占编号 {dir}/{prefix}"));
                    if in_mig {
                        saw_migration_dir = true;
                    }
                } else if in_mig && prompt_mentions_migrations {
                    push_coll(format!("同触 {dir}/（已占 {prefix}）"));
                    saw_migration_dir = true;
                }
            } else if in_mig && prompt_mentions_migrations && !saw_migration_dir {
                push_coll("同触 migrations/".to_string());
                saw_migration_dir = true;
            }
        }

        // 2. Shared version number (semver) across prompt / version files / patch.
        if !our_versions.is_empty() {
            let mut their_versions = BTreeSet::new();
            let their_prompt = crate::history::extract_user_prompt(&other, &theirs);
            their_versions.extend(extract_semvers(&their_prompt));
            let patch_path = other.join("changes.patch");
            if let Ok(bytes) = fs::read(&patch_path) {
                let sample = String::from_utf8_lossy(&bytes[..bytes.len().min(256 * 1024)]);
                their_versions.extend(extract_semvers(&sample));
            }
            if Path::new(top).is_dir() {
                for p in &changed {
                    if is_version_file(p) {
                        let fp = Path::new(top).join(p);
                        if let Ok(bytes) = fs::read(&fp) {
                            let sample =
                                String::from_utf8_lossy(&bytes[..bytes.len().min(128 * 1024)]);
                            their_versions.extend(extract_semvers(&sample));
                        }
                    }
                }
            }
            for ver in our_versions.intersection(&their_versions) {
                push_coll(format!("同占版本号 {ver}"));
            }
        }

        // 3. Active run that hasn't written files yet: check shared hotspot keywords in prompts.
        if is_active && changed.is_empty() {
            let their_prompt = crate::history::extract_user_prompt(&other, &theirs);
            let their_lower = their_prompt.to_lowercase();
            for hot in ["CHANGELOG.md", "Cargo.toml", "SKILL.md", "package.json", "migrations/"] {
                if prompt.contains(hot) && their_prompt.contains(hot) {
                    push_coll(format!("同触热点 {hot}"));
                }
            }
            if prompt_mentions_migrations
                && (their_lower.contains("migrations/")
                    || their_lower.contains("migration")
                    || their_prompt.contains("迁移"))
            {
                push_coll("同触 migrations/".to_string());
            }
        }

        if is_unapplied && mentioned.is_empty() && collisions.is_empty() {
            continue;
        }

        let mut entry = json!({
            "name": s(&theirs, "name"),
            "run": other_id,
            "status": if is_active { "running" } else { "unapplied" },
            "changed": changed.len(),
            "sample": changed.iter().take(5).collect::<Vec<_>>(),
            "named": mentioned,
            "collisions": collisions,
        });
        if theirs["protect"].as_array().is_some_and(|p| !p.is_empty()) {
            entry["protect"] = theirs["protect"].clone();
        }
        found.push(entry);
    }
    found
}
/// How far the source moved since a finished, unapplied worktree run took its snapshot:
/// commits advanced, changed files, and which of them overlap with this chain's own changes.
#[allow(dead_code)]
pub fn source_drift(run: &Path) -> Option<Value> {
    let meta = json(run.join("meta.json"));
    if !meta["worktree"].is_object() || s(&meta, "mode") != "write" || run.join(".applied").exists()
    {
        return None;
    }
    let (_, top, _, after) = chain_changes(run).ok()?;
    let source = PathBuf::from(s(&meta["worktree"], "source"));
    if !source.is_dir() {
        return None;
    }
    let applied = applied_state(run, &meta);
    let baseline = if !s(&applied, "tree").is_empty() {
        applied.clone()
    } else {
        let (tree, _) = effective_total_base(&top, &meta);
        json!({"tree":tree,"large":{}})
    };
    let ours = tree_changes(&top, &baseline, &json!({"tree":after}), false).ok()?;
    let base_head = if !s(&applied, "head").is_empty() {
        s(&applied, "head").to_string()
    } else {
        s(&meta, "netBase").to_string()
    };
    let commits = if !base_head.is_empty() {
        git_text(&source, &["rev-list", "--count", &format!("{base_head}..HEAD")])
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
    } else {
        0
    };
    let now = snapshot(&source, run, &strings(&meta["snapshotExclude"]))?;
    let moved = tree_changes(&source, &baseline, &now, false).ok()?;
    if moved.is_empty() && commits == 0 {
        return None;
    }
    let overlap = moved
        .iter()
        .map(|c| s(c, "path"))
        .filter(|p| ours.iter().any(|c| s(c, "path") == *p))
        .collect::<Vec<_>>();
    let mut drift =
        json!({"files":moved.len(),"overlap":overlap.iter().take(10).collect::<Vec<_>>()});
    if commits > 0 {
        drift["commits"] = json!(commits);
    }
    if overlap.len() > 10 {
        drift["overlapMore"] = json!(overlap.len() - 10);
    }
    Some(drift)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// Default on `reply`: sync cleanly when possible, keep old baseline on conflict.
    Auto,
    /// Explicit `--sync`: fail with an error on any conflict.
    Strict,
    /// Explicit `--sync=agent`: write text conflict markers for the agent; fail on non-text conflict.
    Agent,
}

pub struct SyncReport {
    pub synced: bool,
    pub paths: Vec<String>,
    pub conflicts: Vec<String>,
    pub base_tree: String,
    pub base_head: String,
    pub base_commit: Option<String>,
    pub source_dirty: bool,
}

fn replay_commits_onto_base(
    worktree: &Path,
    old_base_commit: &str,
    new_base_commit: &str,
) -> Res<Option<String>> {
    let old_head = git_text(worktree, &["rev-parse", "-q", "--verify", "HEAD^{commit}"])
        .unwrap_or_default();
    if old_head.is_empty() || old_head == old_base_commit {
        return Ok(Some(new_base_commit.to_string()));
    }
    let list = git_text(
        worktree,
        &[
            "rev-list",
            "--reverse",
            &format!("{old_base_commit}..{old_head}"),
        ],
    )?;
    let commits: Vec<&str> = list.lines().filter(|l| !l.is_empty()).collect();
    let mut cur_parent = new_base_commit.to_string();
    for commit in commits {
        let parents_line = git_text(worktree, &["rev-list", "--parents", "-n", "1", commit])?;
        let parents: Vec<&str> = parents_line.split_whitespace().collect();
        // Skip replaying merge commits or root commits cleanly.
        if parents.len() != 2 {
            return Ok(None);
        }
        let c_parent = parents[1];
        let merge = git_command()
            .arg("-C")
            .arg(worktree)
            .args([
                "merge-tree",
                "--write-tree",
                &format!("--merge-base={c_parent}"),
                &cur_parent,
                commit,
            ])
            .output()
            .map_err(|e| e.to_string())?;
        if !merge.status.success() {
            return Ok(None);
        }
        let tree = String::from_utf8_lossy(&merge.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if tree.is_empty() {
            return Ok(None);
        }
        let meta_raw = git(
            worktree,
            &["show", "-s", "--format=%an%x00%ae%x00%aI%x00%B", commit],
        )?;
        let mut fields = meta_raw.splitn(4, |b| *b == 0);
        let author_name = String::from_utf8_lossy(fields.next().unwrap_or_default()).into_owned();
        let author_email = String::from_utf8_lossy(fields.next().unwrap_or_default()).into_owned();
        let author_date = String::from_utf8_lossy(fields.next().unwrap_or_default()).into_owned();
        let body = fields.next().unwrap_or_default();
        let mut ct = git_command();
        ct.arg("-C")
            .arg(worktree)
            .args(["commit-tree", &tree, "-p", &cur_parent, "-F", "-"])
            .env("GIT_AUTHOR_NAME", &author_name)
            .env("GIT_AUTHOR_EMAIL", &author_email)
            .env("GIT_AUTHOR_DATE", &author_date)
            .env("GIT_COMMITTER_NAME", &author_name)
            .env("GIT_COMMITTER_EMAIL", &author_email)
            .env("GIT_COMMITTER_DATE", &author_date)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = ct.spawn().map_err(|e| e.to_string())?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(body);
        }
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Ok(None);
        }
        cur_parent = String::from_utf8_lossy(&out.stdout).trim().to_string();
    }
    Ok(Some(cur_parent))
}

pub fn sync_with_mode(run: &Path, meta: &Value, mode: SyncMode) -> Res<SyncReport> {
    sync_with_mode_inner(run, meta, mode, false)
}

/// Sync a run's worktree cleanly to the committed source `HEAD` (never pulling dirty
/// uncommitted working-tree files from source). Used by mainline-blocked auto-rerun.
pub fn sync_to_head(run: &Path, meta: &Value) -> Res<SyncReport> {
    sync_with_mode_inner(run, meta, SyncMode::Auto, true)
}

fn sync_with_mode_inner(
    run: &Path,
    meta: &Value,
    mode: SyncMode,
    committed_only: bool,
) -> Res<SyncReport> {
    let agent = mode == SyncMode::Agent;
    let source = PathBuf::from(s(&meta["worktree"], "source"));
    let worktree = PathBuf::from(s(&meta["worktree"], "path"));
    let exclude = strings(&meta["snapshotExclude"]);
    let source_head =
        git_text(&source, &["rev-parse", "-q", "--verify", "HEAD^{commit}"]).unwrap_or_default();
    let source_head_tree = if !source_head.is_empty() {
        git_text(&source, &["rev-parse", "-q", "--verify", "HEAD^{tree}"]).unwrap_or_default()
    } else {
        String::new()
    };
    let (current_source, source_dirty) = if committed_only {
        if source_head_tree.is_empty() {
            return Err(format!("cannot resolve HEAD tree for {}", source.display()));
        }
        (json!({"tree": source_head_tree, "large": {}}), false)
    } else {
        let snap = snapshot(&source, run, &exclude)
            .ok_or_else(|| format!("cannot snapshot {} for --sync", source.display()))?;
        let dirty = source_head_tree != s(&snap, "tree");
        (snap, dirty)
    };
    let applied = applied_state(run, meta);
    let baseline = if !s(&applied, "tree").is_empty() {
        applied
    } else {
        let (tree, _) = effective_total_base(&worktree, meta);
        json!({"tree":tree,"large":{}})
    };
    let changes = tree_changes(&source, &baseline, &current_source, false)?;
    if changes.is_empty() {
        return Ok(SyncReport {
            synced: false,
            paths: vec![],
            conflicts: vec![],
            base_tree: s(&current_source, "tree").to_string(),
            base_head: source_head,
            base_commit: None,
            source_dirty,
        });
    }
    let mut actions = vec![];
    let mut conflicts = vec![];
    let mut paths = vec![];
    for change in changes {
        let path = s(&change, "path").to_string();
        let target = worktree.join(&path);
        // Auto-sync never pulls unresolved conflict markers from the source workspace into a worktree.
        if mode == SyncMode::Auto && !conflict_labels(&source.join(&path)).is_empty() {
            conflicts.push(path);
            continue;
        }
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
        let (mode_str, new) = blob(&source, s(&current_source, "tree"), &path)?;
        let now = current(&target);
        let now_mode = entry_mode(&target);
        let valid = |x: &Option<String>| {
            x.as_deref()
                .is_none_or(|v| ["100644", "100755", "120000"].contains(&v))
        };
        if !valid(&old_mode) || !valid(&mode_str) {
            conflicts.push(path);
        } else if now == new && now_mode == mode_str {
            continue;
        } else if now == old && (now_mode == old_mode || now_mode == mode_str) {
            paths.push(path.clone());
            actions.push(Action {
                kind: if new.is_none() { "deleted" } else { "applied" }.into(),
                path,
                target,
                mode: mode_str,
                content: new.unwrap_or_default(),
            });
        } else if agent || (old.is_some() && new.is_some() && now.is_some()) {
            let base = old.as_deref().unwrap_or_default();
            let theirs = new.as_deref().unwrap_or_default();
            let mine = now.as_deref().unwrap_or_default();
            if [old_mode.as_deref(), mode_str.as_deref(), now_mode.as_deref()]
                .contains(&Some("120000"))
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
            let output = git_command()
                .arg("merge-file")
                .args(["-L", "worktree", "-L", "base", "-L", "caller"])
                .args([&minefile, &basefile, &theirfile])
                .output()
                .map_err(|e| e.to_string())?;
            let merged = fs::read(&minefile).map_err(|e| e.to_string())?;
            let _ = fs::remove_dir_all(scratch);
            if output.status.success()
                || (agent
                    && output
                        .status
                        .code()
                        .is_some_and(|code| (1..=127).contains(&code)))
            {
                if !output.status.success() {
                    conflicts.push(path.clone());
                }
                paths.push(path.clone());
                actions.push(Action {
                    kind: "merged".into(),
                    path,
                    target,
                    mode: resolved_mode(
                        old_mode.as_deref(),
                        mode_str.as_deref(),
                        now_mode.as_deref(),
                    )
                    .or_else(|| Some("100644".into())),
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
        match mode {
            SyncMode::Auto => {
                return Ok(SyncReport {
                    synced: false,
                    paths: vec![],
                    conflicts,
                    base_tree: s(&current_source, "tree").to_string(),
                    base_head: source_head,
                    base_commit: None,
                    source_dirty,
                });
            }
            SyncMode::Strict => {
                return Err(format!("--sync conflict in: {}", conflicts.join(", ")));
            }
            SyncMode::Agent => {
                // Binary, symlink, gitlink and structural conflicts cannot safely be represented
                // by text markers. Fail atomically rather than silently dropping caller changes.
                if conflicts.iter().any(|p| !paths.contains(p)) {
                    return Err(format!("--sync conflict in: {}", conflicts.join(", ")));
                }
            }
        }
    }
    // Prepare the new Git baseline commit & replay colleague commits purely in Git's object store
    // BEFORE mutating worktree files, so Auto mode stays atomic and `target/` is never touched.
    let mut synced_base_commit: Option<String> = None;
    let mut new_wt_head: Option<String> = None;
    if conflicts.is_empty() {
        let new_base = if !source_dirty && !source_head.is_empty() {
            Some(source_head.clone())
        } else {
            let mut c = git_command();
            c.arg("-C")
                .arg(&source)
                .arg("commit-tree")
                .arg(s(&current_source, "tree"));
            if !source_head.is_empty() {
                c.args(["-p", &source_head]);
            }
            c.arg("-m").arg(format!(
                "delegate: synced baseline of {} for {}",
                source.display(),
                run.file_name().unwrap_or_default().to_string_lossy()
            ));
            for who in ["AUTHOR", "COMMITTER"] {
                c.env(format!("GIT_{who}_NAME"), "delegate")
                    .env(format!("GIT_{who}_EMAIL"), "delegate@localhost");
            }
            c.output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|sha| !sha.is_empty())
        };
        if let Some(base_commit) = new_base {
            let old_base = if b(meta, "netBaseRebased") && !s(meta, "netBase").is_empty() {
                Some(s(meta, "netBase").to_string())
            } else {
                crate::changes::worktree_base_commit(run, meta)
            };
            if let Some(old_base) = old_base {
                match replay_commits_onto_base(&worktree, &old_base, &base_commit)? {
                    Some(head) => {
                        synced_base_commit = Some(base_commit);
                        new_wt_head = Some(head);
                    }
                    None if mode == SyncMode::Auto => {
                        return Ok(SyncReport {
                            synced: false,
                            paths: vec![],
                            conflicts: paths,
                            base_tree: s(&current_source, "tree").to_string(),
                            base_head: source_head,
                            base_commit: None,
                            source_dirty,
                        });
                    }
                    None => {}
                }
            }
        }
    }
    for action in actions {
        if action.kind == "deleted" {
            match fs::remove_file(&action.target) {
                Ok(()) => remove_empty_parents(&action.target, &worktree),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
        } else if let Some(mode_str) = action.mode.as_deref() {
            write_entry(&action.target, mode_str, &action.content)?;
        } else {
            write(&action.target, &action.content)?;
        }
    }
    if let Some(head) = &new_wt_head {
        // Mixed reset updates HEAD and index without touching working-tree files or `target/`.
        let _ = git(&worktree, &["reset", "--mixed", "-q", head]);
    }
    let mut large_hashes = serde_json::Map::new();
    if let Some(large) = current_source["large"].as_object() {
        for path in large.keys() {
            large_hashes.insert(path.clone(), json!(file_hash(&source.join(path))?));
        }
    }
    let mut state = json!({"at":iso(),"tree":current_source["tree"],"large":current_source["large"],"largeHashes":large_hashes});
    if !source_head.is_empty() {
        state["head"] = json!(&source_head);
    }
    let mut chain = run.to_path_buf();
    loop {
        write_json(chain.join(".sync-base"), &state)?;
        let chain_meta = json(chain.join("meta.json"));
        crate::refs::pin_slot(&chain, &chain_meta, "sync-base", s(&current_source, "tree"));
        let parent = s(&chain_meta, "parent").to_string();
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
    Ok(SyncReport {
        synced: true,
        paths,
        conflicts,
        base_tree: s(&current_source, "tree").to_string(),
        base_head: source_head,
        base_commit: synced_base_commit,
        source_dirty,
    })
}
/// Standard exit code for merge/apply conflicts and unresolved conflict markers.
pub const EXIT_CONFLICT: i32 = 3;

pub struct ApplyOutcome {
    pub code: i32,
    pub conclusion: Value,
    pub written_files: Vec<String>,
    pub regenerated_files: Vec<String>,
    pub had_dirty_merge: bool,
    pub already_applied: bool,
}
pub(crate) struct ApplyInnerResult {
    pub code: i32,
    pub written: Vec<String>,
    pub regenerated: Vec<String>,
    pub had_dirty_merge: bool,
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
fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}
fn fence_marker(line: &str) -> Option<(u8, usize, &str)> {
    let bytes = line.as_bytes();
    let indent = bytes.iter().take_while(|&&b| b == b' ').count();
    if indent > 3 {
        return None;
    }
    let rest = &bytes[indent..];
    let ch = *rest.first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let count = rest.iter().take_while(|&&b| b == ch).count();
    if count < 3 {
        return None;
    }
    let tail = &line[indent + count..];
    Some((ch, count, tail))
}
pub(crate) fn conflict_labels_in_bytes(path: &Path, bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let markdown = is_markdown(path);
    let mut labels = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    // Inside Markdown code fences, require a full <<<<<<< ... ======= ... >>>>>>> block
    // so prose/examples mentioning conflict markers do not false-positive.
    let mut fenced_open: Option<String> = None;
    let mut fenced_divider = false;
    for line in text.lines() {
        if markdown {
            if let Some((fch, fcount)) = fence {
                if let Some((ch, count, tail)) = fence_marker(line) {
                    if ch == fch && count >= fcount && tail.trim().is_empty() {
                        fence = None;
                        fenced_open = None;
                        fenced_divider = false;
                        continue;
                    }
                }
                if let Some(label) = line.strip_prefix("<<<<<<< ") {
                    fenced_open = Some(label.to_string());
                    fenced_divider = false;
                } else if line == "=======" && fenced_open.is_some() {
                    fenced_divider = true;
                } else if let Some(label) = line.strip_prefix(">>>>>>> ") {
                    if let (Some(open), true) = (fenced_open.take(), fenced_divider) {
                        labels.push(open);
                        labels.push(label.to_string());
                    }
                    fenced_divider = false;
                }
                continue;
            } else if let Some((ch, count, tail)) = fence_marker(line) {
                if ch == b'~' || !tail.contains('`') {
                    fence = Some((ch, count));
                    fenced_open = None;
                    fenced_divider = false;
                    continue;
                }
            }
        }
        if let Some(label) = line
            .strip_prefix("<<<<<<< ")
            .or_else(|| line.strip_prefix(">>>>>>> "))
        {
            labels.push(label.to_string());
        }
    }
    labels
}
/// Marker labels include the originating run, so diagnostics survive old/cleaned runs.
pub fn conflict_labels(path: &Path) -> Vec<String> {
    if path.is_symlink() {
        return vec![];
    }
    fs::read(path)
        .ok()
        .map(|bytes| conflict_labels_in_bytes(path, &bytes))
        .unwrap_or_default()
}
/// Sync handoffs also reject a half-removed marker block (e.g. only the divider).
pub fn has_conflict_markers(path: &Path) -> bool {
    if path.is_symlink() {
        return false;
    }
    fs::read(path).is_ok_and(|bytes| {
        if !conflict_labels_in_bytes(path, &bytes).is_empty() {
            return true;
        }
        let text = String::from_utf8_lossy(&bytes);
        let markdown = is_markdown(path);
        let mut fence: Option<(u8, usize)> = None;
        let mut prev_nonempty = false;
        for line in text.lines() {
            if markdown {
                if let Some((fch, fcount)) = fence {
                    if let Some((ch, count, tail)) = fence_marker(line) {
                        if ch == fch && count >= fcount && tail.trim().is_empty() {
                            fence = None;
                        }
                    }
                    prev_nonempty = false;
                    continue;
                } else if let Some((ch, count, tail)) = fence_marker(line) {
                    if ch == b'~' || !tail.contains('`') {
                        fence = Some((ch, count));
                        prev_nonempty = false;
                        continue;
                    }
                }
            }
            if line.starts_with("||||||| ") {
                return true;
            }
            if line == "=======" && !(markdown && prev_nonempty) {
                return true;
            }
            prev_nonempty = !line.trim().is_empty();
        }
        false
    })
}
fn marked_files(source: &Path, rules: &[String]) -> Res<Vec<String>> {
    let mut paths = rules
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    if rules.iter().any(|p| p.ends_with('/')) {
        paths.extend(
            git(
                source,
                &[
                    "ls-files",
                    "-z",
                    "--cached",
                    "--others",
                    "--exclude-standard",
                ],
            )?
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .filter(|p| matches_rule(p, rules)),
        );
    }
    Ok(paths
        .into_iter()
        .filter(|p| !conflict_labels(&source.join(p)).is_empty())
        .collect())
}
enum PreflightMarkers {
    Clear(Vec<String>),
    Blocked {
        paths: Vec<String>,
        details: Vec<String>,
    },
}

fn check_existing_markers(run: &Path, meta: &Value, keep_commits: bool) -> Res<PreflightMarkers> {
    let (_, top, _, after) = chain_changes(run)?;
    let applied = applied_state(run, meta);
    let rec = json(run.join("changes.json"));
    let (total_base, _) = effective_total_base(&top, meta);
    let before = if !keep_commits && !s(&applied, "tree").is_empty() {
        s(&applied, "tree").to_string()
    } else {
        total_base.clone()
    };
    let missing = crate::refs::missing_in_top(
        &top,
        run,
        meta,
        &[("base", &before), ("after", &after)],
    );
    if !missing.is_empty() {
        return Err(crate::refs::recovery_message(run, meta, &missing));
    }
    let mut changes = tree_changes(
        &top,
        &json!({"tree":before,"large":applied["large"]}),
        &json!({"tree":after,"large":rec["afterLarge"]}),
        false,
    )?;
    if changes.is_empty() && !keep_commits && !s(&applied, "tree").is_empty() {
        // Match apply_inner's repeated-apply drift check, but not unrelated earlier rounds.
        changes = tree_changes(
            &top,
            &json!({"tree":total_base}),
            &json!({"tree":after}),
            false,
        )?;
    }
    let source = Path::new(s(&meta["worktree"], "source"));
    let mut paths = changes
        .iter()
        .map(|c| s(c, "path").to_string())
        .collect::<Vec<_>>();
    if let Some(large) = rec["afterLarge"].as_object() {
        paths.extend(large.keys().cloned());
    }
    if keep_commits {
        let worktree = Path::new(s(&meta["worktree"], "path"));
        let (base_slot, base) = if b(meta, "netBaseRebased") && !s(meta, "netBase").is_empty() {
            ("net-base", s(meta, "netBase").to_string())
        } else {
            (
                "worktree-base",
                crate::changes::worktree_base_commit(run, meta).unwrap_or_default(),
            )
        };
        if worktree.is_dir() && !base.is_empty() {
            let missing =
                crate::refs::missing_in_top(worktree, run, meta, &[(base_slot, &base)]);
            if !missing.is_empty() {
                return Err(crate::refs::recovery_message(run, meta, &missing));
            }
            for hash in git_text(
                worktree,
                &["log", "--format=%H", &format!("{base}..HEAD")],
            )?
            .lines()
            {
                paths.extend(
                    git(
                        worktree,
                        &[
                            "diff-tree",
                            "--no-commit-id",
                            "--name-only",
                            "--no-renames",
                            "-r",
                            "-z",
                            hash,
                        ],
                    )?
                    .split(|b| *b == 0)
                    .filter(|p| !p.is_empty())
                    .map(|p| String::from_utf8_lossy(p).into_owned()),
                );
            }
        }
    }
    let (generated_paths, _) = generated(source)?;
    let inputs = generated_inputs(source)?;
    let regenerating = generation_markers(run, meta).iter().any(|p| p.exists())
        || paths.iter().any(|p| {
            !matches_rule(p, &generated_paths) && (inputs.is_empty() || matches_rule(p, &inputs))
        });
    let mut checked = paths
        .iter()
        .filter(|p| keep_commits || regenerating || !matches_rule(p, &generated_paths))
        .cloned()
        .collect::<Vec<_>>();
    if regenerating && !keep_commits {
        checked.extend(generated_paths);
    }
    let blocked_paths = marked_files(source, &checked)?;
    if !blocked_paths.is_empty() {
        let details = blocked_paths
            .iter()
            .map(|path| {
                format!(
                    "{path} (markers: {})",
                    conflict_labels(&source.join(path)).join(", ")
                )
            })
            .collect::<Vec<_>>();
        return Ok(PreflightMarkers::Blocked {
            paths: blocked_paths,
            details,
        });
    }
    Ok(PreflightMarkers::Clear(checked))
}
/// Uncommitted paths in `source` (staged, unstaged, and untracked, excluding `.local/`).
/// In a freshly initialized repository with no HEAD commit yet, returns empty (no baseline commit
/// to separate uncommitted hunks against).
pub(crate) fn source_dirty_paths(source: &Path) -> Vec<String> {
    if read_git_head(source).is_none() {
        return Vec::new();
    }
    let Ok(raw) = git(
        source,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
    ) else {
        return Vec::new();
    };
    let mut dirty = Vec::new();
    for entry in raw.split(|b| *b == 0) {
        if entry.len() <= 3 {
            continue;
        }
        let path = String::from_utf8_lossy(&entry[3..]).into_owned();
        if path == ".local" || path.starts_with(".local/") {
            continue;
        }
        dirty.push(path);
    }
    dirty
}

fn dirty_path_overlaps(path: &str, dirty: &[String]) -> bool {
    let a = path.trim_end_matches('/');
    dirty.iter().any(|d| {
        let b = d.trim_end_matches('/');
        a == b
            || a.strip_prefix(b).is_some_and(|r| r.starts_with('/'))
            || b.strip_prefix(a).is_some_and(|r| r.starts_with('/'))
    })
}

fn read_file_list(path: &Path) -> Option<Vec<String>> {
    if !path.is_file() {
        return None;
    }
    let mut files: Vec<String> = read(path)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    files.sort();
    files.dedup();
    Some(files)
}

fn resolve_parent_run_dir(run: &Path, parent_id: &str) -> Option<PathBuf> {
    if parent_id.is_empty() {
        return None;
    }
    if let Some(dir) = run.parent().map(|p| p.join(parent_id)).filter(|p| p.is_dir()) {
        return Some(dir);
    }
    let applied_sib = runs_root().join(".applied").join(parent_id);
    if applied_sib.is_dir() {
        return Some(applied_sib);
    }
    let archived = cache_dir().join("archive").join(parent_id).join("record");
    if archived.is_dir() {
        return Some(archived);
    }
    None
}

fn chain_root_base_large(run: &Path, meta: &Value) -> Value {
    if b(meta, "netBaseRebased") {
        return json!({});
    }
    let mut cur_dir = run.to_path_buf();
    let mut cur_meta = meta.clone();
    let mut visited = std::collections::BTreeSet::<String>::new();
    let self_id = s(&cur_meta, "run").to_string();
    if !self_id.is_empty() {
        visited.insert(self_id);
    }
    while !s(&cur_meta, "parent").is_empty() {
        let parent_id = s(&cur_meta, "parent").to_string();
        if !visited.insert(parent_id.clone()) {
            break;
        }
        let Some(pdir) = resolve_parent_run_dir(&cur_dir, &parent_id) else {
            break;
        };
        let pmeta = json(pdir.join("meta.json"));
        if pmeta.is_null() {
            break;
        }
        cur_dir = pdir;
        cur_meta = pmeta;
    }
    if cur_meta["base"]["large"].is_object() {
        cur_meta["base"]["large"].clone()
    } else {
        json!({})
    }
}

/// Net changed paths of a run's entire reply chain relative to `effective_total_base`
/// (same baseline and `--no-renames` semantics as `delegate diff <run> --total`:
/// additions, modifications, deletions, and both ends of renames).
/// Works before apply, after apply (`.applied`), and after worktree removal / archive
/// as long as the git objects remain reachable in `worktree.source` or `top`.
pub fn chain_net_files(run: &Path, meta: &Value) -> Option<Vec<String>> {
    let rec = json(run.join("changes.json"));
    if rec.is_null() {
        return None;
    }
    let after = s(&rec, "after");
    if after.is_empty() {
        return None;
    }
    let top = [
        s(&rec, "top"),
        s(&meta["worktree"], "source"),
        s(meta, "top"),
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| !p.as_os_str().is_empty() && p.is_dir())?;

    let (mut base_tree, _) = effective_total_base(&top, meta);
    if base_tree.is_empty() {
        base_tree = s(&rec, "base").to_string();
    }
    if base_tree.is_empty() {
        return None;
    }
    let before_large = chain_root_base_large(run, meta);
    let before = json!({"tree": base_tree, "large": before_large});
    let after_val = json!({"tree": after, "large": rec["afterLarge"]});
    let changes = tree_changes(&top, &before, &after_val, meta["worktree"].is_object()).ok()?;
    let mut files: Vec<String> = changes
        .into_iter()
        .map(|c| s(&c, "path").to_string())
        .filter(|p| !p.is_empty())
        .collect();
    files.sort();
    files.dedup();
    Some(files)
}

fn parse_patch_paths(patch: &str) -> Vec<String> {
    let mut set = std::collections::BTreeSet::<String>::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            if let Some((a, b)) = rest.split_once(" b/") {
                if !a.is_empty() {
                    set.insert(a.to_string());
                }
                if !b.is_empty() {
                    set.insert(b.to_string());
                }
            }
        } else if let Some(p) = line.strip_prefix("rename from ") {
            if !p.is_empty() {
                set.insert(p.to_string());
            }
        } else if let Some(p) = line.strip_prefix("rename to ") {
            if !p.is_empty() {
                set.insert(p.to_string());
            }
        }
    }
    set.into_iter().collect()
}

fn collect_round_recorded_files(run: &Path, set: &mut std::collections::BTreeSet<String>) {
    let mut found = false;
    let rec = json(run.join("changes.json"));
    if let Some(arr) = rec["changes"].as_array() {
        for item in arr {
            let p = s(item, "path");
            if !p.is_empty() {
                set.insert(p.to_string());
                found = true;
            }
        }
    }
    if let Some(obj) = rec["afterLarge"].as_object() {
        for k in obj.keys() {
            if !k.is_empty() {
                set.insert(k.clone());
                found = true;
            }
        }
    }
    if !found {
        for patch_name in ["applied.patch", "changes.patch"] {
            let patch_path = run.join(patch_name);
            if patch_path.is_file() {
                for p in parse_patch_paths(&read(&patch_path)) {
                    set.insert(p);
                    found = true;
                }
                if found {
                    break;
                }
            }
        }
    }
    if !found {
        let sum = json(run.join("summary.json"));
        for p in strings(&sum["files"]) {
            if !p.is_empty() {
                set.insert(p);
            }
        }
    }
}

fn collect_chain_fallback_files(run: &Path) -> Vec<String> {
    let mut set = std::collections::BTreeSet::<String>::new();
    let mut visited = std::collections::BTreeSet::<String>::new();
    let mut cur = Some(run.to_path_buf());
    while let Some(dir) = cur {
        let meta = json(dir.join("meta.json"));
        let rid = s(&meta, "run").to_string();
        if !rid.is_empty() && !visited.insert(rid) {
            break;
        }
        collect_round_recorded_files(&dir, &mut set);
        let parent_id = s(&meta, "parent").to_string();
        cur = if parent_id.is_empty() {
            None
        } else {
            resolve_parent_run_dir(&dir, &parent_id)
        };
    }
    set.into_iter().collect()
}

/// Deterministic list of repo-relative files changed by a run's entire chain
/// (matches `delegate diff <run> --total` and `delegate apply <run>`).
/// Works before apply, after apply (`.applied`), and after auto-clean archive.
pub fn run_files(run: &Path) -> Vec<String> {
    if let Some(files) = read_file_list(&run.join("applied-files.txt")).filter(|v| !v.is_empty()) {
        return files;
    }
    let applied = json(run.join(".applied"));
    if let Some(arr) = applied["files"].as_array().filter(|a| !a.is_empty()) {
        let mut files: Vec<String> = arr
            .iter()
            .filter_map(Value::as_str)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        files.sort();
        files.dedup();
        return files;
    }
    let meta = json(run.join("meta.json"));
    if let Some(files) = chain_net_files(run, &meta) {
        return files;
    }
    if let Some(files) = read_file_list(&run.join("chain-files.txt")) {
        return files;
    }
    collect_chain_fallback_files(run)
}

fn record_dirty_overlap_failure(
    run: &Path,
    meta: &Value,
    source: &Path,
    dirty_overlap: &[String],
    conclusion: &mut Value,
) -> ApplyInnerResult {
    let run_id = {
        let r = s(meta, "run");
        if !r.is_empty() {
            r.to_string()
        } else {
            run.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }
    };
    let commit_cmd = format!(
        "先提交源工作区现有改动，再运行：{} apply {run_id}",
        script().display()
    );
    let allow_cmd = format!(
        "{} apply --allow-dirty-overlap {run_id}",
        script().display()
    );
    if !crate::output::json_enabled() {
        for p in dirty_overlap {
            println!(" dirty-overlap    {p}");
        }
    }
    eprintln!(
        "delegate: nothing applied; {} target file(s) already have uncommitted changes in {}: {}.\n  下一步（选项 1，推荐）：{commit_cmd}\n  下一步（选项 2，明确接受合并进未提交改动）：{allow_cmd}",
        dirty_overlap.len(),
        source.display(),
        dirty_overlap.join(", "),
    );
    conclusion["apply"]["failedStep"] = json!("dirty-overlap");
    conclusion["apply"]["failureDetail"] = json!(format!(
        "{} file(s) already have uncommitted changes in source: {}",
        dirty_overlap.len(),
        dirty_overlap.join(", ")
    ));
    conclusion["apply"]["dirtyOverlap"] = json!(dirty_overlap);
    conclusion["apply"]["writtenFiles"] = json!([]);
    conclusion["apply"]["next"] = json!([commit_cmd, allow_cmd]);
    conclusion["next"] = json!(commit_cmd);
    ApplyInnerResult {
        code: 1,
        written: vec![],
        regenerated: vec![],
        had_dirty_merge: false,
    }
}

fn already_applied_outcome(run: &Path, meta: &Value) -> Option<ApplyOutcome> {
    let run_id = {
        let id = s(meta, "run");
        if !id.is_empty() {
            id.to_string()
        } else {
            run.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }
    };
    let direct_applied = run.join(".applied");
    let direct_cleaned = run.join(".applied-cleaned");
    let archived_applied = cache_dir()
        .join("archive")
        .join(&run_id)
        .join("record")
        .join(".applied");
    let applied_doc = if direct_applied.is_file() {
        json(&direct_applied)
    } else if direct_cleaned.is_file() {
        Value::Null
    } else if !run.join("meta.json").is_file() && archived_applied.is_file() {
        json(&archived_applied)
    } else {
        return None;
    };
    let at = if !s(&applied_doc, "at").is_empty() {
        s(&applied_doc, "at").to_string()
    } else if !s(meta, "appliedAt").is_empty() {
        s(meta, "appliedAt").to_string()
    } else {
        "未知时间".to_string()
    };
    let head = if !s(&applied_doc, "head").is_empty() {
        s(&applied_doc, "head").to_string()
    } else {
        s(meta, "appliedHead").to_string()
    };
    let message = format!("已于 {at} 合入");
    let mut apply_obj = json!({
        "ok": true,
        "alreadyApplied": true,
        "appliedAt": at,
        "message": message,
    });
    if !head.is_empty() {
        apply_obj["head"] = json!(head);
    }
    let conclusion = json!({
        "run": run_id,
        "apply": apply_obj,
    });
    Some(ApplyOutcome {
        code: 0,
        conclusion,
        written_files: Vec::new(),
        regenerated_files: Vec::new(),
        had_dirty_merge: false,
        already_applied: true,
    })
}

pub fn apply(
    run: &Path,
    merge: bool,
    dry: bool,
    keep_commits: bool,
    commit_mode: bool,
    allow_dirty_overlap: bool,
) -> Res<ApplyOutcome> {
    let mut meta = json(run.join("meta.json"));
    if s(&meta, "mode") == "read-only" {
        return Err("read-only snapshot: nothing to apply / 只读任务禁止 apply".into());
    }
    let source = PathBuf::from(s(&meta["worktree"], "source"));
    if !source.is_absolute() {
        if let Some(outcome) = already_applied_outcome(run, &meta) {
            return Ok(outcome);
        }
        return apply_checked(
            run,
            merge,
            dry,
            keep_commits,
            commit_mode,
            allow_dirty_overlap,
            &[],
        );
    }
    if !dry {
        lane::install_signals();
    }
    let _apply_guard = crate::source_watch::apply_guard(&source)?;
    if let Some(outcome) = already_applied_outcome(run, &meta) {
        return Ok(outcome);
    }
    // Refresh the delivered diff before releasing its worktree (also captures later edits).
    if Path::new(s(&meta["worktree"], "path")).is_dir() {
        crate::changes::record(&mut meta, run);
    }
    let written_paths = match check_existing_markers(run, &meta, keep_commits)? {
        PreflightMarkers::Clear(paths) => paths,
        PreflightMarkers::Blocked { paths, details } => {
            let msg = format!(
                "nothing applied: unresolved conflict markers / 冲突标记未解决: {}",
                details.join("; ")
            );
            eprintln!("delegate: {msg}");
            let now = iso();
            let mut conclusion = json!({
                "run": run.file_name().unwrap_or_default().to_string_lossy(),
                "operation": "apply",
                "apply": {
                    "ok": false,
                    "dryRun": dry,
                    "failedStep": "conflict-markers",
                    "failureDetail": msg,
                    "conflictMarkers": paths,
                    "writtenFiles": [],
                },
                "startedAt": now,
                "finishedAt": now,
                "elapsedMs": 0,
            });
            if let Some(sources) = meta.get("configSources") {
                conclusion["configSources"] = sources.clone();
            }
            return Ok(ApplyOutcome {
                code: EXIT_CONFLICT,
                conclusion,
                written_files: vec![],
                regenerated_files: vec![],
                had_dirty_merge: false,
                already_applied: false,
            });
        }
    };
    if !dry && run.join("changes.patch").is_file() {
        let mut patch = fs::read(run.join("changes.patch"))
            .map_err(|e| format!("retain applied.patch: {e}"))?;
        if let Some(large) = json(run.join("changes.json"))["afterLarge"].as_object() {
            for path in large.keys() {
                let worktree = Path::new(s(&meta["worktree"], "path"));
                let mut file = fs::File::open(worktree.join(path)).map_err(|e| e.to_string())?;
                let mut buffer = [0; 8192];
                let binary = loop {
                    let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
                    if count == 0 {
                        break false;
                    }
                    if buffer[..count].contains(&0) {
                        break true;
                    }
                };
                if binary {
                    patch.extend_from_slice(
                        format!("\n# binary/large file omitted: {}\n", json!(path)).as_bytes(),
                    );
                } else {
                    // Large untracked text is outside the snapshot tree, but must
                    // still be recoverable without its soon-to-be-released worktree.
                    let diff = git_command()
                        .arg("-C")
                        .arg(worktree)
                        .args(["diff", "--no-index", "--binary", "--", "/dev/null", path])
                        .output()
                        .map_err(|e| e.to_string())?;
                    if !diff.status.success() && diff.status.code() != Some(1) {
                        return Err(String::from_utf8_lossy(&diff.stderr).into());
                    }
                    patch.extend(diff.stdout);
                }
            }
        }
        write(run.join("applied.patch"), patch)?;
    }
    apply_checked(
        run,
        merge,
        dry,
        keep_commits,
        commit_mode,
        allow_dirty_overlap,
        &written_paths,
    )
}
pub(crate) fn collect_commits(run: &Path, meta: &Value) -> Res<Vec<Value>> {
    let worktree = Path::new(s(&meta["worktree"], "path"));
    let (base_slot, base) = if b(meta, "netBaseRebased") && !s(meta, "netBase").is_empty() {
        ("net-base", s(meta, "netBase").to_string())
    } else {
        (
            "worktree-base",
            crate::changes::worktree_base_commit(run, meta).unwrap_or_default(),
        )
    };
    if base.is_empty() || !worktree.is_dir() {
        return Ok(Vec::new());
    }
    let missing = crate::refs::missing_in_top(worktree, run, meta, &[(base_slot, &base)]);
    if !missing.is_empty() {
        return Err(crate::refs::recovery_message(run, meta, &missing));
    }
    let log = git_text(
        worktree,
        &[
            "log",
            "--reverse",
            "--format=%H%x09%s",
            &format!("{base}..HEAD"),
        ],
    )?;
    Ok(log
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(hash, title)| {
            let files = git_text(
                worktree,
                &["diff-tree", "--no-commit-id", "--name-only", "-r", hash],
            )
            .unwrap_or_default()
            .lines()
            .count();
            json!({"hash":hash,"title":title,"files":files})
        })
        .collect())
}

#[derive(Debug, Clone)]
pub(crate) struct MergeDirOutcome {
    pub conflicts: Vec<String>,
    pub marked_paths: Vec<String>,
    pub numbered_warnings: Vec<Value>,
    pub touches_generated_inputs: bool,
    pub generate_command: String,
    pub after_tree: String,
    pub has_large: bool,
}

/// 3-way merge a run's net changes into an arbitrary target worktree directory (used by
/// `delegate merge` for its isolated integration worktree). Reuses `apply_inner`'s exact
/// merge semantics (3-way `git merge-file`, `appendOnly` `--union`, `generated.paths` skip,
/// conflict-marker rejection, large files, symlinks/exec modes). Fails closed without writing
/// if any conflict or conflict marker is detected.
pub(crate) fn merge_run_into_dir(
    run: &Path,
    target_root: &Path,
    source_for_config: &Path,
) -> Res<MergeDirOutcome> {
    let (meta, mut top, _, mut after) = chain_changes(run)?;
    if meta["worktree"].is_null() {
        return Err(format!(
            "{} worked in place; not a worktree run",
            run.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    if s(&meta, "mode") == "read-only" {
        return Err(format!(
            "{} is read-only; nothing to merge",
            run.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    let applied = applied_state(run, &meta);
    let (total_base, _) = effective_total_base(&top, &meta);
    let before = if !s(&applied, "tree").is_empty() {
        s(&applied, "tree").to_string()
    } else {
        total_base.clone()
    };
    let (generated_paths, generate_command) = generated(source_for_config)?;
    let append_only = append_only(source_for_config)?;
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
    let missing = crate::refs::missing_in_top(
        &top,
        run,
        &meta,
        &[("base", &before), ("after", &after)],
    );
    if !missing.is_empty() {
        return Err(crate::refs::recovery_message(run, &meta, &missing));
    }
    let changes = tree_changes(
        &top,
        &json!({"tree":before,"large":applied["large"]}),
        &json!({"tree":after,"large":large}),
        false,
    )?;

    let mut actions = vec![];
    let mut matched = vec![];
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
        let target = target_root.join(&path);
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
        if through_symlink(target_root, &target)
            || now_mode.as_deref() == Some("dir")
            || !valid(&old_mode)
            || !valid(&mode)
        {
            conflicts.push(path);
        } else if now == new && now_mode == mode {
            matched.push(path);
            continue;
        } else if now == old && (now_mode == old_mode || now_mode == mode) {
            actions.push(Action {
                kind: if new.is_none() { "deleted" } else { "applied" }.into(),
                path,
                target,
                mode: if new.is_none() {
                    None
                } else {
                    resolved_mode(old_mode.as_deref(), mode.as_deref(), now_mode.as_deref())
                },
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
            let scratch = target_root
                .parent()
                .unwrap_or(target_root)
                .join(format!(".merge-int-{}-{}", std::process::id(), now_ns()));
            fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
            let minefile = scratch.join("mine");
            let basefile = scratch.join("base");
            let theirfile = scratch.join("theirs");
            write(&minefile, mine).map_err(|e| format!("{}: {e}", minefile.display()))?;
            write(&basefile, base).map_err(|e| format!("{}: {e}", basefile.display()))?;
            write(&theirfile, theirs).map_err(|e| format!("{}: {e}", theirfile.display()))?;
            let status = git_command()
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
            let mut clean = status.success();
            if !clean && matches_rule(&path, &append_only) {
                write(&minefile, mine).map_err(|e| format!("{}: {e}", minefile.display()))?;
                clean = git_command()
                    .args(["merge-file", "--union"])
                    .args([&minefile, &basefile, &theirfile])
                    .status()
                    .map_err(|e| e.to_string())?
                    .success();
            }
            let merged = fs::read(&minefile).map_err(|e| format!("{}: {e}", minefile.display()))?;
            let _ = fs::remove_dir_all(&scratch);
            if clean {
                actions.push(Action {
                    kind: "merged".into(),
                    path,
                    target,
                    mode: resolved_mode(old_mode.as_deref(), mode.as_deref(), now_mode.as_deref()),
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
            let target = target_root.join(path);
            if !origin.is_file()
                || through_symlink(target_root, &target)
                || target.is_symlink()
                || entry_mode(&target).as_deref() == Some("dir")
            {
                conflicts.push(path.clone());
            } else if !target.exists() {
                actions.push(Action {
                    kind: "copied".into(),
                    path: path.clone(),
                    target,
                    mode: entry_mode(&origin),
                    content: fs::read(&origin).map_err(|e| format!("{}: {e}", origin.display()))?,
                });
            } else if current(&target)
                != Some(fs::read(&origin).map_err(|e| format!("{}: {e}", origin.display()))?)
            {
                conflicts.push(path.clone());
            } else {
                matched.push(path.clone());
            }
        }
    }

    let mut marked_paths = Vec::new();
    for action in &actions {
        if action.kind != "copied"
            && !conflict_labels_in_bytes(Path::new(&action.path), &action.content).is_empty()
        {
            marked_paths.push(action.path.clone());
        }
    }

    let gen_inputs = generated_inputs(source_for_config)?;
    let touches_generated_inputs = !actions.is_empty()
        && !generated_paths.is_empty()
        && !generate_command.is_empty()
        && (gen_inputs.is_empty()
            || actions
                .iter()
                .any(|a| matches_rule(&a.path, &gen_inputs)));

    additions.retain(|path| {
        actions
            .iter()
            .any(|a| &a.path == path && a.kind != "deleted")
    });
    let numbered_warnings =
        numbered_conflicts(target_root, &additions, &actions, &generated_paths)?;

    let has_large = large.as_object().is_some_and(|m| !m.is_empty());

    if !conflicts.is_empty() || !marked_paths.is_empty() {
        return Ok(MergeDirOutcome {
            conflicts,
            marked_paths,
            numbered_warnings,
            touches_generated_inputs: false,
            generate_command,
            after_tree: after,
            has_large,
        });
    }

    for a in actions {
        let op = match a.kind.as_str() {
            "deleted" => fs::remove_file(&a.target)
                .map_err(|e| e.to_string())
                .map(|()| remove_empty_parents(&a.target, target_root)),
            "copied" => {
                if let Some(mode) = a.mode.as_deref().filter(|m| *m == "100755" || *m == "100644") {
                    write_entry(&a.target, mode, &a.content)
                } else {
                    fs::create_dir_all(a.target.parent().unwrap_or(target_root))
                        .map_err(|e| format!("merge {}: {e}", a.target.display()))?;
                    write(&a.target, &a.content)
                }
            }
            _ => {
                if let Some(mode) = a.mode.as_deref() {
                    write_entry(&a.target, mode, &a.content)
                } else {
                    write(&a.target, &a.content)
                }
            }
        };
        if let Err(e) = op {
            return Err(format!("merge {}: {e}", a.target.display()));
        }
    }

    let _ = matched;
    Ok(MergeDirOutcome {
        conflicts,
        marked_paths,
        numbered_warnings,
        touches_generated_inputs,
        generate_command,
        after_tree: after,
        has_large,
    })
}

fn apply_checked(
    run: &Path,
    merge: bool,
    dry: bool,
    keep_commits: bool,
    commit_mode: bool,
    allow_dirty_overlap: bool,
    written_paths: &[String],
) -> Res<ApplyOutcome> {
    let apply_started_at = iso();
    let apply_t0 = std::time::Instant::now();
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
        "operation":"apply","apply":{"ok":false,"dryRun":dry},"startedAt":apply_started_at});
    if let Some(sources) = json(run.join("meta.json")).get("configSources") {
        conclusion["configSources"] = sources.clone();
    }
    let meta = json(run.join("meta.json"));
    let source = Path::new(s(&meta["worktree"], "source"));
    let worktree = Path::new(s(&meta["worktree"], "path"));
    let commits = collect_commits(run, &meta)?;
    conclusion["commits"] = json!(commits);
    let uncommitted = crate::changes::uncommitted(&meta)?;
    if !uncommitted.is_empty() {
        conclusion["commitsDiffer"] = json!(uncommitted);
        eprintln!(
            "delegate: commits differ from the final tree: {}",
            uncommitted.join(", ")
        );
    }
    let blind = crate::output::blind_files(
        &json(run.join("summary.json"))["files"],
        &meta["acceptBlind"],
    );
    if !blind.is_empty() {
        conclusion["acceptBlind"] = json!(blind);
    }
    if !crate::output::json_enabled() {
        for item in &commits {
            println!(
                " commit {} {} ({} files)",
                &s(item, "hash")[..7],
                s(item, "title"),
                n(item, "files")
            );
        }
    }
    if keep_commits && !uncommitted.is_empty() {
        return Err(format!("--keep-commits requires committed worktree changes; commits differ from the final tree: {}", uncommitted.join(", ")));
    }
    let inner = if keep_commits && !commits.is_empty() {
        let mut kc_dirty_overlap: Vec<String> = Vec::new();
        if !allow_dirty_overlap && !merge && source.is_dir() {
            let dirty_paths = source_dirty_paths(source);
            if !dirty_paths.is_empty() {
                let mut kc_touched = std::collections::BTreeSet::new();
                for item in &commits {
                    let hash = s(item, "hash");
                    let touched = git(
                        worktree,
                        &[
                            "diff-tree",
                            "--no-commit-id",
                            "--name-only",
                            "--no-renames",
                            "-r",
                            "-z",
                            hash,
                        ],
                    )
                    .unwrap_or_default();
                    for path in touched.split(|b| *b == 0).filter(|p| !p.is_empty()) {
                        kc_touched.insert(String::from_utf8_lossy(path).into_owned());
                    }
                }
                for path in kc_touched {
                    if dirty_path_overlaps(&path, &dirty_paths) {
                        kc_dirty_overlap.push(path);
                    }
                }
            }
        }
        if !kc_dirty_overlap.is_empty() {
            record_dirty_overlap_failure(run, &meta, source, &kc_dirty_overlap, &mut conclusion)
        } else if dry {
            ApplyInnerResult {
                code: 0,
                written: vec![],
                regenerated: vec![],
                had_dirty_merge: false,
            }
        } else {
            let mut written_set = std::collections::BTreeSet::new();
            let mut applied_commits = Vec::new();
            let mut failed_commit: Option<(String, String)> = None;
            let code = {
                let _guard = crate::source_watch::guard(source, true)?;
                let before = crate::source_watch::fingerprint(source)?;
                let mut code = 0;
                for item in &commits {
                    let hash = s(item, "hash");
                    let title = s(item, "title");
                    let touched = git(
                        worktree,
                        &[
                            "diff-tree",
                            "--no-commit-id",
                            "--name-only",
                            "--no-renames",
                            "-r",
                            "-z",
                            hash,
                        ],
                    )
                    .unwrap_or_default();
                    for path in touched.split(|b| *b == 0).filter(|p| !p.is_empty()) {
                        written_set.insert(String::from_utf8_lossy(path).into_owned());
                    }
                    let status = git_command()
                        .arg("-C")
                        .arg(source)
                        .args(["cherry-pick", "--no-edit", hash])
                        .status()
                        .map_err(|e| e.to_string())?;
                    if !status.success() {
                        eprintln!("delegate: cherry-pick stopped at {hash}; resolve conflicts in {}, then continue with git cherry-pick --continue", source.display());
                        failed_commit = Some((hash.to_string(), title.to_string()));
                        let has_unmerged = git(source, &["diff", "--name-only", "--diff-filter=U"])
                            .map(|out| !out.trim_ascii().is_empty())
                            .unwrap_or(false);
                        code = if has_unmerged { EXIT_CONFLICT } else { 1 };
                        break;
                    }
                    applied_commits.push(hash.to_string());
                }
                crate::source_watch::register(source, &before, written_paths)?;
                code
            };
            let written: Vec<String> = written_set.into_iter().collect();
            if code == 0 {
                let at = iso();
                let head = git_text(source, &["rev-parse", "HEAD"])
                    .ok()
                    .unwrap_or_default();
                let state = json!({"at":at,"head":head,"tree":s(&meta["base"],"tree")});
                write_json(run.join(".applied"), &state)?;
                let mut updated = json(run.join("meta.json"));
                updated["appliedAt"] = json!(at);
                if !head.is_empty() {
                    updated["appliedHead"] = json!(head);
                }
                let _ = write_json(run.join("meta.json"), &updated);
                crate::refs::pin_slot(run, &meta, "applied", s(&meta["base"], "tree"));
                crate::premerge::spawn_worker_force(source);
            } else if let Some((hash, title)) = failed_commit {
                let short = &hash[..7.min(hash.len())];
                conclusion["apply"]["failedStep"] = json!("cherry-pick");
                conclusion["apply"]["failureDetail"] = json!(format!(
                    "cherry-pick stopped at {short} {title} ({} commit(s) already applied)",
                    applied_commits.len()
                ));
                conclusion["apply"]["writtenFiles"] = json!(&written);
            }
            ApplyInnerResult {
                code,
                written,
                regenerated: vec![],
                had_dirty_merge: false,
            }
        }
    } else if source.is_absolute() && !meta["worktree"].is_null() {
        match crate::premerge::validate_fast_forward(run, source) {
            crate::premerge::FastForwardCheck::Hit(plan) => {
                if let Some(ff_res) =
                    crate::premerge::apply_fast_forward(run, source, &plan, dry, &mut conclusion)?
                {
                    ff_res
                } else {
                    conclusion["fastForward"] = json!({
                        "hit": false,
                        "missCode": "path_validation",
                        "reason": "预合并树路径校验未通过，走三方合并",
                    });
                    if !dry && crate::premerge::cancel_for_run(run, source) {
                        conclusion["applyTiming"]["premergeCancelled"] = json!(true);
                    }
                    apply_inner(run, merge, dry, allow_dirty_overlap, &mut conclusion)?
                }
            }
            crate::premerge::FastForwardCheck::Miss(miss) => {
                conclusion["fastForward"] = json!({
                    "hit": false,
                    "missCode": miss.code(),
                    "reason": miss.human_line(),
                });
                if !dry && crate::premerge::cancel_for_run(run, source) {
                    conclusion["applyTiming"]["premergeCancelled"] = json!(true);
                }
                apply_inner(run, merge, dry, allow_dirty_overlap, &mut conclusion)?
            }
        }
    } else {
        apply_inner(run, merge, dry, allow_dirty_overlap, &mut conclusion)?
    };
    let mut code = inner.code;
    let written_files = inner.written;
    let regenerated_files = inner.regenerated;
    if !dry && code == 0 {
        let mut all_touched = std::collections::BTreeSet::<String>::new();
        if let Some(chain_files) = chain_net_files(run, &meta) {
            for p in chain_files {
                all_touched.insert(p);
            }
        }
        for p in &written_files {
            all_touched.insert(p.clone());
        }
        for p in &regenerated_files {
            all_touched.insert(p.clone());
        }
        if all_touched.is_empty() {
            for p in run_files(run) {
                all_touched.insert(p);
            }
        }
        let applied_files_vec: Vec<String> = all_touched.into_iter().collect();
        if !applied_files_vec.is_empty() {
            let _ = write(
                run.join("applied-files.txt"),
                format!("{}\n", applied_files_vec.join("\n")),
            );
            let applied_path = run.join(".applied");
            if applied_path.is_file() {
                let mut st = json(&applied_path);
                if st.is_object() {
                    st["files"] = json!(&applied_files_vec);
                    let _ = write_json(&applied_path, &st);
                }
            }
        }
    }
    if keep_commits {
        conclusion["apply"]["keepCommits"] = json!(true);
    }
    if !dry {
        let marked = marked_files(source, written_paths)?;
        if !marked.is_empty() {
            conclusion["apply"]["conflictMarkers"] = json!(marked);
            if s(&conclusion["apply"], "failedStep").is_empty() {
                conclusion["apply"]["failedStep"] = json!("conflict-markers");
                conclusion["apply"]["failureDetail"] = json!(format!(
                    "{} file(s) contain conflict markers: {}",
                    marked.len(),
                    marked.join(", ")
                ));
            }
            if conclusion["apply"].get("writtenFiles").is_none() {
                conclusion["apply"]["writtenFiles"] = json!(&written_files);
            }
            let mut summary = json(run.join("summary.json"));
            summary["conflictMarkers"] = json!(marked);
            write_json(run.join("summary.json"), &summary)?;
            code = EXIT_CONFLICT;
        }
    }
    if code != 0 {
        if s(&conclusion["apply"], "failedStep").is_empty() {
            conclusion["apply"]["failedStep"] = json!("apply");
            conclusion["apply"]["failureDetail"] = json!("apply returned non-zero");
        }
        if conclusion["apply"].get("writtenFiles").is_none() {
            conclusion["apply"]["writtenFiles"] = json!(&written_files);
        }
    }
    conclusion["apply"]["ok"] = json!(code == 0);
    conclusion["finishedAt"] = json!(iso());
    conclusion["elapsedMs"] = json!(apply_t0.elapsed().as_millis() as u64);
    accept_validity(run, dry, &mut conclusion);
    let marked = strings(&conclusion["apply"]["conflictMarkers"]);
    let mut total_uncommitted: std::collections::BTreeSet<&str> =
        written_files.iter().map(String::as_str).collect();
    total_uncommitted.extend(regenerated_files.iter().map(String::as_str));
    let uncommitted_count = total_uncommitted.len();
    let commit_titles: Vec<&str> = commits
        .iter()
        .map(|c| s(c, "title"))
        .filter(|t| !t.is_empty())
        .collect();
    apply_progress(if code == 0 {
        if !dry && !keep_commits && !commit_mode && uncommitted_count > 0 {
            if commit_titles.is_empty() {
                format!("apply complete / 应用完成 · 改动未提交：{uncommitted_count} 个文件")
            } else {
                format!(
                    "apply complete / 应用完成 · 改动未提交：{uncommitted_count} 个文件；同事原提交：{}",
                    commit_titles.join(" / ")
                )
            }
        } else {
            "apply complete / 应用完成".to_string()
        }
    } else if !marked.is_empty() {
        format!(
            "applied with conflict markers / 已应用，{} 个文件有冲突标记待解决: {}",
            marked.len(),
            marked.join(", ")
        )
    } else {
        let step = s(&conclusion["apply"], "failedStep");
        let detail = s(&conclusion["apply"], "failureDetail");
        let written_n = strings(&conclusion["apply"]["writtenFiles"]).len();
        format!(
            "apply failed at {step} / 应用失败（{detail}；已写入源工作区 {written_n} 个文件）"
        )
    });
    Ok(ApplyOutcome {
        code,
        conclusion,
        written_files,
        regenerated_files,
        had_dirty_merge: inner.had_dirty_merge,
        already_applied: false,
    })
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
    } else if b(accept, "notExecuted") || b(accept, "infrastructureFailure") {
        conclusion["acceptPending"] = json!(true);
        "acceptance not established: queue/transport interrupted; retry or run locally".into()
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
/// The acceptance a run recorded: its `accept` (with `--accept-also` folded in) and any
/// `acceptAlso` kept apart, without repeats.
pub fn acceptance(meta: &Value) -> Vec<String> {
    let mut commands = Vec::new();
    let accept = s(meta, "accept").trim();
    if !accept.is_empty() {
        commands.push(accept.to_string());
    }
    for command in strings(&meta["acceptAlso"]) {
        if !command.trim().is_empty() && !commands.contains(&command) {
            commands.push(command);
        }
    }
    commands
}
/// Post-merge acceptance on the source tree (field notes 16): parallel routes each pass in
/// their own worktree, yet type drift between them only shows once merged. When a run's
/// acceptance does not carry over, `apply` reruns it there: `applyVerify` in
/// `.delegate.json` as a command replaces it, `false` turns it off (`--verify` /
/// `--no-verify` override). Returns the commands to run; `apply` runs the union of a
/// batch once, after its last merge.
pub fn verify_plan(run: &Path, forced: Option<bool>, conclusion: &mut Value) -> Res<Vec<String>> {
    if conclusion["apply"]["ok"] != json!(true) || conclusion["apply"]["dryRun"] == json!(true) {
        return Ok(vec![]);
    }
    let meta = json(run.join("meta.json"));
    let source = s(&meta["worktree"], "source");
    if source.is_empty() {
        return Ok(vec![]);
    }
    let file = Path::new(source).join(".delegate.json");
    let raw = if file.is_file() {
        serde_json::from_str::<Value>(&read(&file))
            .map_err(|e| format!("cannot read {}: {e}", file.display()))?
    } else {
        json!({})
    };
    let configured = match raw.get("applyVerify") {
        None | Some(Value::Null) | Some(Value::Bool(true)) => Some(None),
        Some(Value::Bool(false)) => None,
        Some(Value::String(command)) if !command.trim().is_empty() => Some(Some(command.clone())),
        _ => {
            return Err(format!(
                "{}: applyVerify must be true, false or a command",
                file.display()
            ))
        }
    };
    if !forced.unwrap_or(configured.is_some()) {
        if configured.is_none() && forced != Some(true) {
            conclusion["applyVerifyDisabled"] = json!(true);
        }
        return Ok(vec![]);
    }
    if forced != Some(true) && conclusion["acceptStillValid"] == json!(true) {
        conclusion["verify"] = json!({"skipped":"acceptStillValid"});
        return Ok(vec![]);
    }
    let mut commands = match configured.flatten() {
        Some(command) => vec![command],
        None => acceptance(&meta),
    };
    if commands.is_empty() {
        if let Some(command) = raw.get("accept").and_then(Value::as_str) {
            commands.push(command.to_string());
        }
    }
    if commands.is_empty() {
        conclusion["verify"] = json!({"skipped":"no acceptance command"});
    }
    Ok(commands)
}
/// Runs `commands` in order on the merged source tree through the heavy lane, stopping
/// at the first failure; `run` holds the log and supplies the environment.
pub fn run_verify(run: &Path, commands: &[String]) -> Value {
    let meta = json(run.join("meta.json"));
    let tree = &meta["worktree"];
    let cwd = Some(s(tree, "sourceWorkdir"))
        .filter(|w| !w.is_empty())
        .unwrap_or(s(tree, "source"))
        .to_string();
    let label = format!(
        "verify {}",
        run.file_name().unwrap_or_default().to_string_lossy()
    );
    apply_progress("verifying merged tree / 正在验收合并结果");
    let verify_cmds: Vec<&str> = commands.iter().map(String::as_str).collect();
    let slot = lane::acquire_shell_for_commands(&label, &verify_cmds, None, Some(run), |ahead, _| {
        apply_progress(format!("verify queued / 排队中: {ahead} ahead"))
    });
    let retry = format!("验收未完成（排队/基础设施）；重排：{} apply --verify --keep {}；或本地：DELEGATE_REMOTE=0 {} lane {}",
        shell_quote(&script().to_string_lossy()), s(&meta, "run"), shell_quote(&script().to_string_lossy()), shell_quote(&commands.join(" && ")));
    let mut result = json!({"commands":commands,"notExecuted":true,"next":retry});
    let Ok(slot) = slot else {
        result["ok"] = json!(false);
        result["tail"] = json!("stopped while queued for the heavy lane");
        return result;
    };
    let path = run.join("verify.log");
    result["log"] = json!(path);
    let Ok(mut log) = fs::File::create(&path) else {
        result["ok"] = json!(false);
        result["tail"] = json!(format!("cannot create {}", path.display()));
        return result;
    };
    result.as_object_mut().unwrap().remove("notExecuted");
    result.as_object_mut().unwrap().remove("next");
    let mut code = 0;
    let accept_timeout = meta["acceptTimeoutSeconds"].as_f64().filter(|t| *t > 0.0);
    for command in commands {
        let _ = writeln!(log, "$ {command}");
        let (mut exit, mut timed) = crate::common::run_shell(
            (command, "verify"),
            Path::new(&cwd),
            run,
            &meta["env"],
            accept_timeout,
            &mut log,
            None,
        )
        .unwrap_or((1, false));
        if exit == 75 {
            let remote_status = json(run.join("verify.remote.json"));
            if s(&remote_status, "stage") == "missing_managed" {
                let reason = s(&remote_status, "reason").to_string();
                let paths = remote_status["paths"].clone();
                let _ = writeln!(
                    log,
                    "\n[remote missing managed paths: {reason}; rerunning verify locally]"
                );
                let _ = log.flush();
                (exit, timed) = crate::common::run_shell_with(
                    (command, "verify"),
                    Path::new(&cwd),
                    run,
                    &meta["env"],
                    accept_timeout,
                    &mut log,
                    None,
                    true,
                )
                .unwrap_or((1, false));
                result["localFallback"] = json!({"reason": reason, "paths": paths});
            }
        }
        let _ = writeln!(
            log,
            "{}\n[exit {exit}]",
            if timed { "\n[verify timed out]" } else { "" }
        );
        if exit != 0 {
            result.as_object_mut().unwrap().extend(shell_outcome(run, "verify", exit, timed).as_object().unwrap().clone());
            code = exit;
            result["command"] = json!(command);
            break;
        }
    }
    drop(slot);
    result["ok"] = json!(code == 0);
    result["exitCode"] = json!(code);
    if code != 0 {
        let text = read(&path);
        let chars: Vec<char> = text.trim().chars().collect();
        result["tail"] = json!(chars[chars.len().saturating_sub(1500)..]
            .iter()
            .collect::<String>());
        result["next"] = if b(&result, "notExecuted") || b(&result, "infrastructureFailure") {
            json!(retry)
        } else {
            json!("the merge is already in the source tree: fix it there, or reply to the task with the failure")
        };
    }
    apply_progress(if code == 0 {
        "verify passed / 验收通过"
    } else {
        "verify failed / 验收失败"
    });
    result
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
fn path_exists_in_rev(repo: &Path, rev: &str, path: &str) -> bool {
    if rev.is_empty() || path.is_empty() || !repo.is_dir() {
        return false;
    }
    git_command()
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "-e", &format!("{rev}:{path}")])
        .status()
        .is_ok_and(|st| st.success())
}

/// Numbered files (`0148_name.sql`) that exist only as uncommitted work in the source
/// checkout or as net additions (relative to `netBase` / effective chain base) in another
/// unapplied run's worktree. Files already present on mainline (`source HEAD`) or synced
/// from mainline are never reported as occupied by another run.
pub fn numbers_taken(source: &Path, own: &Path) -> Vec<(String, String, String)> {
    use std::collections::BTreeSet;
    let mut taken = BTreeSet::new();

    // 1. Uncommitted newly added numbered files in the source checkout (excluding anything in HEAD).
    if let Ok(out) = git_command()
        .arg("-C")
        .arg(source)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
    {
        for entry in out.stdout.split(|b| *b == 0) {
            let entry = String::from_utf8_lossy(entry);
            let Some((status, path)) = entry.split_at_checked(3) else {
                continue;
            };
            if !(status.starts_with("??") || status.starts_with('A')) {
                continue;
            }
            if let Some((directory, prefix)) = numbered_prefix(path) {
                if !path_exists_in_rev(source, "HEAD", path) {
                    taken.insert((directory, prefix, "source checkout".to_string()));
                }
            }
        }
    }

    // 2. Other unapplied, non-superseded runs on the same source: count ONLY net newly added
    //    numbered files relative to the run's netBase / effective chain base, and exclude
    //    anything already present on mainline (`source HEAD`).
    let all = crate::runs::all_runs();
    let superseded_parents: BTreeSet<String> = all
        .iter()
        .filter_map(|r| {
            let p = s(&json(r.join("meta.json")), "parent").to_string();
            (!p.is_empty()).then_some(p)
        })
        .collect();
    let mut seen_paths: BTreeSet<PathBuf> = BTreeSet::new();
    seen_paths.insert(own.to_path_buf());
    seen_paths.insert(source.to_path_buf());

    for run in all {
        let meta = json(run.join("meta.json"));
        let wt_path = PathBuf::from(s(&meta["worktree"], "path"));
        if Path::new(s(&meta["worktree"], "source")) != source
            || !wt_path.is_dir()
            || wt_path == own
            || wt_path == source
        {
            continue;
        }
        if run.join(".applied").exists()
            || run.join(".applied-cleaned").exists()
            || b(&meta, "appliedCleaned")
        {
            continue;
        }
        let run_id = crate::events::run_id_for(&run, &meta);
        if superseded_parents.contains(&run_id) {
            continue;
        }
        if !seen_paths.insert(wt_path.clone()) {
            continue;
        }
        let label = s(&meta, "name").to_string();
        let base_state = effective_chain_base(&run, &meta, &wt_path);
        let base_tree = s(&base_state, "tree").to_string();
        let net_base = s(&meta, "netBase").to_string();
        let base_rev = if !base_tree.is_empty() {
            base_tree
        } else if !net_base.is_empty() {
            net_base.clone()
        } else {
            let wb = read(run.join("worktree-base-commit")).trim().to_string();
            if !wb.is_empty() { wb } else { "HEAD".to_string() }
        };

        let mut added_paths: BTreeSet<String> = BTreeSet::new();
        // Tracked additions (committed or staged/working-tree) relative to `base_rev`.
        if let Ok(out) = git_command()
            .arg("-C")
            .arg(&wt_path)
            .args(["diff", "--name-only", "--diff-filter=A", "-z", &base_rev])
            .output()
        {
            if out.status.success() {
                for raw in out.stdout.split(|b| *b == 0) {
                    let p = String::from_utf8_lossy(raw);
                    if !p.is_empty() {
                        added_paths.insert(p.into_owned());
                    }
                }
            }
        }
        // Untracked files in the worktree (must not already exist in `base_rev`).
        if let Ok(out) = git_command()
            .arg("-C")
            .arg(&wt_path)
            .args(["ls-files", "--others", "--exclude-standard", "-z"])
            .output()
        {
            if out.status.success() {
                for raw in out.stdout.split(|b| *b == 0) {
                    let p = String::from_utf8_lossy(raw);
                    if !p.is_empty() && !path_exists_in_rev(&wt_path, &base_rev, &p) {
                        added_paths.insert(p.into_owned());
                    }
                }
            }
        }

        for path in added_paths {
            let Some((directory, prefix)) = numbered_prefix(&path) else {
                continue;
            };
            // Exclude anything already committed on source mainline or in the run's netBase.
            if path_exists_in_rev(source, "HEAD", &path) {
                continue;
            }
            if !net_base.is_empty()
                && (path_exists_in_rev(&wt_path, &net_base, &path)
                    || path_exists_in_rev(source, &net_base, &path))
            {
                continue;
            }
            taken.insert((directory, prefix, label.clone()));
        }
    }
    taken.into_iter().collect()
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
/// Stage only the applied paths and create one squash commit on the source checkout.
/// Fails closed if the source index already has staged changes or a 3-way merged file
/// contained pre-existing local uncommitted edits.
pub fn commit_applied(
    source: &Path,
    runs: &[PathBuf],
    paths: &[String],
    titles: &[String],
    run_names: &[String],
    trailer_runs: &[String],
    had_dirty_merge: bool,
) -> Res<(String, String)> {
    if paths.is_empty() {
        return Err("no written files to commit".into());
    }
    if had_dirty_merge {
        return Err(
            "refusing --commit: a 3-way merged file already had uncommitted local edits in the source workspace; review and commit manually"
                .into(),
        );
    }
    let cached = git_command()
        .arg("-C")
        .arg(source)
        .args(["diff", "--cached", "--quiet"])
        .status()
        .map_err(|e| e.to_string())?;
    if !cached.success() {
        return Err(
            "refusing --commit: source index already has staged changes; commit or unstage them first"
                .into(),
        );
    }
    let (subject, mut message) = if titles.len() == 1 {
        (titles[0].clone(), titles[0].clone())
    } else if titles.len() > 1 {
        let body = titles
            .iter()
            .map(|t| format!("- {t}"))
            .collect::<Vec<_>>()
            .join("\n");
        (titles[0].clone(), format!("{}\n\n{body}", titles[0]))
    } else {
        let fallback = format!("delegate: apply {}", run_names.join(", "));
        (fallback.clone(), fallback)
    };
    let mut trailers = Vec::new();
    for rid in trailer_runs {
        let rid = rid.trim();
        let line = format!("Delegate-Run: {rid}");
        if !rid.is_empty()
            && !trailers.contains(&line)
            && !message.lines().any(|l| l.trim().eq_ignore_ascii_case(&line))
        {
            trailers.push(line);
        }
    }
    if !trailers.is_empty() {
        message.push_str("\n\n");
        message.push_str(&trailers.join("\n"));
        message.push('\n');
    }
    let head = {
        let _guard = crate::source_watch::guard(source, true)?;
        let add = git_command()
            .arg("-C")
            .arg(source)
            .args(["add", "-A", "--"])
            .args(paths)
            .output()
            .map_err(|e| e.to_string())?;
        if !add.status.success() {
            return Err(format!(
                "git add failed: {}",
                String::from_utf8_lossy(&add.stderr).trim()
            ));
        }
        let commit = git_command()
            .arg("-C")
            .arg(source)
            .args(["commit", "-m", &message])
            .output()
            .map_err(|e| e.to_string())?;
        if !commit.status.success() {
            let _ = git_command()
                .arg("-C")
                .arg(source)
                .args(["reset", "-q", "--"])
                .args(paths)
                .status();
            let detail = String::from_utf8_lossy(&commit.stderr);
            let detail = if detail.trim().is_empty() {
                String::from_utf8_lossy(&commit.stdout).trim().to_string()
            } else {
                detail.trim().to_string()
            };
            return Err(format!("git commit failed: {detail}"));
        }
        git_text(source, &["rev-parse", "HEAD"]).unwrap_or_default()
    };
    if !head.is_empty() {
        for run in runs {
            let applied_path = run.join(".applied");
            if applied_path.is_file() {
                let mut state = json(&applied_path);
                if state.is_object() {
                    state["head"] = json!(&head);
                    let _ = write_json(&applied_path, &state);
                }
            }
            let meta_path = run.join("meta.json");
            if meta_path.is_file() {
                let mut m = json(&meta_path);
                if m.is_object() {
                    m["appliedHead"] = json!(&head);
                    let _ = write_json(&meta_path, &m);
                }
            }
        }
    }
    Ok((head, subject))
}
fn apply_inner(
    run: &Path,
    merge: bool,
    dry: bool,
    allow_dirty_overlap: bool,
    conclusion: &mut Value,
) -> Res<ApplyInnerResult> {
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
    let (total_base, _) = effective_total_base(&top, &meta);
    let mut before = if !s(&applied, "tree").is_empty() {
        s(&applied, "tree").to_string()
    } else {
        total_base.clone()
    };
    let source = PathBuf::from(s(&meta["worktree"], "source"));
    let (generated_paths, generate_command) = generated(&source)?;
    let append_only = append_only(&source)?;
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
    let missing = crate::refs::missing_in_top(
        &top,
        run,
        &meta,
        &[("base", &before), ("after", &after)],
    );
    if !missing.is_empty() {
        return Err(crate::refs::recovery_message(run, &meta, &missing));
    }
    let mut changes = tree_changes(
        &top,
        &json!({"tree":before,"large":applied["large"]}),
        &json!({"tree":after,"large":large}),
        false,
    )?;
    if changes.is_empty() && !s(&applied, "tree").is_empty() {
        // A repeated apply still detects drift in files merged in an earlier round.
        before = total_base.clone();
        changes = tree_changes(&top, &json!({"tree":before}), &json!({"tree":after}), false)?;
    }
    let allow_dirty = allow_dirty_overlap || merge;
    let dirty_paths = if allow_dirty {
        Vec::new()
    } else {
        source_dirty_paths(&source)
    };
    let mut dirty_overlap = vec![];
    let mut actions = vec![];
    let mut matched = vec![];
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
            matched.push(path);
            continue;
        } else {
            let own_prev_applied = !s(&applied, "tree").is_empty()
                && before != total_base
                && now == old
                && (now_mode == old_mode || now_mode == mode);
            if !allow_dirty && !own_prev_applied && dirty_path_overlaps(&path, &dirty_paths) {
                dirty_overlap.push(path);
                continue;
            }
            if now == old && (now_mode == old_mode || now_mode == mode) {
                actions.push(Action {
                    kind: if new.is_none() { "deleted" } else { "applied" }.into(),
                    path,
                    target,
                    mode: if new.is_none() {
                        None
                    } else {
                        resolved_mode(old_mode.as_deref(), mode.as_deref(), now_mode.as_deref())
                    },
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
                let status = git_command()
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
                let mut clean = status.success();
                if !clean && matches_rule(&path, &append_only) {
                    write(&minefile, mine).map_err(|e| format!("{}: {e}", minefile.display()))?;
                    clean = git_command()
                        .args(["merge-file", "--union"])
                        .args([&minefile, &basefile, &theirfile])
                        .status()
                        .map_err(|e| e.to_string())?
                        .success();
                    if clean {
                        apply_progress(format!(
                            "kept both sides / 两边都保留（appendOnly）: {path}"
                        ));
                    }
                }
                let merged = fs::read(&minefile).map_err(|e| format!("{}: {e}", minefile.display()))?;
                fs::remove_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
                let merged_mode =
                    resolved_mode(old_mode.as_deref(), mode.as_deref(), now_mode.as_deref());
                if clean {
                    actions.push(Action {
                        kind: "merged".into(),
                        path,
                        target,
                        mode: merged_mode,
                        content: merged,
                    });
                } else if merge {
                    actions.push(Action {
                        kind: "conflict-markers".into(),
                        path,
                        target,
                        mode: merged_mode,
                        content: merged,
                    });
                } else {
                    conflicts.push(path);
                }
            } else {
                conflicts.push(path);
            }
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
                && before != total_base
                && applied["large"].get(path) == obj.get(path)
                && applied["largeHashes"].get(path).and_then(Value::as_str)
                    == Some(file_hash(&origin)?.as_str())
            {
                continue;
            } else if current(&target)
                == Some(fs::read(&origin).map_err(|e| format!("{}: {e}", origin.display()))?)
            {
                matched.push(path.clone());
            } else if !allow_dirty
                && !(target.exists()
                    && !s(&applied, "tree").is_empty()
                    && before != total_base
                    && applied["largeHashes"].get(path).and_then(Value::as_str)
                        == Some(file_hash(&target)?.as_str()))
                && dirty_path_overlaps(path, &dirty_paths)
            {
                dirty_overlap.push(path.clone());
            } else if !target.exists()
                || applied["largeHashes"].get(path).and_then(Value::as_str)
                    == Some(file_hash(&target)?.as_str())
            {
                actions.push(Action {
                    kind: "copied".into(),
                    path: path.clone(),
                    target,
                    mode: entry_mode(&origin),
                    content: fs::read(&origin).map_err(|e| format!("{}: {e}", origin.display()))?,
                });
            } else {
                conflicts.push(path.clone());
            }
        }
    }
    if !dirty_overlap.is_empty() {
        dirty_overlap.sort();
        dirty_overlap.dedup();
        return Ok(record_dirty_overlap_failure(
            run,
            &meta,
            &source,
            &dirty_overlap,
            conclusion,
        ));
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
        // Still record this round (including zero-change replies) as applied.
    }
    if !conflicts.is_empty() && !merge {
        let run_id = {
            let r = s(&meta, "run");
            if !r.is_empty() {
                r.to_string()
            } else {
                run.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }
        };
        let files_preview = if conflicts.len() <= 5 {
            conflicts.join(", ")
        } else {
            format!("{} 等 {} 个文件", conflicts[..5].join(", "), conflicts.len())
        };
        let rebase_cmd = format!(
            "delegate reply --sync=agent {run_id} \"rebase 到最新主干并解决冲突（{files_preview}）\""
        );
        let merge_cmd = format!("{} apply --merge {run_id}", script().display());
        for p in &conflicts {
            println!(" conflict         {p}");
        }
        eprintln!(
            "delegate: nothing applied; {} file(s) were also changed in {} since the run started.\n  下一步（推荐，交同事 rebase 解决冲突）：{rebase_cmd}\n  或本地合并写入冲突标记：{merge_cmd}\n  或查看改动：{} diff {run_id} --total",
            conflicts.len(),
            source.display(),
            script().display(),
        );
        conclusion["apply"]["failedStep"] = json!("conflicts");
        conclusion["apply"]["failureDetail"] = json!(format!(
            "{} file(s) also changed in source: {}",
            conflicts.len(),
            conflicts.join(", ")
        ));
        conclusion["apply"]["conflicts"] = json!(conflicts);
        conclusion["apply"]["writtenFiles"] = json!([]);
        conclusion["apply"]["next"] = json!([rebase_cmd, merge_cmd]);
        conclusion["next"] = json!(rebase_cmd);
        return Ok(ApplyInnerResult {
            code: EXIT_CONFLICT,
            written: vec![],
            regenerated: vec![],
            had_dirty_merge: false,
        });
    }
    for action in &mut actions {
        if action.kind != "copied"
            && !conflict_labels_in_bytes(Path::new(&action.path), &action.content).is_empty()
        {
            action.kind = "conflict-markers".into();
        }
    }
    let marked_paths = actions
        .iter()
        .filter(|x| {
            x.kind == "conflict-markers"
                || !conflict_labels_in_bytes(Path::new(&x.path), &x.content).is_empty()
        })
        .map(|x| x.path.clone())
        .collect::<Vec<_>>();
    let marked = !marked_paths.is_empty();
    if marked {
        conclusion["apply"]["conflictMarkers"] = json!(marked_paths);
        if !dry {
            let mut summary = json(run.join("summary.json"));
            summary["conflictMarkers"] = json!(marked_paths);
            write_json(run.join("summary.json"), &summary)?;
        }
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
    let mut written = Vec::with_capacity(actions.len());
    let mut had_dirty_merge = false;
    {
        // Hold the source-watch lock ONLY for the brief write-to-disk + fingerprint-register window
        // so concurrent `delegate start` snapshots are never blocked by regeneration or diffing.
        let _write_guard = if !dry && (!actions.is_empty() || !matched.is_empty()) {
            Some(crate::source_watch::guard(&source, true)?)
        } else {
            None
        };
        let before_actions = if _write_guard.is_some() {
            Some(crate::source_watch::fingerprint(&source)?)
        } else {
            None
        };
        if let Some(before) = &before_actions {
            had_dirty_merge = actions
                .iter()
                .any(|a| a.kind == "merged" && !before[&a.path].is_null());
        }
        for a in actions {
            if !dry {
                written.push(a.path.clone());
                let operation = match a.kind.as_str() {
                    "deleted" => fs::remove_file(&a.target)
                        .map_err(|e| e.to_string())
                        .map(|()| remove_empty_parents(&a.target, &source)),
                    "copied" => {
                        if let Some(mode) = a.mode.as_deref().filter(|m| *m == "100755" || *m == "100644") {
                            write_entry(&a.target, mode, &a.content)
                        } else {
                            fs::create_dir_all(a.target.parent().unwrap_or(&source))
                                .map_err(|e| format!("apply {}: {e}", a.target.display()))?;
                            write(&a.target, &a.content)
                        }
                    }
                    _ => {
                        if let Some(mode) = a.mode.as_deref() {
                            write_entry(&a.target, mode, &a.content)
                        } else {
                            write(&a.target, &a.content)
                        }
                    }
                };
                if let Err(e) = operation {
                    if let Some(before) = &before_actions {
                        let _ = crate::source_watch::register_matching(&source, before, &written, &[]);
                    }
                    return Err(format!("apply {}: {e}", a.target.display()));
                }
            }
            println!(" {:<16} {}", a.kind, a.path);
        }
        if let Some(before) = &before_actions {
            crate::source_watch::register_matching(&source, before, &written, &matched)?;
        }
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
    let mut regenerated = Vec::new();
    if dry {
        eprintln!("delegate: dry run; nothing written");
        if regenerate {
            println!(" regenerated      {} (dry run)", generated_paths.join(", "));
        }
    } else if regenerate {
        let interrupted = || -> Option<i32> {
            lane::stopped().then(|| {
                128 + lane::SIGNAL.load(std::sync::atomic::Ordering::SeqCst)
            })
        };
        if let Some(code) = interrupted() {
            eprintln!("delegate: regeneration interrupted before start");
            conclusion["apply"]["failedStep"] = json!("regeneration");
            conclusion["apply"]["failureDetail"] = json!("regeneration interrupted before start");
            conclusion["apply"]["writtenFiles"] = json!(&written);
            return Ok(ApplyInnerResult {
                code,
                written,
                regenerated,
                had_dirty_merge,
            });
        }
        let gen_queue_t0 = std::time::Instant::now();
        let _slot = match lane::acquire_shell_for_commands(
            &format!(
                "regenerate {}",
                run.file_name().unwrap_or_default().to_string_lossy()
            ),
            &[generate_command.as_str()],
            None,
            Some(run),
            |ahead, labels| {
                apply_progress(format!(
                    "generator queued / 生成器排队: {ahead} ahead; {}",
                    labels.join(", ")
                ))
            },
        ) {
            Ok(slot) => slot,
            Err(_) if lane::stopped() => {
                let code = 128 + lane::SIGNAL.load(std::sync::atomic::Ordering::SeqCst);
                eprintln!("delegate: regeneration interrupted while queued");
                conclusion["apply"]["failedStep"] = json!("regeneration");
                conclusion["apply"]["failureDetail"] = json!("regeneration interrupted while queued");
                conclusion["apply"]["writtenFiles"] = json!(&written);
                return Ok(ApplyInnerResult {
                    code,
                    written,
                    regenerated,
                    had_dirty_merge,
                });
            }
            Err(error) => return Err(error),
        };
        conclusion["applyTiming"]["generateQueueMs"] =
            json!(gen_queue_t0.elapsed().as_millis() as u64);
        let log = std::path::absolute(run.join("generate.log")).map_err(|e| e.to_string())?;
        let mut file =
            fs::File::create(&log).map_err(|e| format!("apply {}: {e}", log.display()))?;
        let timeout = seconds(&setting("GENERATE_TIMEOUT", "10m"))?;
        let mut gen_env = meta["env"].clone();
        if !gen_env.is_object() {
            gen_env = json!({});
        }
        let target_contended = crate::source_watch::cargo_target_busy(&source);
        let isolate_mode = setting("APPLY_ISOLATE_TARGET", "auto").to_ascii_lowercase();
        let explicit_target = gen_env
            .get("CARGO_TARGET_DIR")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.trim().is_empty())
            || std::env::var_os("CARGO_TARGET_DIR").is_some_and(|v| !v.is_empty());
        if target_contended {
            conclusion["applyTiming"]["targetContended"] = json!(true);
        }
        if !explicit_target && (isolate_mode == "always" || (isolate_mode != "never" && target_contended)) {
            let iso_target = cache_dir()
                .join("apply-target")
                .join(crate::premerge::repo_key(&source));
            let _ = fs::create_dir_all(&iso_target);
            gen_env["CARGO_TARGET_DIR"] = json!(iso_target.to_string_lossy());
            conclusion["applyTiming"]["regenerateTargetIsolated"] = json!(true);
            conclusion["applyTiming"]["regenerateTargetDir"] = json!(iso_target.to_string_lossy());
            apply_progress(format!(
                "主仓 target/ 正在被占用（疑似 post-commit/构建），本次 regenerate 改用独立 CARGO_TARGET_DIR={} 避让",
                iso_target.display()
            ));
        }
        apply_progress(format!(
            "generating / 正在生成; log: {}; execution timeout: {timeout}s (queue excluded)",
            log.display()
        ));
        let before_gen = {
            let _g = crate::source_watch::guard(&source, false)?;
            crate::source_watch::fingerprint(&source)?
        };
        let gen_t0 = std::time::Instant::now();
        let shell = run_shell(
            (&generate_command, "generate"),
            &source,
            run,
            &gen_env,
            Some(timeout),
            &mut file,
            None,
        );
        conclusion["applyTiming"]["generateMs"] = json!(gen_t0.elapsed().as_millis() as u64);
        let stopped = interrupted();
        let succeeded = stopped.is_none() && matches!(shell, Ok((0, false)));
        let reg = {
            let _g = crate::source_watch::guard(&source, true)?;
            crate::source_watch::register_matching(
                &source,
                &before_gen,
                &generated_paths,
                if succeeded { &generated_paths } else { &[] },
            )
        };
        let (code, timed) = shell.map_err(|e| format!("apply generated.command: {e}"))?;
        regenerated = reg?;
        if let Some(signal_code) = stopped {
            let _ = writeln!(file, "\n[interrupted]");
            eprintln!("delegate: regeneration interrupted; {}", log.display());
            conclusion["apply"]["failedStep"] = json!("regeneration");
            conclusion["apply"]["failureDetail"] =
                json!(format!("regeneration interrupted; log: {}", log.display()));
            conclusion["apply"]["generateLog"] = json!(log.display().to_string());
            conclusion["apply"]["writtenFiles"] = json!(&written);
            return Ok(ApplyInnerResult {
                code: signal_code,
                written,
                regenerated,
                had_dirty_merge,
            });
        }
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
            conclusion["apply"]["failedStep"] = json!("regeneration");
            conclusion["apply"]["failureDetail"] = json!(format!(
                "regeneration {} (exit {code}); log: {}",
                if timed { "timed out" } else { "failed" },
                log.display()
            ));
            conclusion["apply"]["generateExitCode"] = json!(code);
            conclusion["apply"]["generateLog"] = json!(log.display().to_string());
            conclusion["apply"]["writtenFiles"] = json!(&written);
            return Ok(ApplyInnerResult {
                code: 1,
                written,
                regenerated,
                had_dirty_merge,
            });
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
        let at = iso();
        let head = git_text(&source, &["rev-parse", "HEAD"])
            .ok()
            .unwrap_or_default();
        let state = json!({"at":at,"head":head,"tree":after,"large":large,"largeHashes":large_hashes});
        let mut chain = run.to_path_buf();
        loop {
            write_json(chain.join(".applied"), &state)?;
            write_json(chain.join(".sync-base"), &state)?;
            let mut chain_meta = json(chain.join("meta.json"));
            chain_meta["appliedAt"] = json!(at);
            if !head.is_empty() {
                chain_meta["appliedHead"] = json!(head);
            }
            let _ = write_json(chain.join("meta.json"), &chain_meta);
            crate::refs::pin_slots(
                &chain,
                &chain_meta,
                &[("applied", &after), ("sync-base", &after)],
            );
            let parent = s(&chain_meta, "parent").to_string();
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
    let final_code = if !conflicts.is_empty() {
        conclusion["apply"]["failedStep"] = json!("skipped-conflicts");
        conclusion["apply"]["failureDetail"] = json!(format!(
            "{} binary/symlink/large file(s) skipped: {}",
            conflicts.len(),
            conflicts.join(", ")
        ));
        conclusion["apply"]["skippedFiles"] = json!(conflicts);
        conclusion["apply"]["writtenFiles"] = json!(&written);
        EXIT_CONFLICT
    } else if marked {
        conclusion["apply"]["failedStep"] = json!("conflict-markers");
        conclusion["apply"]["failureDetail"] = json!(format!(
            "{} file(s) contain conflict markers: {}",
            marked_paths.len(),
            marked_paths.join(", ")
        ));
        conclusion["apply"]["writtenFiles"] = json!(&written);
        EXIT_CONFLICT
    } else {
        0
    };
    Ok(ApplyInnerResult {
        code: final_code,
        written,
        regenerated,
        had_dirty_merge,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_config_rejects_node_modules_links() {
        let cfg = json!({"worktree": {"link": ["apps/web/node_modules"]}});
        let err = work_config(&cfg, Path::new(".delegate.json")).unwrap_err();
        assert!(err.contains("node_modules"), "{err}");
        let root_nm = json!({"worktree": {"link": ["node_modules/"]}});
        assert!(work_config(&root_nm, Path::new(".delegate.json")).is_err());
        let ok = json!({"worktree": {"link": ["third_party/pi"]}});
        assert!(work_config(&ok, Path::new(".delegate.json")).is_ok());
    }

    #[test]
    fn markdown_fenced_examples_and_setext_headings_do_not_false_positive() {
        let dir = std::env::temp_dir().join(format!("delegate-md-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let doc = dir.join("guide.md");
        fs::write(
            &doc,
            "Title\n=======\n\n```text\n>>>>>>> example-label\n=======\n```\n",
        )
        .unwrap();
        assert!(conflict_labels(&doc).is_empty());
        assert!(!has_conflict_markers(&doc));

        // A real full conflict block inside a Markdown code fence IS still detected.
        let real = dir.join("real.md");
        fs::write(
            &real,
            "```text\n<<<<<<< current\na\n=======\nb\n>>>>>>> run-1\n```\n",
        )
        .unwrap();
        assert_eq!(conflict_labels(&real), vec!["current", "run-1"]);
        assert!(has_conflict_markers(&real));

        // Non-Markdown files still catch half-removed markers.
        let plain = dir.join("a.txt");
        fs::write(&plain, ">>>>>>> old-run\n").unwrap();
        assert_eq!(conflict_labels(&plain), vec!["old-run"]);
        assert!(has_conflict_markers(&plain));
        let _ = fs::remove_dir_all(&dir);
    }
}
