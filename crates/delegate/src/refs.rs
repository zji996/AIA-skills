//! Pin Git objects referenced by delegate runs under `refs/delegate/<run-id>/*`
//! so `git prune` and `git gc` keep them reachable until `delegate clean`.
use crate::common::*;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

const MARKER: &str = ".refs-pinned";
const MARKER_ACTIVE: &str = "v1:active";
const MARKER_DONE: &str = "v1:done";

fn is_oid(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_component(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("@{")
        && name.bytes().all(|b| {
            b > 0x20
                && b != 0x7f
                && !matches!(b, b'/' | b'\\' | b'~' | b'^' | b':' | b'?' | b'*' | b'[')
        })
}

/// Deterministic Git ref path segment for a run id; valid ids stay verbatim.
pub fn ref_component(run_id: &str) -> String {
    if valid_component(run_id) {
        return run_id.to_string();
    }
    let mut clean: String = run_id
        .chars()
        .map(|c| {
            let b = c as u32;
            if b <= 0x20
                || b == 0x7f
                || matches!(c, '/' | '\\' | '~' | '^' | ':' | '?' | '*' | '[')
            {
                '-'
            } else {
                c
            }
        })
        .collect();
    while clean.contains("..") {
        clean = clean.replace("..", "-");
    }
    while clean.contains("@{") {
        clean = clean.replace("@{", "-");
    }
    let clean = clean.trim_matches('.').trim_end_matches(".lock");
    let digest = sha1_smol::Sha1::from(run_id.as_bytes()).digest().to_string();
    if clean.is_empty() {
        format!("run-{}", &digest[..8])
    } else {
        format!("{clean}-{}", &digest[..8])
    }
}

pub fn ref_prefix(run_id: &str) -> String {
    format!("refs/delegate/{}/", ref_component(run_id))
}

fn run_id(run: &Path, meta: &Value) -> String {
    let from_meta = s(meta, "run");
    if !from_meta.is_empty() {
        from_meta.to_string()
    } else {
        run.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }
}

fn is_git_common_dir(path: &Path) -> bool {
    path.join("HEAD").is_file() && path.join("objects").is_dir()
}

pub fn git_common_dir(path: &Path) -> Option<PathBuf> {
    if !path.is_dir() {
        return None;
    }
    if is_git_common_dir(path) {
        return Some(path.to_path_buf());
    }
    git_text(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()
    .map(PathBuf::from)
    .filter(|dir| dir.is_dir())
}

pub fn repo_git_dir(run: &Path, meta: &Value) -> Option<PathBuf> {
    let rec = json(run.join("changes.json"));
    for candidate in [
        s(meta, "repoKey"),
        s(&meta["worktree"], "source"),
        s(meta, "top"),
        s(&rec, "top"),
        s(meta, "sourceBuildRoot"),
        s(meta, "workdir"),
    ] {
        if candidate.is_empty() {
            continue;
        }
        if let Some(dir) = git_common_dir(Path::new(candidate)) {
            return Some(dir);
        }
    }
    None
}

pub fn collect_slots(run: &Path, meta: &Value) -> Vec<(&'static str, String)> {
    let mut slots = Vec::new();
    let mut push = |slot: &'static str, oid: &str| {
        let oid = oid.trim();
        if is_oid(oid) {
            slots.push((slot, oid.to_string()));
        }
    };
    let base = s(&meta["base"], "tree");
    push("base", base);
    let chain_base = s(meta, "chainBase");
    if !chain_base.is_empty() && chain_base != base {
        push("chain-base", chain_base);
    }
    let worktree_base = read(run.join("worktree-base-commit"));
    push("worktree-base", worktree_base.trim());
    push("net-base", s(meta, "netBase"));
    let rec = json(run.join("changes.json"));
    let rec_base = s(&rec, "base");
    if base.is_empty() && !rec_base.is_empty() {
        push("base", rec_base);
    }
    push("after", s(&rec, "after"));
    let sync_base = json(run.join(".sync-base"));
    push("sync-base", s(&sync_base, "tree"));
    let applied = json(run.join(".applied"));
    push("applied", s(&applied, "tree"));
    push("applied-base", s(&meta["appliedBase"], "tree"));
    slots
}

fn decode_sha1(oid: &str) -> Option<[u8; 20]> {
    if oid.len() != 40 {
        return None;
    }
    let bytes = oid.as_bytes();
    let mut out = [0u8; 20];
    for i in 0..20 {
        let hi = (bytes[i * 2] as char).to_digit(16)?;
        let lo = (bytes[i * 2 + 1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

fn object_dirs(git_dir: &Path) -> Vec<PathBuf> {
    let main = git_dir.join("objects");
    let mut dirs = vec![main.clone()];
    let mut seen = BTreeSet::new();
    seen.insert(main.clone());
    let mut idx = 0;
    while idx < dirs.len() {
        let alts = dirs[idx].join("info/alternates");
        idx += 1;
        for line in read(&alts).lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let p = PathBuf::from(line);
            let p = if p.is_absolute() {
                p
            } else {
                dirs[idx - 1].join(p)
            };
            if p.is_dir() && seen.insert(p.clone()) {
                dirs.push(p);
            }
        }
    }
    dirs
}

/// Pure-filesystem check for SHA-1 loose and v2-packed objects (no `git` spawn).
/// Returns `None` only when a non-v2 pack index or non-SHA-1 OID requires `git cat-file`.
fn object_exists_fs(obj_dirs: &[PathBuf], oid: &str) -> Option<bool> {
    let hash = decode_sha1(oid)?;
    let (prefix, suffix) = (&oid[..2], &oid[2..]);
    let mut unknown = false;
    for dir in obj_dirs {
        if dir.join(prefix).join(suffix).is_file() {
            return Some(true);
        }
        let pack_dir = dir.join("pack");
        let Ok(entries) = std::fs::read_dir(&pack_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("idx") {
                continue;
            }
            let Ok(data) = std::fs::read(&path) else {
                unknown = true;
                continue;
            };
            if data.len() < 1032 || &data[0..8] != b"\xfftOc\x00\x00\x00\x02" {
                unknown = true;
                continue;
            }
            let u32_at = |offset: usize| -> usize {
                u32::from_be_bytes([
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                ]) as usize
            };
            let b0 = hash[0] as usize;
            let mut lo = if b0 == 0 { 0 } else { u32_at(8 + (b0 - 1) * 4) };
            let mut hi = u32_at(8 + b0 * 4);
            let total = u32_at(8 + 255 * 4);
            let table_end = 1032usize.saturating_add(total.saturating_mul(20));
            if lo > hi || hi > total || data.len() < table_end {
                unknown = true;
                continue;
            }
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let off = 1032 + mid * 20;
                match data[off..off + 20].cmp(&hash) {
                    std::cmp::Ordering::Equal => return Some(true),
                    std::cmp::Ordering::Less => lo = mid + 1,
                    std::cmp::Ordering::Greater => hi = mid,
                }
            }
        }
    }
    if unknown { None } else { Some(false) }
}

pub fn existing_objects(git_dir: &Path, oids: &[&str]) -> BTreeSet<String> {
    let unique: BTreeSet<&str> = oids.iter().copied().filter(|oid| is_oid(oid)).collect();
    if unique.is_empty() {
        return BTreeSet::new();
    }
    let obj_dirs = object_dirs(git_dir);
    let mut found = BTreeSet::new();
    let mut fallback = Vec::new();
    for oid in unique {
        match object_exists_fs(&obj_dirs, oid) {
            Some(true) => {
                found.insert(oid.to_string());
            }
            Some(false) => {}
            None => fallback.push(oid),
        }
    }
    if fallback.is_empty() {
        return found;
    }
    let mut cmd = git_command();
    cmd.arg("--git-dir")
        .arg(git_dir)
        .args(["cat-file", "--batch-check"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        return found;
    };
    if let Some(mut stdin) = child.stdin.take() {
        for oid in &fallback {
            let _ = writeln!(stdin, "{oid}");
        }
    }
    let Ok(output) = child.wait_with_output() else {
        return found;
    };
    if !output.status.success() {
        return found;
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some((oid, rest)) = line.split_once(' ') {
            if rest != "missing" {
                found.insert(oid.to_string());
            }
        }
    }
    found
}

fn update_refs(git_dir: &Path, instructions: &str) {
    if instructions.is_empty() {
        return;
    }
    let mut cmd = git_command();
    cmd.arg("--git-dir")
        .arg(git_dir)
        .args(["update-ref", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(instructions.as_bytes());
    }
    let _ = child.wait();
}

pub fn pin_slots(run: &Path, meta: &Value, slots: &[(&str, &str)]) {
    let valid: Vec<(&str, &str)> = slots
        .iter()
        .map(|(slot, oid)| (*slot, oid.trim()))
        .filter(|(_, oid)| is_oid(oid))
        .collect();
    if valid.is_empty() {
        return;
    }
    let Some(git_dir) = repo_git_dir(run, meta) else {
        return;
    };
    let prefix = ref_prefix(&run_id(run, meta));
    let mut batch = String::new();
    for (slot, oid) in valid {
        batch.push_str(&format!("update {prefix}{slot} {oid}\n"));
    }
    update_refs(&git_dir, &batch);
}

pub fn pin_slot(run: &Path, meta: &Value, slot: &str, oid: &str) {
    pin_slots(run, meta, &[(slot, oid)]);
}

pub fn premerge_ref_name(run_id: &str) -> String {
    format!("refs/delegate/premerge/{}", ref_component(run_id))
}

/// Pin a premerged tree OID under `refs/delegate/premerge/<run-id>` so `git gc` keeps it alive.
pub fn pin_premerge(run: &Path, meta: &Value, merged_tree: &str) -> Option<String> {
    let oid = merged_tree.trim();
    if !is_oid(oid) {
        return None;
    }
    let git_dir = repo_git_dir(run, meta)?;
    let refname = premerge_ref_name(&run_id(run, meta));
    update_refs(&git_dir, &format!("update {refname} {oid}\n"));
    Some(refname)
}

fn write_marker(run: &Path, finished: bool) {
    if run.is_dir() && !crate::archive::is_record(run) {
        let _ = write(
            run.join(MARKER),
            format!("{}\n", if finished { MARKER_DONE } else { MARKER_ACTIVE }),
        );
    }
}

/// Pin every existing object referenced by `run` and record migration state.
pub fn pin_run(run: &Path, meta: &Value) {
    let slots = collect_slots(run, meta);
    if !slots.is_empty() {
        if let Some(git_dir) = repo_git_dir(run, meta) {
            let oids: Vec<&str> = slots.iter().map(|(_, oid)| oid.as_str()).collect();
            let present = existing_objects(&git_dir, &oids);
            let prefix = ref_prefix(&run_id(run, meta));
            let mut batch = String::new();
            for (slot, oid) in &slots {
                if present.contains(oid) {
                    batch.push_str(&format!("update {prefix}{slot} {oid}\n"));
                }
            }
            update_refs(&git_dir, &batch);
        }
    }
    write_marker(run, run.join("exit_code").is_file());
}

pub fn mark_finished(run: &Path) {
    let meta = json(run.join("meta.json"));
    pin_run(run, &meta);
    write_marker(run, true);
}

pub fn delete_run(run: &Path, meta: &Value) {
    let Some(git_dir) = repo_git_dir(run, meta) else {
        return;
    };
    let id = run_id(run, meta);
    let prefix = ref_prefix(&id);
    let premerge_ref = premerge_ref_name(&id);
    let Ok(output) = cmd_output({
        let mut cmd = git_command();
        cmd.arg("--git-dir")
            .arg(&git_dir)
            .args(["for-each-ref", "--format=%(refname)", &prefix, &premerge_ref]);
        cmd
    }) else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let mut batch = String::new();
    for refname in String::from_utf8_lossy(&output.stdout).lines() {
        let refname = refname.trim();
        if !refname.is_empty() {
            batch.push_str(&format!("delete {refname}\n"));
        }
    }
    update_refs(&git_dir, &batch);
}

fn needs_backfill(run: &Path) -> bool {
    if crate::archive::is_record(run) || !run.join("meta.json").is_file() {
        return false;
    }
    let marker = read(run.join(MARKER));
    let marker = marker.trim();
    if marker == MARKER_DONE {
        return false;
    }
    if marker == MARKER_ACTIVE && !run.join("exit_code").is_file() {
        return false;
    }
    true
}

/// On first 6.22.1 command, backfill refs for all uncleaned runs created before 6.22.1.
pub fn backfill_all() {
    let runs: Vec<PathBuf> = crate::runs::all_runs()
        .into_iter()
        .filter(|run| needs_backfill(run))
        .collect();
    if runs.is_empty() {
        return;
    }
    type Group = Vec<(PathBuf, String, Vec<(&'static str, String)>, bool)>;
    let mut by_repo: BTreeMap<PathBuf, Group> = BTreeMap::new();
    for run in runs {
        let meta = json(run.join("meta.json"));
        let finished = run.join("exit_code").is_file();
        let slots = collect_slots(&run, &meta);
        if let Some(git_dir) = (!slots.is_empty()).then(|| repo_git_dir(&run, &meta)).flatten() {
            let id = run_id(&run, &meta);
            by_repo.entry(git_dir).or_default().push((run, id, slots, finished));
        } else {
            write_marker(&run, finished);
        }
    }
    for (git_dir, group) in by_repo {
        let oids: Vec<&str> = group
            .iter()
            .flat_map(|(_, _, slots, _)| slots.iter().map(|(_, oid)| oid.as_str()))
            .collect();
        let present = existing_objects(&git_dir, &oids);
        let mut batch = String::new();
        for (_, id, slots, _) in &group {
            let prefix = ref_prefix(id);
            for (slot, oid) in slots {
                if present.contains(oid) {
                    batch.push_str(&format!("update {prefix}{slot} {oid}\n"));
                }
            }
        }
        update_refs(&git_dir, &batch);
        for (run, _, slots, finished) in group {
            let missing: Vec<Value> = slots
                .iter()
                .filter(|(_, oid)| !present.contains(oid))
                .map(|(slot, oid)| serde_json::json!({"ref": slot, "sha": oid}))
                .collect();
            if !missing.is_empty() {
                let _ = write_json(run.join("missing-objects.json"), &serde_json::json!(missing));
            } else {
                let _ = std::fs::remove_file(run.join("missing-objects.json"));
            }
            write_marker(&run, finished);
        }
    }
}

#[allow(dead_code)]
pub fn backfill_one(run: &Path) {
    if !needs_backfill(run) {
        return;
    }
    let meta = json(run.join("meta.json"));
    pin_run(run, &meta);
}

pub fn missing_objects(run: &Path, meta: &Value) -> Vec<(&'static str, String)> {
    let slots = collect_slots(run, meta);
    if slots.is_empty() {
        return Vec::new();
    }
    let Some(git_dir) = repo_git_dir(run, meta) else {
        return Vec::new();
    };
    let oids: Vec<&str> = slots.iter().map(|(_, oid)| oid.as_str()).collect();
    let present = existing_objects(&git_dir, &oids);
    slots
        .into_iter()
        .filter(|(_, oid)| !present.contains(oid))
        .collect()
}

pub fn missing_in_top(
    top: &Path,
    run: &Path,
    meta: &Value,
    pairs: &[(&'static str, &str)],
) -> Vec<(&'static str, String)> {
    let checked: Vec<(&'static str, &str)> = pairs
        .iter()
        .map(|(slot, oid)| (*slot, oid.trim()))
        .filter(|(_, oid)| is_oid(oid))
        .collect();
    if checked.is_empty() {
        return Vec::new();
    }
    let Some(git_dir) = git_common_dir(top).or_else(|| repo_git_dir(run, meta)) else {
        return Vec::new();
    };
    let oids: Vec<&str> = checked.iter().map(|(_, oid)| *oid).collect();
    let present = existing_objects(&git_dir, &oids);
    checked
        .into_iter()
        .filter(|(_, oid)| !present.contains(*oid))
        .map(|(slot, oid)| (slot, oid.to_string()))
        .collect()
}

pub fn recovery_hint(run: &Path, meta: &Value) -> String {
    let wt = s(&meta["worktree"], "path");
    let id = run_id(run, meta);
    let has_patch = run.join("applied.patch").is_file() || run.join("changes.patch").is_file();
    if !wt.is_empty() && Path::new(wt).is_dir() {
        let q = shell_quote(wt);
        let patch = if has_patch {
            format!("（也可用 {} patch {id} 查看保留补丁）", script().display())
        } else {
            String::new()
        };
        format!("手工救回：未跟踪文件先 git -C {q} add -N .，再 git -C {q} diff HEAD{patch}")
    } else if has_patch {
        format!(
            "worktree 已不存在，可用 {} patch {id} 查看保留补丁",
            script().display()
        )
    } else {
        "worktree 已不存在，无法从工作树恢复".into()
    }
}

pub fn recovery_message(run: &Path, meta: &Value, missing: &[(&str, String)]) -> String {
    let list = missing
        .iter()
        .map(|(slot, oid)| format!("{slot}={}", &oid[..oid.len().min(12)]))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "快照对象已丢失（{list}，可能被 git prune 清理）；{}",
        recovery_hint(run, meta)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_component_keeps_valid_ids_and_sanitizes_invalid_ones() {
        assert_eq!(ref_component("20261121-100000-fix-bug"), "20261121-100000-fix-bug");
        assert_eq!(ref_component("20261121-100000-修复登录"), "20261121-100000-修复登录");
        for bad in ["a..b", "foo.lock", ".hidden", "trail.", "a/b", "a@{b}", "a b"] {
            let safe = ref_component(bad);
            assert!(valid_component(&safe), "{bad} -> {safe}");
            assert_ne!(safe, bad);
        }
    }

    #[test]
    fn fs_object_check_finds_loose_and_packed_objects_and_detects_missing() {
        let base = std::env::temp_dir().join(format!("delegate-refs-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let g = |args: &[&str]| {
            let out = git_command()
                .arg("-C")
                .arg(&base)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        g(&["init", "-q"]);
        std::fs::write(base.join("a.txt"), "hello\n").unwrap();
        g(&["add", "a.txt"]);
        g(&["commit", "-qm", "init"]);
        let tree = g(&["rev-parse", "HEAD^{tree}"]);
        let git_dir = base.join(".git");
        let dirs = object_dirs(&git_dir);
        let missing = "0".repeat(40);
        assert_eq!(object_exists_fs(&dirs, &tree), Some(true));
        assert_eq!(object_exists_fs(&dirs, &missing), Some(false));
        g(&["gc", "--quiet"]);
        assert_eq!(object_exists_fs(&dirs, &tree), Some(true));
        assert_eq!(object_exists_fs(&dirs, &missing), Some(false));
        let _ = std::fs::remove_dir_all(&base);
    }
}
