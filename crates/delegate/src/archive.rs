//! Evidence preservation is deliberately separate from build-cache retention.
use crate::common::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

fn build_path(path: &str) -> bool {
    path.split('/').any(|part| {
        matches!(
            part,
            "target"
                | "node_modules"
                | ".git"
                | "__pycache__"
                | ".cache"
                | "cache"
                | "caches"
                | ".pytest_cache"
                | ".mypy_cache"
                | ".ruff_cache"
                | ".next"
                | "dist"
                | "build"
                | ".cargo"
                | ".gradle"
                | ".npm"
                | ".pnpm-store"
                | ".venv"
                | ".turbo"
                | ".parcel-cache"
        )
    })
}
fn safe_file(root: &Path, name: &str) -> bool {
    let mut path = root.to_path_buf();
    let parts = Path::new(name).components().collect::<Vec<_>>();
    for (i, part) in parts.iter().enumerate() {
        if !matches!(part, Component::Normal(_)) {
            return false;
        }
        path.push(part);
        if i + 1 == parts.len() {
            // Preserve internal file links as content, never dereference managed
            // directory links or a file pointing back to the source checkout.
            if path.is_symlink() {
                return path
                    .canonicalize()
                    .ok()
                    .zip(root.canonicalize().ok())
                    .is_some_and(|(file, root)| file.starts_with(root) && file.is_file());
            }
            return path.is_file();
        }
        if path.is_symlink() {
            return false;
        }
    }
    false
}
fn path_reference(result: &str, name: &str) -> bool {
    result.match_indices(name).any(|(offset, _)| {
        result[offset + name.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && !"/_-.+".contains(c))
    })
}
fn mentioned(result: &str, name: &str) -> bool {
    path_reference(result, name)
        || Path::new(name)
            .ancestors()
            .skip(1)
            .filter_map(Path::to_str)
            .filter(|p| !p.is_empty())
            .any(|p| path_reference(result, p) || path_reference(result, &format!("{p}/")))
}
fn names(bytes: Vec<u8>) -> BTreeSet<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

/// A failed archive aborts explicit clean: never delete evidence after a copy error.
pub fn preserve(run: &Path) -> Res<Option<PathBuf>> {
    if is_record(run) {
        return Ok(status(run).map(|m| PathBuf::from(s(&m, "path"))));
    }
    let meta = json(run.join("meta.json"));
    let top = Path::new(s(&meta["worktree"], "path"));
    if !meta["worktree"].is_object() || !top.is_dir() {
        return Ok(None);
    }
    let ignored = names(git(
        top,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
    )?);
    let mut all = ignored.clone();
    all.extend(names(git(
        top,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )?));
    let result = format!(
        "{}\n{}",
        read(run.join("result.md")),
        read(run.join("result-original.md"))
    );
    let mut candidates = vec![];
    for name in all {
        let priority = mentioned(&result, &name);
        if (priority || (ignored.contains(&name) && !build_path(&name))) && safe_file(top, &name) {
            let size = fs::metadata(top.join(&name))
                .map_err(|e| e.to_string())?
                .len();
            candidates.push((!priority, size, name));
        }
    }
    if candidates.is_empty() {
        return Ok(None);
    }
    // Retain referenced evidence first; within each class drop largest files first.
    candidates.sort();
    let cap = setting("ARCHIVE_MAX_BYTES", "209715200")
        .parse::<u64>()
        .unwrap_or(209715200);
    let archive = cache_dir()
        .join("archive")
        .join(run.file_name().ok_or("run has no name")?);
    let mut kept = vec![];
    let mut skipped = vec![];
    let mut bytes: u64 = 0;
    for (_, size, name) in candidates {
        if size > cap.saturating_sub(bytes) {
            skipped.push(json!({"path":name,"bytes":size,"reason":"per-run size cap"}));
            continue;
        }
        let dest = archive.join("files").join(&name);
        fs::create_dir_all(dest.parent().ok_or("archive path has no parent")?)
            .map_err(|e| e.to_string())?;
        fs::copy(top.join(&name), dest).map_err(|e| format!("archive {name}: {e}"))?;
        bytes += size;
        kept.push(name);
    }
    if !skipped.is_empty() {
        eprintln!(
            "delegate: archive size cap: skipped {} file(s); see {}/manifest.json",
            skipped.len(),
            archive.display()
        );
    }
    let manifest =
        json!({"path":archive,"bytes":bytes,"files":kept,"skipped":skipped,"createdAt":iso()});
    write_json(archive.join("manifest.json"), &manifest)?;
    write_json(run.join("archive.json"), &manifest)?;
    // Keep enough history to route status/result after explicit clean removes the run.
    let record = archive.join("record");
    fs::create_dir_all(&record).map_err(|e| e.to_string())?;
    for name in [
        "meta.json",
        "summary.json",
        "result.md",
        "result-original.md",
        "exit_code",
        "archive.json",
        ".delivered",
        ".applied",
        "compression.json",
    ] {
        if run.join(name).is_file() {
            fs::copy(run.join(name), record.join(name)).map_err(|e| e.to_string())?;
        }
    }
    touch(&archive)?;
    prune(&archive);
    Ok(Some(archive))
}

pub fn touch(archive: &Path) -> Res<()> {
    write(archive.join(".access"), epoch().to_string())
}
fn prune(keep: &Path) {
    let root = cache_dir().join("archive");
    let days = setting("ARCHIVE_KEEP_DAYS", "30")
        .parse::<f64>()
        .unwrap_or(30.0);
    let limit = setting("ARCHIVE_TOTAL_BYTES", "2147483648")
        .parse::<u64>()
        .unwrap_or(2147483648);
    let mut entries = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| {
            let path = e.path();
            let accessed = read(path.join(".access")).parse::<f64>().unwrap_or(0.0);
            let bytes = json(path.join("manifest.json"))["bytes"]
                .as_u64()
                .unwrap_or(0);
            (accessed, path, bytes)
        })
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut total = entries.iter().map(|e| e.2).sum::<u64>();
    for (accessed, path, bytes) in entries {
        if path != keep
            && ((days > 0.0 && epoch() - accessed > days * 86400.0) || total > limit)
            && fs::remove_dir_all(path).is_ok()
        {
            total = total.saturating_sub(bytes);
        }
    }
}

pub fn is_record(run: &Path) -> bool {
    let manifest = json(run.join("archive.json"));
    !s(&manifest, "path").is_empty() && run == Path::new(s(&manifest, "path")).join("record")
}
pub fn status(run: &Path) -> Option<Value> {
    let manifest = json(run.join("archive.json"));
    let path = Path::new(s(&manifest, "path"));
    if path.is_dir() {
        let _ = touch(path);
        Some(manifest)
    } else {
        None
    }
}
