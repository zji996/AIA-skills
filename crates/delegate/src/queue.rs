//! Capacity queue. All decisions/reservations run under the machine start lock.
use crate::common::*;
use crate::{launch, runs};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

fn key(meta: &Value) -> (bool, u64) {
    (
        s(meta, "parent").is_empty(),
        meta["queuedNs"].as_u64().unwrap_or(0),
    )
}
fn waiting() -> Vec<(PathBuf, Value)> {
    let Ok(entries) = fs::read_dir(state_dir()) else {
        return vec![];
    };
    let mut out = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "slot"))
        .map(|e| PathBuf::from(read(e.path()).trim()))
        .filter(|run| {
            run.join(".waiting").exists()
                && run.join("queue.json").is_file()
                && runs::active(&runs::state(run))
        })
        .map(|run| {
            let meta = json(run.join("meta.json"));
            (run, meta)
        })
        .collect::<Vec<_>>();
    out.sort_by_key(|(_, meta)| key(meta));
    out
}
fn blocked(meta: &Value, active: &[PathBuf]) -> Res<Option<String>> {
    if let Some(reason) = launch::capacity(
        active,
        &meta["configCapacity"],
        &launch::repo_key_for_meta(meta),
    )? {
        return Ok(Some(reason));
    }
    if let Some(cap) = meta["modelCap"]["limit"].as_u64() {
        let count = active
            .iter()
            .filter(|run| s(&json(run.join("meta.json")), "model") == s(meta, "model"))
            .count();
        if count as u64 >= cap {
            return Ok(Some(format!(
                "模型并发 / model {} (maxActive={cap})",
                s(&meta["modelCap"], "name")
            )));
        }
    }
    Ok(None)
}
/// Reserve capacity virtually for eligible older/higher-priority waiters. A waiter
/// blocked on its model (or repo) is skipped, not a head-of-line barrier.
pub fn reason(meta: &Value, active: &[PathBuf]) -> Res<Option<String>> {
    let mut reserved = active.to_vec();
    for (run, prior) in waiting() {
        if key(&prior) >= key(meta) {
            break;
        }
        if blocked(&prior, &reserved)?.is_none() {
            reserved.push(run);
        }
    }
    blocked(meta, &reserved)
}
pub fn enter(run: &Path, meta: &mut Value) -> Res<()> {
    meta["queuedNs"] = json!(now_ns() as u64);
    meta["queuedAt"] = json!(iso());
    write_json(run.join("meta.json"), meta)?;
    update(run, "总路数 / machine capacity", 0)
}
pub fn update(run: &Path, reason: &str, ahead: usize) -> Res<()> {
    let meta = json(run.join("meta.json"));
    let record = json!({"reason":reason,"ahead":ahead,"since":meta["queuedAt"]});
    let path = run.join("queue.json");
    if json(&path) == record {
        return Ok(());
    }
    write_json(path, &record)
}
/// Status is a live view; it does not admit or allocate anything.
pub fn status(run: &Path) -> Option<Value> {
    if !run.join(".waiting").exists() || !run.join("queue.json").is_file() {
        return None;
    }
    let _guard = lock(&state_dir().join(".start.lock"), true, false).ok()?;
    if !run.join(".waiting").exists() {
        return None;
    }
    let meta = json(run.join("meta.json"));
    let active = launch::active_machine(&state_dir());
    let mut out = json(run.join("queue.json"));
    if let Ok(Some(reason)) = reason(&meta, &active) {
        out["reason"] = json!(reason);
    }
    out["ahead"] = json!(waiting()
        .iter()
        .filter(|(_, other)| key(other) < key(&meta))
        .count());
    Some(out)
}
pub fn notice(run: &Path) {
    if let Some(queue) = status(run) {
        eprintln!(
            "delegate: queued: {}; starts automatically\ndelegate: 已排队：{}；将自动开始",
            s(&queue, "reason"),
            s(&queue, "reason")
        );
    }
}
