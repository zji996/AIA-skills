//! Append-only follow-up mailbox for active runs, with race-safe turn-boundary delivery
//! and post-settle fallback (`pending-reply.json` + `_reply-pending` worker).
//!
//! Locking discipline:
//! - Every mutating operation (`enqueue`, `take_or_close_in_run`, `close_in_run`,
//!   `revert_to_fallback`, `cancel`, `deliver`) synchronizes on `<run>/reply.lock`
//!   (`LockKind::ReplyMailbox`).
//! - Read-only paths (`status`, `summary_json`, `append_delivered_rounds`) read
//!   atomically-renamed JSON files without taking any lock.
use crate::common::*;
use crate::{launch, runs};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub const APPEND_HEADER: &str =
    "追加问题（主控在你作答期间补充）：把它并进你的完整答复，最终答复要覆盖原任务和全部追加";

const APPENDS_FILE: &str = "appends.json";
const PENDING_FILE: &str = "pending-reply.json";
const LOCK_FILE: &str = "reply.lock";

pub const PI_RPC_STEER_MARKER: &str = ".pi-rpc-steer";

#[derive(Clone, Debug)]
pub struct AppendItem {
    pub seq: u64,
    pub message: String,
    pub images: Vec<Value>,
    pub args: Vec<String>,
    pub cwd: String,
    pub queued_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnqueueMode {
    InRun,
    InRunSteer,
    FallbackClosing,
    FallbackOptions,
}

#[derive(Clone, Debug)]
pub struct EnqueueOutcome {
    pub run: PathBuf,
    pub name: String,
    pub seq: u64,
    pub mode: EnqueueMode,
}

impl EnqueueOutcome {
    pub fn mode_str(&self) -> &'static str {
        match self.mode {
            EnqueueMode::InRun => "in_run",
            EnqueueMode::InRunSteer => "steer",
            EnqueueMode::FallbackClosing | EnqueueMode::FallbackOptions => "fallback",
        }
    }

    pub fn human_line(&self) -> String {
        match self.mode {
            EnqueueMode::InRun => format!(
                "已追加到 {}：当前轮结束后在同一会话接着答（第 {} 条追加）",
                self.name, self.seq
            ),
            EnqueueMode::InRunSteer => format!(
                "已追加到 {}：当前工具调用结束后通过 steer 注入同一会话（第 {} 条追加）",
                self.name, self.seq
            ),
            EnqueueMode::FallbackClosing => format!(
                "已追加到 {}：当前轮已在收尾，已退回结束后同会话开新 round（第 {} 条追加）",
                self.name, self.seq
            ),
            EnqueueMode::FallbackOptions => format!(
                "已追加到 {}：指定了新会话或模型变更，已退回结束后开新 round（第 {} 条追加）",
                self.name, self.seq
            ),
        }
    }
}

#[derive(Clone, Debug)]
pub enum TakeInRun {
    Messages(Vec<AppendItem>),
    Closed,
}

/// Lock-free read of the post-settle fallback file (`pending-reply.json`), if present.
/// Used by `gc`, `clean`, `apply`, and `runs::status` to protect runs that still have
/// an undelivered post-settle follow-up round.
pub fn status(run: &Path) -> Option<Value> {
    let value = json(run.join(PENDING_FILE));
    value.is_object().then_some(value)
}

fn load_mailbox(run: &Path) -> Value {
    let value = json(run.join(APPENDS_FILE));
    if value.is_object() && value["items"].is_array() {
        value
    } else {
        json!({
            "closed": false,
            "closedAt": Value::Null,
            "nextSeq": 1,
            "items": [],
        })
    }
}

fn item_from_value(v: &Value) -> AppendItem {
    AppendItem {
        seq: v["seq"].as_u64().unwrap_or(0),
        message: s(v, "message").to_string(),
        images: v["images"].as_array().cloned().unwrap_or_default(),
        args: strings(&v["args"]),
        cwd: s(v, "cwd").to_string(),
        queued_at: s(v, "queuedAt").to_string(),
    }
}

/// Format one or more appended messages into the single merged prompt sent at a turn boundary.
pub fn format_merged_followup(items: &[AppendItem]) -> String {
    let mut text = String::from(APPEND_HEADER);
    text.push_str("\n\n");
    for (idx, item) in items.iter().enumerate() {
        if idx > 0 {
            text.push_str("\n\n");
        }
        let body = item.message.trim_end();
        text.push_str(&format!("{}. {body}", item.seq));
    }
    text
}

/// Collect unique image attachments across a batch of appended messages, preserving order.
pub fn merged_images(items: &[AppendItem]) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        for img in &item.images {
            if let Some(p) = img.as_str() {
                if !p.is_empty() && seen.insert(p.to_string()) {
                    out.push(json!(p));
                }
            }
        }
    }
    out
}

fn run_display_name(run: &Path) -> String {
    let meta = json(run.join("meta.json"));
    let name = s(&meta, "name");
    if !name.is_empty() {
        name.to_string()
    } else {
        run.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }
}

fn format_fallback_message(items: &[AppendItem]) -> String {
    if items.len() == 1 {
        return items[0].message.clone();
    }
    format_merged_followup(items)
}

fn write_fallback_file_and_spawn(
    run: &Path,
    mailbox: &Value,
    had_active_pending: bool,
) -> Res<()> {
    let pending_items: Vec<AppendItem> = mailbox["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|it| {
            s(it, "delivery") == "fallback"
                && it["deliveredAt"].is_null()
                && it["cancelledAt"].is_null()
        })
        .map(item_from_value)
        .collect();
    if pending_items.is_empty() {
        return Ok(());
    }
    let last = pending_items.last().unwrap();
    let first = pending_items.first().unwrap();
    let merged_msg = format_fallback_message(&pending_items);
    let merged_imgs = merged_images(&pending_items);
    let all_args: Vec<Vec<String>> = pending_items
        .iter()
        .filter(|it| !it.args.is_empty())
        .map(|it| it.args.clone())
        .collect();
    let messages_json: Vec<Value> = pending_items
        .iter()
        .map(|it| {
            json!({
                "seq": it.seq,
                "message": it.message,
                "queuedAt": it.queued_at,
            })
        })
        .collect();
    let saved = json!({
        "args": last.args,
        "allArgs": all_args,
        "images": merged_imgs,
        "message": merged_msg,
        "messages": messages_json,
        "count": pending_items.len(),
        "queuedAt": first.queued_at,
        "cwd": last.cwd,
    });
    write_json(run.join(PENDING_FILE), &saved)?;
    if !had_active_pending {
        let mut command = Command::new(script());
        command
            .arg("_reply-pending")
            .arg(run)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        group(&mut command);
        if let Err(error) = command.spawn() {
            let _ = fs::remove_file(run.join(PENDING_FILE));
            return Err(error.to_string());
        }
    }
    Ok(())
}

/// Enqueue a follow-up message onto an active run.
///
/// Returns `Ok(None)` when the target run is no longer active so the caller falls through
/// to normal `launch::reply`. Otherwise appends to `<run>/appends.json` under `reply.lock`,
/// choosing in-run turn-boundary delivery when the channel is open and session/model options
/// are unchanged, or falling back to post-settle `pending-reply.json` + `_reply-pending`.
pub fn enqueue(args: &[String], o: &mut launch::Options) -> Res<Option<EnqueueOutcome>> {
    let run = runs::resolve_head(o.run.as_deref().unwrap_or("last"))?;
    if !runs::active(&runs::state(&run)) {
        return Ok(None);
    }
    if o.wait {
        return Err(
            "cannot combine --wait when appending to an active run; collect the reply later".into(),
        );
    }
    // Snapshot stdin/files now before taking the lock (and preserve in `o` in case the run
    // finishes before we acquire the lock and we fall through to normal `launch::reply`).
    let prompt = launch::read_prompt(o)?;
    o.prompt = Some(prompt.clone());
    o.prompt_file = None;
    o.words.clear();

    let cwd = std::env::current_dir()
        .map_err(|e| e.to_string())?
        .to_string_lossy()
        .into_owned();
    let images: Vec<Value> = o.images.iter().map(|p| json!(p)).collect();
    let pi_rpc_steer = run.join(PI_RPC_STEER_MARKER).is_file();
    let force_fallback = o.fresh
        || o.model.is_some()
        || o.tier.is_some()
        || o.thinking_cli.is_some()
        || (pi_rpc_steer && !o.images.is_empty());

    let outcome = {
        let _guard = crate::locks::acquire(
            crate::locks::LockKind::ReplyMailbox,
            &run.join(LOCK_FILE),
            true,
        )
        .map_err(|e| e.to_string())?;
        if !runs::active(&runs::state(&run)) {
            return Ok(None);
        }
        let mut mailbox = load_mailbox(&run);
        let closed = b(&mailbox, "closed");
        let seq = mailbox["nextSeq"].as_u64().unwrap_or(1).max(1);
        mailbox["nextSeq"] = json!(seq + 1);
        let name = run_display_name(&run);

        let use_fallback = closed || force_fallback;
        let delivery = if use_fallback { "fallback" } else { "in_run" };
        let item = json!({
            "seq": seq,
            "message": prompt,
            "images": images,
            "args": args,
            "cwd": cwd,
            "queuedAt": iso(),
            "deliveredAt": Value::Null,
            "deliveredRound": Value::Null,
            "delivery": delivery,
            "cancelledAt": Value::Null,
        });
        if let Some(arr) = mailbox["items"].as_array_mut() {
            arr.push(item);
        }
        if let Some(obj) = mailbox.as_object_mut() {
            obj.remove("fallbackError");
        }
        if use_fallback {
            let existing = status(&run);
            let had_active_pending = existing
                .as_ref()
                .is_some_and(|v| s(v, "error").is_empty());
            write_json(run.join(APPENDS_FILE), &mailbox)?;
            write_fallback_file_and_spawn(&run, &mailbox, had_active_pending)?;
            EnqueueOutcome {
                run: run.clone(),
                name,
                seq,
                mode: if closed {
                    EnqueueMode::FallbackClosing
                } else {
                    EnqueueMode::FallbackOptions
                },
            }
        } else {
            write_json(run.join(APPENDS_FILE), &mailbox)?;
            let is_steer = run.join(PI_RPC_STEER_MARKER).is_file();
            EnqueueOutcome {
                run: run.clone(),
                name,
                seq,
                mode: if is_steer {
                    EnqueueMode::InRunSteer
                } else {
                    EnqueueMode::InRun
                },
            }
        }
    };
    crate::index::upsert_run(&run);
    crate::board::refresh_for_run(&run);
    Ok(Some(outcome))
}

/// Drain currently pending `in_run` items for mid-turn Pi RPC `steer` injection without closing
/// the mailbox. Marks taken items with `delivery = "steer"` and `deliveredAt = now`.
pub fn take_pending_in_run_for_steer(run: &Path) -> Res<Vec<AppendItem>> {
    if !run.join(APPENDS_FILE).is_file() {
        return Ok(Vec::new());
    }
    let _guard = crate::locks::acquire(
        crate::locks::LockKind::ReplyMailbox,
        &run.join(LOCK_FILE),
        true,
    )
    .map_err(|e| e.to_string())?;
    let mut mailbox = load_mailbox(run);
    if b(&mailbox, "closed") || run.join(".stopped").exists() {
        return Ok(Vec::new());
    }
    let now = iso();
    let mut taken = Vec::new();
    if let Some(arr) = mailbox["items"].as_array_mut() {
        for item in arr.iter_mut() {
            if s(item, "delivery") == "in_run"
                && item["deliveredAt"].is_null()
                && item["cancelledAt"].is_null()
            {
                item["deliveredAt"] = json!(&now);
                item["delivery"] = json!("steer");
                taken.push(item_from_value(item));
            }
        }
    }
    if !taken.is_empty() {
        write_json(run.join(APPENDS_FILE), &mailbox)?;
    }
    Ok(taken)
}

/// If a mid-turn RPC `steer` command was rejected by the agent, restore those items as pending
/// `in_run` (or promote to `fallback` if the in-run channel already closed) so they are delivered
/// at turn end or in a post-settle round without loss.
pub fn revert_steer_to_in_run(run: &Path, items: &[AppendItem]) -> Res<()> {
    let seqs: HashSet<u64> = items.iter().map(|it| it.seq).collect();
    if seqs.is_empty() {
        return Ok(());
    }
    let _guard = crate::locks::acquire(
        crate::locks::LockKind::ReplyMailbox,
        &run.join(LOCK_FILE),
        true,
    )
    .map_err(|e| e.to_string())?;
    let mut mailbox = load_mailbox(run);
    let closed = b(&mailbox, "closed");
    let stopped = run.join(".stopped").exists();
    let now = iso();
    let mut promoted = false;
    if let Some(arr) = mailbox["items"].as_array_mut() {
        for item in arr.iter_mut() {
            let seq = item["seq"].as_u64().unwrap_or(0);
            if seqs.contains(&seq) && item["cancelledAt"].is_null() {
                item["deliveredAt"] = Value::Null;
                item["deliveredRound"] = Value::Null;
                if stopped {
                    item["cancelledAt"] = json!(&now);
                } else if closed {
                    item["delivery"] = json!("fallback");
                    promoted = true;
                } else {
                    item["delivery"] = json!("in_run");
                }
            }
        }
    }
    write_json(run.join(APPENDS_FILE), &mailbox)?;
    if promoted {
        let had_active_pending = status(run)
            .as_ref()
            .is_some_and(|v| s(v, "error").is_empty());
        write_fallback_file_and_spawn(run, &mailbox, had_active_pending)?;
    }
    Ok(())
}

/// Promote specific items (e.g. image-bearing appends queued before `.pi-rpc-steer` existed)
/// to post-settle fallback WITHOUT closing the in-run text channel.
pub fn promote_items_to_fallback(run: &Path, items: &[AppendItem]) -> Res<()> {
    let seqs: HashSet<u64> = items.iter().map(|it| it.seq).collect();
    if seqs.is_empty() {
        return Ok(());
    }
    let _guard = crate::locks::acquire(
        crate::locks::LockKind::ReplyMailbox,
        &run.join(LOCK_FILE),
        true,
    )
    .map_err(|e| e.to_string())?;
    let mut mailbox = load_mailbox(run);
    let stopped = run.join(".stopped").exists();
    let now = iso();
    if let Some(arr) = mailbox["items"].as_array_mut() {
        for item in arr.iter_mut() {
            let seq = item["seq"].as_u64().unwrap_or(0);
            if seqs.contains(&seq) && item["cancelledAt"].is_null() {
                item["deliveredAt"] = Value::Null;
                item["deliveredRound"] = Value::Null;
                if stopped {
                    item["cancelledAt"] = json!(&now);
                } else {
                    item["delivery"] = json!("fallback");
                }
            }
        }
    }
    write_json(run.join(APPENDS_FILE), &mailbox)?;
    if !stopped {
        let had_active_pending = status(run)
            .as_ref()
            .is_some_and(|v| s(v, "error").is_empty());
        write_fallback_file_and_spawn(run, &mailbox, had_active_pending)?;
    }
    Ok(())
}

/// Atomically re-claim unconsumed `steer` items (confirmed still `queued` by `get_queue` after
/// `agent_settled`) together with any newly arrived `in_run` items for `next_round`.
pub fn take_next_round_with_requeued(
    run: &Path,
    requeued: &[AppendItem],
    next_round: usize,
) -> Res<TakeInRun> {
    let requeued_seqs: HashSet<u64> = requeued.iter().map(|it| it.seq).collect();
    let _guard = crate::locks::acquire(
        crate::locks::LockKind::ReplyMailbox,
        &run.join(LOCK_FILE),
        true,
    )
    .map_err(|e| e.to_string())?;
    let mut mailbox = load_mailbox(run);
    let now = iso();
    if run.join(".stopped").exists() {
        if let Some(arr) = mailbox["items"].as_array_mut() {
            for item in arr.iter_mut() {
                let seq = item["seq"].as_u64().unwrap_or(0);
                if (item["deliveredAt"].is_null() || requeued_seqs.contains(&seq))
                    && item["cancelledAt"].is_null()
                {
                    item["deliveredAt"] = Value::Null;
                    item["deliveredRound"] = Value::Null;
                    item["cancelledAt"] = json!(&now);
                }
            }
        }
        mailbox["closed"] = json!(true);
        if mailbox["closedAt"].is_null() {
            mailbox["closedAt"] = json!(&now);
        }
        write_json(run.join(APPENDS_FILE), &mailbox)?;
        return Ok(TakeInRun::Closed);
    }
    let mut taken = Vec::new();
    if let Some(arr) = mailbox["items"].as_array_mut() {
        for item in arr.iter_mut() {
            if !item["cancelledAt"].is_null() {
                continue;
            }
            let seq = item["seq"].as_u64().unwrap_or(0);
            let is_requeued = requeued_seqs.contains(&seq);
            let is_pending_in_run =
                s(item, "delivery") == "in_run" && item["deliveredAt"].is_null();
            if is_requeued || is_pending_in_run {
                item["delivery"] = json!("in_run");
                item["deliveredAt"] = json!(&now);
                item["deliveredRound"] = json!(next_round);
                taken.push(item_from_value(item));
            }
        }
    }
    taken.sort_by_key(|it| it.seq);
    if !taken.is_empty() {
        write_json(run.join(APPENDS_FILE), &mailbox)?;
        return Ok(TakeInRun::Messages(taken));
    }
    mailbox["closed"] = json!(true);
    if mailbox["closedAt"].is_null() {
        mailbox["closedAt"] = json!(&now);
    }
    write_json(run.join(APPENDS_FILE), &mailbox)?;
    Ok(TakeInRun::Closed)
}

/// Called by the supervisor at a turn boundary.
///
/// Atomically under `reply.lock`:
/// - If there are undelivered `in_run` items (and the run was not stopped), marks them
///   delivered for `next_round` and returns `TakeInRun::Messages(items)`.
/// - Otherwise marks `closed = true` and returns `TakeInRun::Closed` so the caller can
///   close stdin / proceed to accept/review while any later arrivals fall back to `_reply-pending`.
pub fn take_or_close_in_run(run: &Path, next_round: usize) -> Res<TakeInRun> {
    let _guard = crate::locks::acquire(
        crate::locks::LockKind::ReplyMailbox,
        &run.join(LOCK_FILE),
        true,
    )
    .map_err(|e| e.to_string())?;
    let mut mailbox = load_mailbox(run);
    let now = iso();
    if run.join(".stopped").exists() {
        if let Some(arr) = mailbox["items"].as_array_mut() {
            for item in arr.iter_mut() {
                if item["deliveredAt"].is_null() && item["cancelledAt"].is_null() {
                    item["cancelledAt"] = json!(&now);
                }
            }
        }
        mailbox["closed"] = json!(true);
        if mailbox["closedAt"].is_null() {
            mailbox["closedAt"] = json!(&now);
        }
        write_json(run.join(APPENDS_FILE), &mailbox)?;
        return Ok(TakeInRun::Closed);
    }
    let mut taken = Vec::new();
    if let Some(arr) = mailbox["items"].as_array_mut() {
        for item in arr.iter_mut() {
            if s(item, "delivery") == "in_run"
                && item["deliveredAt"].is_null()
                && item["cancelledAt"].is_null()
            {
                item["deliveredAt"] = json!(&now);
                item["deliveredRound"] = json!(next_round);
                taken.push(item_from_value(item));
            }
        }
    }
    if !taken.is_empty() {
        write_json(run.join(APPENDS_FILE), &mailbox)?;
        return Ok(TakeInRun::Messages(taken));
    }
    mailbox["closed"] = json!(true);
    if mailbox["closedAt"].is_null() {
        mailbox["closedAt"] = json!(&now);
    }
    write_json(run.join(APPENDS_FILE), &mailbox)?;
    Ok(TakeInRun::Closed)
}

/// Ensure the in-run append channel is marked closed before entering accept/review/settlement.
/// Any still-undelivered `in_run` items (e.g. if the agent failed or timed out before a turn
/// boundary) are cancelled when `.stopped` is present, or promoted to post-settle fallback so
/// no appended message is ever silently lost.
pub fn close_in_run(run: &Path) -> Res<()> {
    let _guard = crate::locks::acquire(
        crate::locks::LockKind::ReplyMailbox,
        &run.join(LOCK_FILE),
        true,
    )
    .map_err(|e| e.to_string())?;
    let mut mailbox = load_mailbox(run);
    let now = iso();
    let stopped = run.join(".stopped").exists();
    let mut promoted = false;
    let mut changed = !b(&mailbox, "closed");
    if let Some(arr) = mailbox["items"].as_array_mut() {
        for item in arr.iter_mut() {
            if s(item, "delivery") == "in_run"
                && item["deliveredAt"].is_null()
                && item["cancelledAt"].is_null()
            {
                changed = true;
                if stopped {
                    item["cancelledAt"] = json!(&now);
                } else {
                    item["delivery"] = json!("fallback");
                    promoted = true;
                }
            }
        }
    }
    mailbox["closed"] = json!(true);
    if mailbox["closedAt"].is_null() {
        mailbox["closedAt"] = json!(&now);
    }
    if changed || run.join(APPENDS_FILE).is_file() {
        write_json(run.join(APPENDS_FILE), &mailbox)?;
    }
    if promoted {
        let had_active_pending = status(run)
            .as_ref()
            .is_some_and(|v| s(v, "error").is_empty());
        let _ = write_fallback_file_and_spawn(run, &mailbox, had_active_pending);
    }
    Ok(())
}

/// Revert a batch taken for in-run delivery back to post-settle fallback if injecting into the
/// live process fails (e.g., broken pipe on `agy` stdin or missing Pi session file).
pub fn revert_to_fallback(run: &Path, items: &[AppendItem]) -> Res<()> {
    let seqs: HashSet<u64> = items.iter().map(|it| it.seq).collect();
    if seqs.is_empty() {
        return close_in_run(run);
    }
    let _guard = crate::locks::acquire(
        crate::locks::LockKind::ReplyMailbox,
        &run.join(LOCK_FILE),
        true,
    )
    .map_err(|e| e.to_string())?;
    let mut mailbox = load_mailbox(run);
    let now = iso();
    let stopped = run.join(".stopped").exists();
    if let Some(arr) = mailbox["items"].as_array_mut() {
        for item in arr.iter_mut() {
            let seq = item["seq"].as_u64().unwrap_or(0);
            if seqs.contains(&seq) {
                if stopped {
                    item["deliveredAt"] = Value::Null;
                    item["deliveredRound"] = Value::Null;
                    item["cancelledAt"] = json!(&now);
                } else {
                    item["deliveredAt"] = Value::Null;
                    item["deliveredRound"] = Value::Null;
                    item["delivery"] = json!("fallback");
                }
            }
        }
    }
    mailbox["closed"] = json!(true);
    if mailbox["closedAt"].is_null() {
        mailbox["closedAt"] = json!(&now);
    }
    write_json(run.join(APPENDS_FILE), &mailbox)?;
    if !stopped {
        let had_active_pending = status(run)
            .as_ref()
            .is_some_and(|v| s(v, "error").is_empty());
        write_fallback_file_and_spawn(run, &mailbox, had_active_pending)?;
    }
    Ok(())
}

/// Cancel all undelivered appended messages (both in-run and post-settle fallback).
pub fn cancel(run: &Path) -> Res<()> {
    {
        let _guard = crate::locks::acquire(
            crate::locks::LockKind::ReplyMailbox,
            &run.join(LOCK_FILE),
            true,
        )
        .map_err(|e| e.to_string())?;
        touch(run.join(".stopped"));
        let _ = fs::remove_file(run.join(PENDING_FILE));
        if run.join(APPENDS_FILE).is_file() {
            let mut mailbox = load_mailbox(run);
            let now = iso();
            if let Some(arr) = mailbox["items"].as_array_mut() {
                for item in arr.iter_mut() {
                    if item["deliveredAt"].is_null() && item["cancelledAt"].is_null() {
                        item["cancelledAt"] = json!(&now);
                    }
                }
            }
            mailbox["closed"] = json!(true);
            if mailbox["closedAt"].is_null() {
                mailbox["closedAt"] = json!(&now);
            }
            if let Some(obj) = mailbox.as_object_mut() {
                obj.remove("fallbackError");
            }
            write_json(run.join(APPENDS_FILE), &mailbox)?;
        }
    }
    crate::index::upsert_run(run);
    crate::board::refresh_for_run(run);
    Ok(())
}

/// Lock-free summary for `status`, `card`, `snapshot`, and TUI.
/// Returns `None` when no non-cancelled appends exist and no legacy `pending-reply.json` exists.
pub fn summary_json(run: &Path) -> Option<Value> {
    let mailbox = json(run.join(APPENDS_FILE));
    let pending_file = status(run);
    let mut total: usize = 0;
    let mut delivered: usize = 0;
    let mut pending: usize = 0;
    let mut fallback_pending: usize = 0;
    let mut items_out: Vec<Value> = Vec::new();
    let mut closed = false;
    let mut err_str = String::new();

    if mailbox.is_object() && mailbox["items"].is_array() {
        closed = b(&mailbox, "closed");
        if !s(&mailbox, "fallbackError").is_empty() {
            err_str = s(&mailbox, "fallbackError").to_string();
        }
        for item in mailbox["items"].as_array().into_iter().flatten() {
            if !item["cancelledAt"].is_null() {
                continue;
            }
            total += 1;
            let is_delivered = !item["deliveredAt"].is_null();
            if is_delivered {
                delivered += 1;
            } else {
                pending += 1;
                if s(item, "delivery") == "fallback" {
                    fallback_pending += 1;
                }
            }
            items_out.push(json!({
                "seq": item["seq"],
                "queuedAt": item["queuedAt"],
                "deliveredAt": item["deliveredAt"],
                "deliveredRound": item["deliveredRound"],
                "delivery": item["delivery"],
            }));
        }
    }
    if let Some(pf) = &pending_file {
        let pe = s(pf, "error");
        if !pe.is_empty() {
            err_str = pe.to_string();
        }
        if total == 0 {
            let count = pf["count"].as_u64().unwrap_or(1).max(1) as usize;
            total = count;
            pending = count;
            fallback_pending = count;
        }
    }
    if total == 0 && err_str.is_empty() {
        return None;
    }
    let mut out = json!({
        "total": total,
        "delivered": delivered,
        "pending": pending,
        "fallbackPending": fallback_pending,
        "closed": closed,
    });
    if !items_out.is_empty() {
        out["items"] = json!(items_out);
    }
    if !err_str.is_empty() {
        out["error"] = json!(err_str);
    }
    Some(out)
}

/// Lock-free set of intra-run round numbers produced by in-run append delivery.
pub fn append_delivered_rounds(run: &Path) -> HashSet<usize> {
    let mailbox = json(run.join(APPENDS_FILE));
    let mut rounds = HashSet::new();
    for item in mailbox["items"].as_array().into_iter().flatten() {
        if s(item, "delivery") == "in_run" && item["cancelledAt"].is_null() {
            if let Some(r) = item["deliveredRound"].as_u64() {
                if r >= 2 {
                    rounds.insert(r as usize);
                }
            }
        }
    }
    rounds
}

fn merge_fallback_options(saved: &Value) -> Res<launch::Options> {
    let arg_lists: Vec<Vec<String>> = saved["allArgs"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(strings)
                .filter(|v| !v.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![strings(&saved["args"])]);
    let mut merged = launch::parse_launch(&arg_lists[0], true, true)?;
    for extra in &arg_lists[1..] {
        let next = launch::parse_launch(extra, true, true)?;
        merged.fresh |= next.fresh;
        merged.no_sync |= next.no_sync;
        merged.sync |= next.sync;
        merged.sync_agent |= next.sync_agent;
        merged.hide_accept |= next.hide_accept;
        merged.minor |= next.minor;
        if next.model.is_some() {
            merged.model = next.model;
        }
        if next.tier.is_some() {
            merged.tier = next.tier;
        }
        if next.thinking_cli.is_some() {
            merged.thinking_cli = next.thinking_cli;
        }
        if next.thinking.is_some() {
            merged.thinking = next.thinking;
        }
        if next.overflow.is_some() {
            merged.overflow = next.overflow;
        }
        if next.name.is_some() {
            merged.name = next.name;
        }
        if next.accept_set {
            merged.accept = next.accept;
            merged.accept_set = true;
        }
        if next.accept_timeout.is_some() {
            merged.accept_timeout = next.accept_timeout;
        }
        if next.evidence_set {
            merged.evidence = next.evidence;
            merged.evidence_set = true;
        }
        if next.evidence_timeout.is_some() {
            merged.evidence_timeout = next.evidence_timeout;
        }
        if next.timeout.is_some() {
            merged.timeout = next.timeout;
        }
        if next.max_answer.is_some() {
            merged.max_answer = next.max_answer;
        }
        if next.over_limit.is_some() {
            merged.over_limit = next.over_limit;
        }
        if next.work.is_some() {
            merged.work = next.work;
        }
        for img in next.images {
            if !merged.images.contains(&img) {
                merged.images.push(img);
            }
        }
        for path in next.allow {
            if !merged.allow.contains(&path) {
                merged.allow.push(path);
            }
        }
        for cmd in next.accept_also {
            if !merged.accept_also.contains(&cmd) {
                merged.accept_also.push(cmd);
            }
        }
    }
    for img in strings(&saved["images"]) {
        if !img.is_empty() && !merged.images.contains(&img) {
            merged.images.push(img);
        }
    }
    Ok(merged)
}

/// Detached worker (`_reply-pending <run>`) delivering post-settle fallback messages once the
/// parent run finishes.
pub fn deliver(run: &Path) -> Res<()> {
    loop {
        if status(run).is_none() || run.join(".stopped").exists() {
            return Ok(());
        }
        if !runs::active(&runs::state(run)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _guard = crate::locks::acquire(
        crate::locks::LockKind::ReplyMailbox,
        &run.join(LOCK_FILE),
        true,
    )
    .map_err(|e| e.to_string())?;
    let Some(saved) = status(run) else {
        return Ok(());
    };
    if run.join(".stopped").exists() || runs::state(run) == "stopped" {
        let _ = fs::remove_file(run.join(PENDING_FILE));
        if run.join(APPENDS_FILE).is_file() {
            let mut mailbox = load_mailbox(run);
            let now = iso();
            if let Some(arr) = mailbox["items"].as_array_mut() {
                for item in arr.iter_mut() {
                    if item["deliveredAt"].is_null() && item["cancelledAt"].is_null() {
                        item["cancelledAt"] = json!(&now);
                    }
                }
            }
            let _ = write_json(run.join(APPENDS_FILE), &mailbox);
        }
        return Ok(());
    }
    let mut options = merge_fallback_options(&saved)?;
    options.when_done = false;
    options.run = Some(run.to_string_lossy().into_owned());
    options.prompt_file = None;
    options.prompt = Some(s(&saved, "message").to_string());
    options.words.clear();
    std::env::set_current_dir(s(&saved, "cwd")).map_err(|e| e.to_string())?;
    // Leave a durable diagnostic instead of silently losing a failed launch.
    match launch::reply(options) {
        Ok(child) => {
            let now = iso();
            let mut seqs: Vec<u64> = Vec::new();
            if run.join(APPENDS_FILE).is_file() {
                let mut mailbox = load_mailbox(run);
                if let Some(arr) = mailbox["items"].as_array_mut() {
                    for item in arr.iter_mut() {
                        if s(item, "delivery") == "fallback"
                            && item["deliveredAt"].is_null()
                            && item["cancelledAt"].is_null()
                        {
                            item["deliveredAt"] = json!(&now);
                            item["childRun"] = json!(child.to_string_lossy());
                            if let Some(seq) = item["seq"].as_u64() {
                                seqs.push(seq);
                            }
                        }
                    }
                }
                if let Some(obj) = mailbox.as_object_mut() {
                    obj.remove("fallbackError");
                }
                let _ = write_json(run.join(APPENDS_FILE), &mailbox);
            }
            write_json(
                run.join("reply-delivered.json"),
                &json!({"run": child, "at": now, "seqs": seqs}),
            )?;
            fs::remove_file(run.join(PENDING_FILE)).map_err(|e| e.to_string())?;
            crate::index::upsert_run(run);
            crate::board::refresh_for_run(run);
        }
        Err(error) => {
            let mut failed = saved;
            failed["error"] = json!(&error);
            write_json(run.join(PENDING_FILE), &failed)?;
            if run.join(APPENDS_FILE).is_file() {
                let mut mailbox = load_mailbox(run);
                mailbox["fallbackError"] = json!(&error);
                let _ = write_json(run.join(APPENDS_FILE), &mailbox);
            }
            crate::index::upsert_run(run);
            crate::board::refresh_for_run(run);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_run(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "delegate-append-test-{tag}-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&dir).unwrap();
        write_json(dir.join("meta.json"), &json!({"name": "demo"})).unwrap();
        dir
    }

    #[test]
    fn merged_followup_includes_header_and_numbered_items_in_order() {
        let items = vec![
            AppendItem {
                seq: 1,
                message: "补测空输入\n".into(),
                images: vec![],
                args: vec![],
                cwd: "/tmp".into(),
                queued_at: "t1".into(),
            },
            AppendItem {
                seq: 2,
                message: "再检查错误码".into(),
                images: vec![json!("/tmp/a.png")],
                args: vec![],
                cwd: "/tmp".into(),
                queued_at: "t2".into(),
            },
        ];
        let text = format_merged_followup(&items);
        assert!(text.starts_with(APPEND_HEADER));
        assert!(text.contains("\n\n1. 补测空输入\n\n2. 再检查错误码"));
        assert_eq!(merged_images(&items), vec![json!("/tmp/a.png")]);
    }

    #[test]
    fn take_or_close_merges_pending_items_once_then_closes() {
        let run = temp_run("merge");
        write_json(
            run.join(APPENDS_FILE),
            &json!({
                "closed": false,
                "closedAt": Value::Null,
                "nextSeq": 3,
                "items": [
                    {"seq": 1, "message": "q1", "images": [], "args": [], "cwd": "/tmp", "queuedAt": "t1", "deliveredAt": Value::Null, "deliveredRound": Value::Null, "delivery": "in_run", "cancelledAt": Value::Null},
                    {"seq": 2, "message": "q2", "images": [], "args": [], "cwd": "/tmp", "queuedAt": "t2", "deliveredAt": Value::Null, "deliveredRound": Value::Null, "delivery": "in_run", "cancelledAt": Value::Null}
                ]
            }),
        )
        .unwrap();

        let before = summary_json(&run).unwrap();
        assert_eq!(n(&before, "total"), 2);
        assert_eq!(n(&before, "pending"), 2);
        assert_eq!(n(&before, "delivered"), 0);

        match take_or_close_in_run(&run, 2).unwrap() {
            TakeInRun::Messages(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].seq, 1);
                assert_eq!(items[1].seq, 2);
            }
            TakeInRun::Closed => panic!("expected messages"),
        }

        let after_take = summary_json(&run).unwrap();
        assert_eq!(n(&after_take, "total"), 2);
        assert_eq!(n(&after_take, "delivered"), 2);
        assert_eq!(n(&after_take, "pending"), 0);
        assert!(!b(&after_take, "closed"));
        assert!(append_delivered_rounds(&run).contains(&2));

        // Second boundary with no new messages closes the channel and never double-delivers.
        assert!(matches!(
            take_or_close_in_run(&run, 3).unwrap(),
            TakeInRun::Closed
        ));
        let after_close = summary_json(&run).unwrap();
        assert!(b(&after_close, "closed"));
        assert_eq!(n(&after_close, "delivered"), 2);

        let _ = fs::remove_dir_all(&run);
    }

    #[test]
    fn cancel_marks_pending_cancelled_and_keeps_delivered() {
        let run = temp_run("cancel");
        write_json(
            run.join(APPENDS_FILE),
            &json!({
                "closed": false,
                "closedAt": Value::Null,
                "nextSeq": 3,
                "items": [
                    {"seq": 1, "message": "q1", "images": [], "args": [], "cwd": "/tmp", "queuedAt": "t1", "deliveredAt": "t2", "deliveredRound": 2, "delivery": "in_run", "cancelledAt": Value::Null},
                    {"seq": 2, "message": "q2", "images": [], "args": [], "cwd": "/tmp", "queuedAt": "t3", "deliveredAt": Value::Null, "deliveredRound": Value::Null, "delivery": "in_run", "cancelledAt": Value::Null}
                ]
            }),
        )
        .unwrap();
        write_json(run.join(PENDING_FILE), &json!({"message": "q2"})).unwrap();

        cancel(&run).unwrap();
        assert!(status(&run).is_none());
        let sum = summary_json(&run).unwrap();
        assert_eq!(n(&sum, "total"), 1);
        assert_eq!(n(&sum, "delivered"), 1);
        assert_eq!(n(&sum, "pending"), 0);
        assert!(b(&sum, "closed"));

        let _ = fs::remove_dir_all(&run);
    }

    #[test]
    fn steer_take_and_revert_preserve_order_and_never_drop() {
        let run = temp_run("steer");
        write_json(
            run.join(APPENDS_FILE),
            &json!({
                "closed": false,
                "closedAt": Value::Null,
                "nextSeq": 3,
                "items": [
                    {"seq": 1, "message": "s1", "images": [], "args": [], "cwd": "/tmp", "queuedAt": "t1", "deliveredAt": Value::Null, "deliveredRound": Value::Null, "delivery": "in_run", "cancelledAt": Value::Null},
                    {"seq": 2, "message": "s2", "images": [], "args": [], "cwd": "/tmp", "queuedAt": "t2", "deliveredAt": Value::Null, "deliveredRound": Value::Null, "delivery": "in_run", "cancelledAt": Value::Null}
                ]
            }),
        )
        .unwrap();

        let taken = take_pending_in_run_for_steer(&run).unwrap();
        assert_eq!(taken.len(), 2);
        let after = summary_json(&run).unwrap();
        assert_eq!(n(&after, "delivered"), 2);
        assert_eq!(n(&after, "pending"), 0);
        assert!(!b(&after, "closed"));

        // Revert puts them back to pending in_run when channel is still open.
        revert_steer_to_in_run(&run, &taken).unwrap();
        let reverted = summary_json(&run).unwrap();
        assert_eq!(n(&reverted, "delivered"), 0);
        assert_eq!(n(&reverted, "pending"), 2);

        let _ = fs::remove_dir_all(&run);
    }

    #[test]
    fn merge_fallback_options_preserves_fresh_model_and_images_from_earlier_items() {
        let saved = json!({
            "args": ["r1", "second question"],
            "allArgs": [
                ["--fresh", "--model", "neon", "--image", "/tmp/a.png", "--allow", "tests/", "r1", "first question"],
                ["--image", "/tmp/b.png", "r1", "second question"]
            ],
            "images": ["/tmp/a.png", "/tmp/b.png"]
        });
        let merged = merge_fallback_options(&saved).unwrap();
        assert!(merged.fresh);
        assert_eq!(merged.model.as_deref(), Some("neon"));
        assert_eq!(merged.images, vec!["/tmp/a.png", "/tmp/b.png"]);
        assert_eq!(merged.allow, vec!["tests/"]);
    }
}
