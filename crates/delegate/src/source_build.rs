use crate::common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

fn relative_path(path: &str) -> Res<PathBuf> {
    if path.is_empty() || path.contains('\0') {
        return Err("must be a non-empty relative path without NUL".into());
    }
    let mut relative = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir if relative.pop() => {}
            _ => {
                return Err(
                    "must stay inside the source repository (no absolute or escaping paths)".into(),
                )
            }
        }
    }
    Ok(relative)
}

// Check existing ancestors too: a missing build directory beneath an external
// symlink must not bypass validation. Recheck at measurement time after the run.
fn inside(root: &Path, relative: &Path) -> bool {
    let Ok(root) = fs::canonicalize(root) else {
        return false;
    };
    let mut ancestor = root.join(relative);
    loop {
        match fs::symlink_metadata(&ancestor) {
            Ok(_) => return fs::canonicalize(ancestor).is_ok_and(|p| p.starts_with(&root)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !ancestor.pop() {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
}

pub fn validate(value: &Value, file: &Path, root: Option<&Path>) -> Res<()> {
    let Some(config) = value.get("sourceBuild") else {
        return Ok(());
    };
    let check = || -> Res<()> {
        let config = config.as_object().ok_or("sourceBuild must be an object")?;
        if let Some(paths) = config.get("paths") {
            let paths = paths
                .as_array()
                .ok_or("sourceBuild.paths must be an array of relative paths")?;
            for (index, path) in paths.iter().enumerate() {
                let field = format!("sourceBuild.paths[{index}]");
                let path = path
                    .as_str()
                    .ok_or_else(|| format!("{field} must be a string"))?;
                let relative = relative_path(path).map_err(|e| format!("{field}: {e}"))?;
                if root.is_some_and(|root| !inside(root, &relative)) {
                    return Err(format!("{field} must stay inside the source repository"));
                }
            }
        }
        Ok(())
    };
    check().map_err(|e| format!("{}: {e}", file.display()))
}

pub fn record(run: &Path) {
    let mut meta = json(run.join("meta.json"));
    if s(&meta, "mode") != "write" || meta["sourceBuildBytes"].as_u64().is_some() {
        return;
    }
    let root = Path::new(s(&meta, "sourceBuildRoot"));
    if !root.is_absolute() || !root.is_dir() {
        return;
    }
    let Ok(canonical_root) = fs::canonicalize(root) else {
        return;
    };
    let defaults = if root.join("Cargo.toml").is_file() {
        json!(["target"])
    } else {
        json!([])
    };
    let Some(paths) = meta["sourceBuild"]
        .get("paths")
        .unwrap_or(&defaults)
        .as_array()
    else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut sizes = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut total = 0u64;
    for path in paths {
        let Some(path) = path.as_str() else { return };
        let Ok(relative) = relative_path(path) else {
            return;
        };
        if !inside(root, &relative) {
            return;
        }
        let full = root.join(&relative);
        if !full.is_dir() {
            continue;
        }
        let Ok(canonical) = fs::canonicalize(&full) else {
            return;
        };
        if !canonical.starts_with(&canonical_root) {
            return;
        }
        if !seen.insert(canonical.clone()) {
            continue;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return;
        };
        let mut command = std::process::Command::new("du");
        command.args(["-sb", "--"]).arg(&canonical);
        let Ok(output) = crate::cleanup::bounded_output(command, remaining) else {
            return;
        };
        let Some(bytes) = output
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<u64>().ok())
        else {
            return;
        };
        total = total.saturating_add(bytes);
        sizes.push(json!({"path": if relative.as_os_str().is_empty() { ".".into() } else { relative.to_string_lossy().into_owned() }, "bytes":bytes}));
    }
    meta["sourceBuildBytes"] = json!(total);
    meta["sourceBuildPaths"] = json!(sizes);
    meta["sourceBuildMeasuredAt"] = json!(iso());
    meta["sourceBuildMeasuredNs"] = json!(now_ns().to_string());
    let _ = write_json(run.join("meta.json"), &meta);
}
