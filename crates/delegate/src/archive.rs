//! Evidence and run-record archival is separate from build-cache retention and history index.
//! Archives have no total-size hard cap: usage is surfaced in `status` and `board`.
//! Only when the archive filesystem approaches exhaustion (critical free-space watermark)
//! are least-recently-read reconstructible/unneeded trace bytes (`events.jsonl.gz`, then `files/`)
//! trimmed; `prompt.md`, `result.md`, `meta.json`, `summary.json`, patches, and `history.jsonl`
//! are never deleted.
use crate::common::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

/// Default total-archive size anomaly warning threshold for `status` and `board` (30 GiB).
/// Configurable via `DELEGATE_ARCHIVE_WARN_GIB` (`0` disables) or `DELEGATE_ARCHIVE_WARN_BYTES`.
/// Archives have no hard cap: files are never skipped by size during preservation.
pub const DEFAULT_ARCHIVE_WARN_GIB: f64 = 30.0;
/// Filesystem critical-space watermark (aligned with disk-pressure warning policy):
/// free space below 10 GiB or below 5% of total filesystem capacity.
pub const ARCHIVE_FS_CRITICAL_FREE_BYTES: u64 = 10 * 1024 * 1024 * 1024;
pub const ARCHIVE_FS_CRITICAL_FREE_PCT: u64 = 5;

#[derive(Clone, Debug, Default)]
pub struct ArchiveUsage {
    pub bytes: u64,
    pub count: usize,
    pub fs_avail_bytes: Option<u64>,
    pub fs_critical: bool,
    pub warn_bytes: u64,
    pub size_warning: bool,
}

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
                | "venv"
                | ".tox"
                | ".nox"
                | ".turbo"
                | ".parcel-cache"
        ) || part.ends_with(".egg-info")
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
    if build_path(name) {
        // For files inside reconstructible/build directories (e.g. `.venv/`, `target/`,
        // `node_modules/`, `dist/`, `__pycache__/`, `.pytest_cache/`), only archive a
        // file when `result.md` names that specific file — never sweep in the whole
        // directory when an ancestor or build directory itself is mentioned.
        return path_reference(result, name);
    }
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

fn dir_bytes(root: &Path) -> u64 {
    dir_bytes_excluding(root, None)
}

fn dir_bytes_excluding(root: &Path, skip_top_name: Option<&str>) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    for entry in entries.flatten() {
        if let Some(skip) = skip_top_name {
            if entry.file_name().to_str() == Some(skip) {
                continue;
            }
        }
        let path = entry.path();
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.is_dir() {
                total = total.saturating_add(dir_bytes_excluding(&path, None));
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

fn collect_files(root: &Path, prefix: &str, out: &mut Vec<String>, bytes: &mut u64) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.is_dir() {
                collect_files(&path, &rel, out, bytes);
            } else if meta.is_file() {
                *bytes = bytes.saturating_add(meta.len());
                out.push(rel);
            }
        }
    }
}

fn critical_watermarks() -> (u64, u64) {
    let free_bytes = if let Ok(v) = setting("FS_CRITICAL_FREE_BYTES", "").parse::<u64>() {
        v
    } else if let Ok(gib) = setting("FS_CRITICAL_FREE_GIB", "").parse::<u64>() {
        gib.saturating_mul(1024 * 1024 * 1024)
    } else {
        ARCHIVE_FS_CRITICAL_FREE_BYTES
    };
    let free_pct = setting("FS_CRITICAL_FREE_PCT", &ARCHIVE_FS_CRITICAL_FREE_PCT.to_string())
        .parse::<u64>()
        .unwrap_or(ARCHIVE_FS_CRITICAL_FREE_PCT);
    (free_bytes, free_pct)
}

fn statvfs_space(path: &Path) -> Option<(u64, u64)> {
    let mut probe = path;
    while !probe.exists() {
        probe = probe.parent()?;
    }
    let cpath = CString::new(probe.as_os_str().as_bytes()).ok()?;
    let mut st = unsafe { std::mem::zeroed::<libc::statvfs>() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = if st.f_frsize > 0 {
        st.f_frsize as u64
    } else {
        st.f_bsize as u64
    };
    let avail = (st.f_bavail as u64).saturating_mul(frsize);
    let total = (st.f_blocks as u64).saturating_mul(frsize);
    Some((avail, total))
}

fn fs_space(path: &Path) -> Option<(u64, u64)> {
    let real = statvfs_space(path);
    if let Ok(simulated) = setting("ARCHIVE_FS_AVAIL_BYTES", "").parse::<u64>() {
        let total = real
            .map(|(_, t)| t.max(simulated))
            .unwrap_or(simulated.saturating_mul(20).max(1));
        return Some((simulated, total));
    }
    real
}

fn is_fs_critical(avail: u64, total: u64, min_free_bytes: u64, min_free_pct: u64) -> bool {
    (min_free_bytes > 0 && avail < min_free_bytes)
        || (min_free_pct > 0 && total > 0 && avail.saturating_mul(100) < total.saturating_mul(min_free_pct))
}

pub fn usage_cache_path() -> PathBuf {
    cache_dir().join("archive").join(".usage.json")
}

fn scan_usage_counts(root: &Path) -> (u64, usize) {
    let mut bytes = 0u64;
    let mut count = 0usize;
    for entry in fs::read_dir(root).into_iter().flatten().flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        count += 1;
        let manifest = json(path.join("manifest.json"));
        let size = manifest["totalBytes"]
            .as_u64()
            .or_else(|| manifest["bytes"].as_u64())
            .unwrap_or_else(|| dir_bytes(&path));
        bytes = bytes.saturating_add(size);
    }
    (bytes, count)
}

fn write_usage_cache(root: &Path, bytes: u64, count: usize) {
    if !root.is_dir() {
        return;
    }
    let _ = write_json(
        root.join(".usage.json"),
        &json!({
            "bytes": bytes,
            "count": count,
            "updatedAt": iso(),
        }),
    );
}

/// Rescan `<cache>/delegate/archive` and atomically refresh `.usage.json`.
/// Only called from write paths (`preserve`, `trim`, `_gc-worker`).
pub fn refresh_usage_cache() -> (u64, usize) {
    let root = cache_dir().join("archive");
    let (bytes, count) = scan_usage_counts(&root);
    write_usage_cache(&root, bytes, count);
    (bytes, count)
}

fn archive_warn_bytes() -> u64 {
    if let Ok(v) = setting("ARCHIVE_WARN_BYTES", "").parse::<u64>() {
        return v;
    }
    let gib = setting("ARCHIVE_WARN_GIB", &format!("{DEFAULT_ARCHIVE_WARN_GIB}"))
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .unwrap_or(DEFAULT_ARCHIVE_WARN_GIB);
    if gib <= 0.0 {
        0
    } else {
        (gib * (1024.0 * 1024.0 * 1024.0)) as u64
    }
}

/// Strictly read-only archive usage check: reads atomic `.usage.json` cache when present,
/// falling back to an in-memory scan without writing if the cache is missing or corrupt.
pub fn usage() -> ArchiveUsage {
    let root = cache_dir().join("archive");
    let cached = json(root.join(".usage.json"));
    let (bytes, count) = match (cached["bytes"].as_u64(), cached["count"].as_u64()) {
        (Some(b), Some(c)) => (b, c as usize),
        _ => scan_usage_counts(&root),
    };
    let (min_bytes, min_pct) = critical_watermarks();
    let (fs_avail_bytes, fs_critical) = match fs_space(&root) {
        Some((avail, total)) => (
            Some(avail),
            is_fs_critical(avail, total, min_bytes, min_pct),
        ),
        None => (None, false),
    };
    let warn_bytes = archive_warn_bytes();
    let size_warning = warn_bytes > 0 && bytes >= warn_bytes;
    ArchiveUsage {
        bytes,
        count,
        fs_avail_bytes,
        fs_critical,
        warn_bytes,
        size_warning,
    }
}

fn archive_events(src: &Path, record: &Path) -> Res<()> {
    if !src.is_file() {
        return Ok(());
    }
    let gz = record.join("events.jsonl.gz");
    let tmp = atomic_tmp_path(&gz);
    let compressed = (|| -> std::io::Result<bool> {
        let input = File::open(src)?;
        let output = File::create(&tmp)?;
        let status = Command::new("gzip")
            .arg("-c")
            .stdin(Stdio::from(input))
            .stdout(Stdio::from(output))
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            if let Ok(f) = File::open(&tmp) {
                let _ = f.sync_data();
            }
        }
        Ok(status.success())
    })();
    match compressed {
        Ok(true) => {
            fs::rename(&tmp, &gz).map_err(|e| e.to_string())?;
            let _ = fs::remove_file(record.join("events.jsonl"));
            Ok(())
        }
        _ => {
            let _ = fs::remove_file(&tmp);
            fs::copy(src, record.join("events.jsonl")).map_err(|e| e.to_string())?;
            Ok(())
        }
    }
}

/// A failed archive aborts explicit clean: never delete evidence after a copy error.
pub fn preserve(run: &Path) -> Res<Option<PathBuf>> {
    if is_record(run) {
        return Ok(status(run).map(|m| PathBuf::from(s(&m, "path"))));
    }
    let meta = json(run.join("meta.json"));
    let run_name = run.file_name().ok_or("run has no name")?;
    let archive = cache_dir().join("archive").join(run_name);
    let existing = json(archive.join("manifest.json"));
    let existed = existing.is_object();
    let prev_total_bytes = existing["totalBytes"]
        .as_u64()
        .or_else(|| existing["bytes"].as_u64())
        .unwrap_or(0);
    let mut kept = strings(&existing["files"]);
    let mut skipped = existing["skipped"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut bytes: u64 = existing["bytes"].as_u64().unwrap_or(0);
    let created_at = if !s(&existing, "createdAt").is_empty() {
        s(&existing, "createdAt").to_string()
    } else {
        iso()
    };

    let top = Path::new(s(&meta["worktree"], "path"));
    if meta["worktree"].is_object() && top.is_dir() {
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
        let mut result = format!(
            "{}\n{}",
            read(run.join("result.md")),
            read(run.join("result-original.md"))
        );
        for (_, rpath) in crate::runs::intra_run_result_files(run) {
            if rpath != run.join("result.md") {
                result.push('\n');
                result.push_str(&read(&rpath));
            }
        }
        let mut candidates = vec![];
        for name in all {
            let priority = mentioned(&result, &name);
            if (priority || (ignored.contains(&name) && !build_path(&name))) && safe_file(top, &name)
            {
                let size = fs::metadata(top.join(&name))
                    .map_err(|e| e.to_string())?
                    .len();
                candidates.push((!priority, size, name));
            }
        }
        if !candidates.is_empty() {
            // Keep all candidate evidence files without any per-run size hard cap;
            // referenced evidence is ordered first and disk pressure is handled solely by
            // `prune_if_fs_critical` (which trims only reconstructible bytes).
            candidates.sort();
            kept.clear();
            skipped.clear();
            bytes = 0;
            for (_, size, name) in candidates {
                let dest = archive.join("files").join(&name);
                fs::create_dir_all(dest.parent().ok_or("archive path has no parent")?)
                    .map_err(|e| e.to_string())?;
                fs::copy(top.join(&name), dest).map_err(|e| format!("archive {name}: {e}"))?;
                bytes = bytes.saturating_add(size);
                kept.push(name);
            }
        }
    }

    // Keep prompt, full answer, meta, patches, and compressed events so show/result/patch
    // work after clean while discarding worktree, shims, sessions, and agent input streams.
    let record = archive.join("record");
    fs::create_dir_all(&record).map_err(|e| e.to_string())?;
    for name in [
        "meta.json",
        "summary.json",
        "appends.json",
        "prompt.md",
        "result.md",
        "result-original.md",
        "changes.json",
        "changes.patch",
        "applied.patch",
        "applied-files.txt",
        "chain-files.txt",
        "review.json",
        "review.md",
        "usage-rounds.jsonl",
        "controller-review.diff",
        "sign.json",
        "exit_code",
        ".delivered",
        ".applied",
        ".applied-cleaned",
        "compression.json",
    ] {
        if run.join(name).is_file() {
            fs::copy(run.join(name), record.join(name)).map_err(|e| e.to_string())?;
        }
    }
    if !record.join("applied-files.txt").is_file() && !record.join("chain-files.txt").is_file() {
        let chain_files = crate::worktree::run_files(run);
        if !chain_files.is_empty() || run.join("changes.json").is_file() {
            let content = if chain_files.is_empty() {
                String::new()
            } else {
                format!("{}\n", chain_files.join("\n"))
            };
            let _ = write(record.join("chain-files.txt"), content);
        }
    }
    if let Ok(entries) = fs::read_dir(run) {
        for entry in entries.flatten() {
            let fname = entry.file_name();
            if crate::runs::is_round_result_name(&fname.to_string_lossy()) && entry.path().is_file()
            {
                fs::copy(entry.path(), record.join(&fname)).map_err(|e| e.to_string())?;
            }
        }
    }
    let rework_rounds: std::collections::HashSet<String> = {
        let rev = json(run.join("review.json"));
        rev["rounds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| s(r, "verdict").eq_ignore_ascii_case("REWORK"))
            .filter_map(|r| r["round"].as_u64().map(|n| format!("round-{n}")))
            .collect()
    };
    if let Ok(entries) = fs::read_dir(run.join("review")) {
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            let ufile = p.join("usage.json");
            let total_diff = p.join("total.diff");
            let copy_total_diff = rework_rounds.contains(&dir_name) && total_diff.is_file();
            if ufile.is_file() || copy_total_diff {
                let dst = record.join("review").join(&dir_name);
                fs::create_dir_all(&dst).map_err(|e| e.to_string())?;
                if ufile.is_file() {
                    fs::copy(&ufile, dst.join("usage.json")).map_err(|e| e.to_string())?;
                }
                if copy_total_diff {
                    fs::copy(&total_diff, dst.join("total.diff")).map_err(|e| e.to_string())?;
                }
            }
        }
    }
    archive_events(&run.join("events.jsonl"), &record)?;

    let record_bytes = dir_bytes(&record);
    let total_bytes = bytes.saturating_add(record_bytes);
    let manifest = json!({
        "path": archive,
        "bytes": bytes,
        "recordBytes": record_bytes,
        "totalBytes": total_bytes,
        "files": kept,
        "skipped": skipped,
        "createdAt": created_at
    });
    write_json(archive.join("manifest.json"), &manifest)?;
    write_json(record.join("archive.json"), &manifest)?;
    if run.is_dir() {
        write_json(run.join("archive.json"), &manifest)?;
    }
    touch(&archive)?;
    let trimmed = prune_if_fs_critical(&archive);
    let archive_root = cache_dir().join("archive");
    let cached = json(archive_root.join(".usage.json"));
    if trimmed {
        refresh_usage_cache();
    } else if let (Some(old_b), Some(old_c)) = (cached["bytes"].as_u64(), cached["count"].as_u64()) {
        let new_b = old_b.saturating_sub(prev_total_bytes).saturating_add(total_bytes);
        let new_c = if existed { old_c as usize } else { old_c as usize + 1 };
        write_usage_cache(&archive_root, new_b, new_c);
    } else {
        refresh_usage_cache();
    }
    Ok(Some(archive))
}

pub fn touch(archive: &Path) -> Res<()> {
    write(archive.join(".access"), epoch().to_string())
}

fn update_manifest_sizes(archive: &Path, evicted_events: bool, evicted_files: bool) {
    let manifest_path = archive.join("manifest.json");
    let mut manifest = json(&manifest_path);
    if !manifest.is_object() {
        return;
    }
    let files_bytes = dir_bytes(&archive.join("files"));
    let record_bytes = dir_bytes_excluding(&archive.join("record"), Some("archive.json"));
    manifest["bytes"] = json!(files_bytes);
    manifest["recordBytes"] = json!(record_bytes);
    manifest["totalBytes"] = json!(files_bytes.saturating_add(record_bytes));
    if evicted_events {
        manifest["eventsEvicted"] = json!(true);
    }
    if evicted_files {
        manifest["files"] = json!([]);
        manifest["filesEvicted"] = json!(true);
    }
    let _ = write_json(&manifest_path, &manifest);
    let rec_arch = archive.join("record/archive.json");
    if rec_arch.is_file() {
        let _ = write_json(&rec_arch, &manifest);
    }
}

/// Only when the archive filesystem hits its critical free-space watermark do we trim
/// least-recently-read trace bytes (`events.jsonl.gz` / `events.jsonl`, then `files/`).
/// `prompt.md`, `result.md`, `meta.json`, `summary.json`, patches, and `history.jsonl` are never deleted.
fn prune_if_fs_critical(keep: &Path) -> bool {
    let root = cache_dir().join("archive");
    let Some((mut avail, total)) = fs_space(&root) else {
        return false;
    };
    let (min_free_bytes, min_free_pct) = critical_watermarks();
    if !is_fs_critical(avail, total, min_free_bytes, min_free_pct) {
        return false;
    }
    let mut entries: Vec<(f64, PathBuf)> = fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir() && e.path() != keep)
        .map(|e| {
            let path = e.path();
            let accessed = read(path.join(".access")).parse::<f64>().unwrap_or(0.0);
            (accessed, path)
        })
        .collect();
    entries.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut freed_total = 0u64;
    let mut trimmed_events = 0usize;
    let mut trimmed_files = 0usize;

    // Pass 1: drop least-recently-read events.jsonl.gz / events.jsonl first.
    for (_, path) in &entries {
        if !is_fs_critical(avail, total, min_free_bytes, min_free_pct) {
            break;
        }
        let mut freed_here = 0u64;
        for name in ["events.jsonl.gz", "events.jsonl"] {
            let f = path.join("record").join(name);
            if let Ok(meta) = fs::metadata(&f) {
                let sz = meta.len();
                if fs::remove_file(&f).is_ok() {
                    freed_here = freed_here.saturating_add(sz);
                }
            }
        }
        if freed_here > 0 {
            avail = avail.saturating_add(freed_here);
            freed_total = freed_total.saturating_add(freed_here);
            trimmed_events += 1;
            update_manifest_sizes(path, true, false);
        }
    }

    // Pass 2: only if filesystem is STILL critically full, drop copied evidence `files/`.
    for (_, path) in &entries {
        if !is_fs_critical(avail, total, min_free_bytes, min_free_pct) {
            break;
        }
        let files_dir = path.join("files");
        if files_dir.is_dir() {
            let sz = dir_bytes(&files_dir);
            if fs::remove_dir_all(&files_dir).is_ok() {
                avail = avail.saturating_add(sz);
                freed_total = freed_total.saturating_add(sz);
                trimmed_files += 1;
                update_manifest_sizes(path, false, true);
            }
        }
    }

    if freed_total > 0 {
        eprintln!(
            "delegate: archive filesystem critically low; trimmed {} ({trimmed_events} event log(s), {trimmed_files} evidence dir(s)); prompt/result/meta/summary/patch kept",
            format_bytes(freed_total)
        );
        return true;
    }
    false
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

#[derive(Clone, Debug, Default)]
pub struct ArchiveRescan {
    pub root: PathBuf,
    pub count: usize,
    pub bytes: u64,
    pub updated_manifests: usize,
    pub previous_bytes: Option<u64>,
    pub previous_count: Option<usize>,
}

fn rescan_one(archive: &Path) -> Res<(u64, bool)> {
    let files_dir = archive.join("files");
    let record_dir = archive.join("record");
    let mut actual_files = Vec::new();
    let mut files_bytes = 0u64;
    if files_dir.is_dir() {
        collect_files(&files_dir, "", &mut actual_files, &mut files_bytes);
        actual_files.sort();
    }
    let record_bytes = if record_dir.is_dir() {
        dir_bytes_excluding(&record_dir, Some("archive.json"))
    } else {
        0
    };
    let total_bytes = files_bytes.saturating_add(record_bytes);

    let manifest_path = archive.join("manifest.json");
    let mut manifest = json(&manifest_path);
    let had_manifest = manifest.is_object();
    let existing_files = strings(&manifest["files"]);
    let actual_set: BTreeSet<&str> = actual_files.iter().map(String::as_str).collect();
    let existing_set: BTreeSet<&str> = existing_files.iter().map(String::as_str).collect();
    let files_changed = actual_set != existing_set;

    let updated_files: Vec<String> = if files_changed {
        let mut next: Vec<String> = existing_files
            .into_iter()
            .filter(|f| actual_set.contains(f.as_str()))
            .collect();
        let next_set: BTreeSet<String> = next.iter().cloned().collect();
        for f in &actual_files {
            if !next_set.contains(f) {
                next.push(f.clone());
            }
        }
        next
    } else {
        existing_files
    };

    let bytes_changed = manifest["bytes"].as_u64() != Some(files_bytes)
        || manifest["recordBytes"].as_u64() != Some(record_bytes)
        || manifest["totalBytes"].as_u64() != Some(total_bytes);

    let needs_write = !had_manifest || files_changed || bytes_changed;
    if needs_write {
        if !manifest.is_object() {
            manifest = json!({
                "path": archive,
                "skipped": [],
                "createdAt": iso(),
            });
        }
        if s(&manifest, "path").is_empty() {
            manifest["path"] = json!(archive);
        }
        manifest["bytes"] = json!(files_bytes);
        manifest["recordBytes"] = json!(record_bytes);
        manifest["totalBytes"] = json!(total_bytes);
        manifest["files"] = json!(updated_files);
        write_json(&manifest_path, &manifest)?;
        if record_dir.is_dir() {
            let rec_arch = record_dir.join("archive.json");
            let _ = write_json(&rec_arch, &manifest);
        }
    }
    Ok((total_bytes, needs_write))
}

/// Walk `<cache>/delegate/archive` on disk, rewrite any stale `manifest.json` /
/// `record/archive.json` to match actual remaining files and byte sizes, and refresh
/// `.usage.json`.
pub fn rescan() -> Res<ArchiveRescan> {
    let root = cache_dir().join("archive");
    let prev = json(root.join(".usage.json"));
    let previous_bytes = prev["bytes"].as_u64();
    let previous_count = prev["count"].as_u64().map(|c| c as usize);
    if !root.is_dir() {
        return Ok(ArchiveRescan {
            root,
            count: 0,
            bytes: 0,
            updated_manifests: 0,
            previous_bytes,
            previous_count,
        });
    }
    let mut count = 0usize;
    let mut bytes = 0u64;
    let mut updated_manifests = 0usize;
    for entry in fs::read_dir(&root).into_iter().flatten().flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        count += 1;
        let (entry_bytes, updated) = rescan_one(&path)?;
        bytes = bytes.saturating_add(entry_bytes);
        if updated {
            updated_manifests += 1;
        }
    }
    write_usage_cache(&root, bytes, count);
    Ok(ArchiveRescan {
        root,
        count,
        bytes,
        updated_manifests,
        previous_bytes,
        previous_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentioned_inside_build_dir_requires_exact_file_reference() {
        let result = "Updated `backend/`, rebuilt `.venv/`, `target/`, `node_modules/`, `dist/`, \
                      `__pycache__/`, `.pytest_cache/`, and checked `.venv/pyvenv.cfg` and `target/mentioned`.";
        // Explicit individual files inside reconstructible dirs are still archived.
        assert!(mentioned(result, ".venv/pyvenv.cfg"));
        assert!(mentioned(result, "target/mentioned"));
        // Directory mentions (either the build dir itself or an ancestor above it) must NOT
        // sweep in arbitrary files inside reconstructible dirs.
        assert!(!mentioned(result, ".venv/lib/python3.11/site-packages/pkg.py"));
        assert!(!mentioned(result, "backend/.venv/lib/python3.11/site-packages/pkg.py"));
        assert!(!mentioned(result, "target/debug/app"));
        assert!(!mentioned(result, "node_modules/pkg/index.js"));
        assert!(!mentioned(result, "dist/bundle.js"));
        assert!(!mentioned(result, "__pycache__/mod.cpython-311.pyc"));
        assert!(!mentioned(result, ".pytest_cache/v/cache/lastfailed"));
        // Non-reconstructible directory mentions still sweep in their children.
        assert!(mentioned(result, "backend/report.txt"));
    }

    #[test]
    fn default_archive_warn_gib_is_30() {
        assert!((DEFAULT_ARCHIVE_WARN_GIB - 30.0).abs() < f64::EPSILON);
    }
}
