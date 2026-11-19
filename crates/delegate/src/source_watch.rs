//! Source workspace observations, independent of task worktrees. Apply records exact
//! post-write fingerprints under the same lock used by observers: a later edit to
//! an applied path is not silently exempted.
use crate::common::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn link_points_into(link: &Path, target_root: &Path, target_canon: &Path) -> bool {
    let Ok(raw) = fs::read_link(link) else {
        return false;
    };
    let joined = if raw.is_absolute() {
        raw.clone()
    } else {
        link.parent().unwrap_or(Path::new("/")).join(&raw)
    };
    let norm = normalize_path(&joined);
    if norm.starts_with(target_root) || norm.starts_with(target_canon) {
        return true;
    }
    if !raw.is_absolute() {
        if let Some(parent) = link.parent().and_then(|p| p.canonicalize().ok()) {
            let norm_canon = normalize_path(&parent.join(&raw));
            if norm_canon.starts_with(target_root) || norm_canon.starts_with(target_canon) {
                return true;
            }
        }
    }
    joined
        .canonicalize()
        .is_ok_and(|c| c.starts_with(target_root) || c.starts_with(target_canon))
}

fn add_package_dir(dirs: &mut BTreeSet<String>, path: &str) {
    if path == ".local" || path.starts_with(".local/") {
        return;
    }
    if path == "package.json" {
        dirs.insert(String::new());
    } else if let Some(parent) = path.strip_suffix("/package.json") {
        dirs.insert(parent.to_string());
    }
}

fn node_modules_dirs(root: &Path, status_data: Option<&[u8]>) -> Vec<String> {
    let mut dirs = BTreeSet::new();
    dirs.insert(String::new());
    let args: &[&str] = if status_data.is_some() {
        &["ls-files", "-z", "--", "package.json", "*/package.json"]
    } else {
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "package.json",
            "*/package.json",
        ]
    };
    if let Ok(list) = git(root, args) {
        for item in list.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            add_package_dir(&mut dirs, &String::from_utf8_lossy(item));
        }
    }
    if let Some(data) = status_data {
        for row in data.split(|b| *b == 0).filter(|r| r.len() > 3) {
            add_package_dir(&mut dirs, &String::from_utf8_lossy(&row[3..]));
        }
    }
    dirs.into_iter()
        .map(|d| {
            if d.is_empty() {
                "node_modules".to_string()
            } else {
                format!("{d}/node_modules")
            }
        })
        .filter(|rel| fs::symlink_metadata(root.join(rel)).is_ok())
        .collect()
}

fn normalized_workspace_state(bytes: &[u8]) -> Vec<u8> {
    if let Ok(mut val) = serde_json::from_slice::<Value>(bytes) {
        if let Some(obj) = val.as_object_mut() {
            // pnpm refreshes lastValidatedTimestamp on read-only checks.
            obj.remove("lastValidatedTimestamp");
            if let Ok(clean) = serde_json::to_vec(&val) {
                return clean;
            }
        }
    }
    bytes.to_vec()
}

fn fingerprint_node_modules(nm: &Path) -> Option<String> {
    let meta = fs::symlink_metadata(nm).ok()?;
    let mut digest = sha1_smol::Sha1::new();
    if meta.file_type().is_symlink() {
        digest.update(b"symlink\0");
        if let Ok(target) = fs::read_link(nm) {
            digest.update(target.as_os_str().as_encoded_bytes());
        }
        return Some(digest.digest().to_string());
    }
    if !meta.is_dir() {
        return None;
    }
    let mut entries = BTreeMap::<String, Vec<u8>>::new();
    for entry in fs::read_dir(nm).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".pnpm" || name == ".cache" || name.starts_with(".vite") {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_symlink() {
            let target = fs::read_link(&path)
                .map(|p| p.as_os_str().as_encoded_bytes().to_vec())
                .unwrap_or_default();
            entries.insert(name, target);
        } else if ft.is_dir() && (name.starts_with('@') || name == ".bin") {
            for sub in fs::read_dir(&path).into_iter().flatten().flatten() {
                if !sub.file_type().is_ok_and(|t| t.is_symlink()) {
                    continue;
                }
                let sub_name = sub.file_name().to_string_lossy().into_owned();
                let target = fs::read_link(sub.path())
                    .map(|p| p.as_os_str().as_encoded_bytes().to_vec())
                    .unwrap_or_default();
                entries.insert(format!("{name}/{sub_name}"), target);
            }
        } else if name == ".modules.yaml" {
            entries.insert(name, fs::read(&path).unwrap_or_default());
        } else if name == ".pnpm-workspace-state-v1.json" {
            entries.insert(
                name,
                normalized_workspace_state(&fs::read(&path).unwrap_or_default()),
            );
        }
    }
    for (k, v) in entries {
        digest.update(k.as_bytes());
        digest.update(b"\0");
        digest.update(&v);
        digest.update(b"\0");
    }
    Some(digest.digest().to_string())
}

fn node_modules_leaks_into(nm: &Path, target_root: &Path, target_canon: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(nm) else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return link_points_into(nm, target_root, target_canon);
    }
    if !meta.is_dir() {
        return false;
    }
    let root_str = target_root.to_string_lossy();
    let canon_str = target_canon.to_string_lossy();
    for entry in fs::read_dir(nm).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".pnpm" || name == ".cache" || name.starts_with(".vite") {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_symlink() {
            if link_points_into(&path, target_root, target_canon) {
                return true;
            }
        } else if ft.is_dir() && (name.starts_with('@') || name == ".bin") {
            for sub in fs::read_dir(&path).into_iter().flatten().flatten() {
                if sub.file_type().is_ok_and(|t| t.is_symlink())
                    && link_points_into(&sub.path(), target_root, target_canon)
                {
                    return true;
                }
            }
        } else if name == ".modules.yaml" || name == ".pnpm-workspace-state-v1.json" {
            if let Ok(text) = fs::read_to_string(&path) {
                if (!root_str.is_empty() && text.contains(root_str.as_ref()))
                    || (!canon_str.is_empty() && text.contains(canon_str.as_ref()))
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Source `node_modules` directories whose symlinks or pnpm metadata point into this task's worktree.
pub fn worktree_leaks(meta: &Value) -> Vec<String> {
    let source = Path::new(s(&meta["worktree"], "source"));
    let worktree = Path::new(s(&meta["worktree"], "path"));
    if !source.is_absolute() || !worktree.is_absolute() || source == worktree {
        return vec![];
    }
    let wt_canon = worktree
        .canonicalize()
        .unwrap_or_else(|_| worktree.to_path_buf());
    node_modules_dirs(source, None)
        .into_iter()
        .filter(|rel| node_modules_leaks_into(&source.join(rel), worktree, &wt_canon))
        .collect()
}

/// Worktree `node_modules` entries that are symlinks pointing into the source checkout.
pub fn worktree_source_node_modules_links(meta: &Value) -> Vec<String> {
    let source = Path::new(s(&meta["worktree"], "source"));
    let worktree = Path::new(s(&meta["worktree"], "path"));
    if !source.is_absolute() || !worktree.is_absolute() || source == worktree || !worktree.is_dir()
    {
        return vec![];
    }
    let source_canon = source
        .canonicalize()
        .unwrap_or_else(|_| source.to_path_buf());
    node_modules_dirs(worktree, None)
        .into_iter()
        .filter(|rel| {
            let link = worktree.join(rel);
            link.is_symlink() && link_points_into(&link, source, &source_canon)
        })
        .collect()
}

fn journal(source: &Path) -> PathBuf {
    state_dir().join("source-watch").join(
        sha1_smol::Sha1::from(
            source
                .canonicalize()
                .unwrap_or_else(|_| source.to_path_buf())
                .as_os_str()
                .as_encoded_bytes(),
        )
        .digest()
        .to_string(),
    )
}
pub fn guard(source: &Path, exclusive: bool) -> Res<fs::File> {
    let dir = journal(source);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    lock(&dir.join("lock"), exclusive, false).map_err(|e| e.to_string())
}
pub fn fingerprint(source: &Path) -> Res<Value> {
    let data = git(
        source,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
    )?;
    let mut out = json!({});
    for row in data.split(|b| *b == 0).filter(|r| r.len() > 3) {
        let path = String::from_utf8_lossy(&row[3..]).into_owned();
        if path == ".local" || path.starts_with(".local/") { continue; }
        let target = source.join(&path);
        let content = if target.is_symlink() {
            fs::read_link(&target)
                .map(|p| p.as_os_str().as_encoded_bytes().to_vec())
                .unwrap_or_default()
        } else {
            fs::read(&target).unwrap_or_default()
        };
        let mode = fs::symlink_metadata(&target)
            .map(|m| m.permissions().mode())
            .unwrap_or(0);
        let index = git(
            source,
            &["--literal-pathspecs", "ls-files", "-s", "--", &path],
        )?;
        let mut digest = sha1_smol::Sha1::new();
        digest.update(row);
        digest.update(&content);
        digest.update(&index);
        digest.update(&mode.to_le_bytes());
        out[&path] = json!(digest.digest().to_string());
    }
    for rel in node_modules_dirs(source, Some(&data)) {
        if let Some(digest) = fingerprint_node_modules(&source.join(&rel)) {
            out[&rel] = json!(digest);
        }
    }
    Ok(out)
}
pub fn baseline(source: &Path) -> Res<Value> {
    let _guard = guard(source, false)?;
    Ok(json!({"files":fingerprint(source)?, "at":now_ns().to_string()}))
}
fn different(before: &Value, after: &Value) -> Vec<String> {
    before
        .as_object()
        .into_iter()
        .flatten()
        .map(|(p, _)| p)
        .chain(after.as_object().into_iter().flatten().map(|(p, _)| p))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|p| before[*p] != after[*p])
        .cloned()
        .collect()
}
/// Caller holds the exclusive source guard, including on partially failed apply.
pub fn register(source: &Path, before: &Value, paths: &[String]) -> Res<()> {
    let after = fingerprint(source)?;
    let file = journal(source).join("applied.json");
    let mut records = json(&file);
    if !records.is_object() {
        records = json!({});
    }
    for path in different(before, &after)
        .into_iter()
        .filter(|p| crate::worktree::matches_rule(p, paths))
    {
        records[&path] = json!({"fingerprint":after[&path],"at":now_ns().to_string()});
    }
    write_json(file, &records)
}
pub fn busy(meta: &Value) -> bool {
    let source = Path::new(s(&meta["worktree"], "source"));
    meta["sourceStatusBaseline"].is_object() && lock(&journal(source).join("lock"), false, true).is_err()
}
pub fn changed(meta: &Value) -> Vec<String> {
    changes_now(meta, false)
}
pub fn observe(meta: &Value, run: &Path) {
    if !meta["sourceStatusBaseline"].is_object() || busy(meta) { return; }
    let paths = changes_now(meta, true);
    let _ = write_json(run.join("source-observed.json"), &json!(paths));
}
fn changes_now(meta: &Value, nonblocking: bool) -> Vec<String> {
    let baseline = &meta["sourceStatusBaseline"];
    let source = Path::new(s(&meta["worktree"], "source"));
    if !baseline.is_object() || !source.is_absolute() {
        return vec![];
    }
    let dir = journal(source);
    let Ok(_guard) = lock(&dir.join("lock"), false, nonblocking) else {
        return vec![];
    };
    let Ok(now) = fingerprint(source) else {
        return vec![];
    };
    let applied = json(journal(source).join("applied.json"));
    let started = s(baseline, "at").parse::<u128>().unwrap_or(0);
    different(&baseline["files"], &now)
        .into_iter()
        .filter(|path| {
            let record = &applied[path];
            !(s(record, "at").parse::<u128>().unwrap_or(0) >= started
                && record["fingerprint"] == now[path])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn git_ok(dir: &Path, args: &[&str]) {
        let out = git_command()
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn fingerprint_tracks_node_modules_symlinks_and_ignores_vite_cache() {
        let tmp = std::env::temp_dir().join(format!("delegate-sw-{}", now_ns()));
        let src = tmp.join("source");
        let wt = tmp.join("worktree");
        fs::create_dir_all(src.join("apps/web/node_modules/@demo")).unwrap();
        fs::create_dir_all(src.join("node_modules/.pnpm/vite@1/node_modules/vite")).unwrap();
        fs::write(src.join(".gitignore"), "node_modules\n").unwrap();
        fs::write(src.join("package.json"), "{}").unwrap();
        fs::write(src.join("apps/web/package.json"), "{}").unwrap();
        git_ok(&src, &["init", "-q"]);
        git_ok(&src, &["add", "."]);
        git_ok(&src, &["commit", "-q", "-m", "init"]);

        symlink(
            "../../../node_modules/.pnpm/vite@1/node_modules/vite",
            src.join("apps/web/node_modules/vite"),
        )
        .unwrap();
        fs::write(
            src.join("node_modules/.pnpm-workspace-state-v1.json"),
            r#"{"lastValidatedTimestamp":1,"projects":{"/src":{"name":"root"}}}"#,
        )
        .unwrap();

        let before = fingerprint(&src).unwrap();
        assert!(before["apps/web/node_modules"].is_string());

        // Volatile Vite cache and pnpm lastValidatedTimestamp must not trigger false positives.
        fs::create_dir_all(src.join("apps/web/node_modules/.vite")).unwrap();
        fs::write(src.join("apps/web/node_modules/.vite/deps.json"), "{}").unwrap();
        fs::write(
            src.join("node_modules/.pnpm-workspace-state-v1.json"),
            r#"{"lastValidatedTimestamp":999,"projects":{"/src":{"name":"root"}}}"#,
        )
        .unwrap();
        let after_noise = fingerprint(&src).unwrap();
        assert!(different(&before, &after_noise).is_empty());

        // Rewriting a symlink in source/apps/web/node_modules to point into the worktree is caught,
        // even when the worktree target does not exist (dangling symlink).
        fs::remove_file(src.join("apps/web/node_modules/vite")).unwrap();
        symlink(
            "../../../../worktree/node_modules/.pnpm/vite@1/node_modules/vite",
            src.join("apps/web/node_modules/vite"),
        )
        .unwrap();
        let after_rewrite = fingerprint(&src).unwrap();
        assert_eq!(different(&before, &after_rewrite), vec!["apps/web/node_modules"]);

        let meta = json!({"worktree": {"source": src, "path": wt}});
        assert_eq!(worktree_leaks(&meta), vec!["apps/web/node_modules"]);
        let _ = fs::remove_dir_all(&tmp);
    }
}
