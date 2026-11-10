//! Source workspace observations, independent of task worktrees. Apply records exact
//! post-write fingerprints under the same lock used by observers: a later edit to
//! an applied path is not silently exempted.
use crate::common::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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
pub fn changed(meta: &Value) -> Vec<String> {
    changes_now(meta, false)
}
pub fn observe(meta: &Value, run: &Path) {
    let paths = changes_now(meta, true);
    if !paths.is_empty() {
        let _ = write_json(run.join("source-observed.json"), &json!(paths));
    }
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
