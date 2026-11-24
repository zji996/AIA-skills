//! Machine-wide unified work table (`<state_dir>/work.jsonl`), 7-state machine,
//! structured `wait_on` evaluation (including `run_applied` and read-only `owner:Qxx`
//! scanning), progressive-disclosure 6-column table & card views, timely command-tail
//! hints, automatic one-time migration of `notes.json` / `deferred.tsv`, and
//! `delegate work import-md --dry-run`.
use crate::common::*;
use crate::{dag_queue, events, notes, runs};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const STATES: [&str; 7] = [
    "attention",
    "ready",
    "verify",
    "active",
    "blocked",
    "deferred",
    "done",
];

pub const OWNER_KINDS: [&str; 4] = ["act", "nod", "card", "direction"];
pub const NEEDS_OWNER_KINDS: [&str; 3] = ["nod", "act", "card"];
pub const WORK_CLASSES: [&str; 4] = ["feat", "unblock", "infra", "docs"];
pub const DEFAULT_STALE_VERIFY_HOURS: f64 = 4.0;
pub const DEFAULT_STALE_READY_HOURS: f64 = 8.0;
pub const DEFAULT_REMIND_COOLDOWN_HOURS: f64 = 6.0;

pub fn work_class_priority(cls: &str) -> u8 {
    match cls {
        "feat" => 0,
        "unblock" => 1,
        "infra" => 2,
        "docs" => 3,
        _ => 9,
    }
}

pub fn validate_work_class(raw: &str) -> Res<String> {
    let k = raw.trim();
    if WORK_CLASSES.contains(&k) {
        Ok(k.to_string())
    } else {
        Err(format!(
            "invalid --class '{k}' (expected: {})",
            WORK_CLASSES.join("|")
        ))
    }
}

fn is_infra_or_docs(cls: &str) -> bool {
    matches!(cls, "infra" | "docs")
}

fn is_readonly_start_args(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--read-only" {
            return true;
        }
        if let Some(val) = a.strip_prefix("--tier=") {
            if matches!(val, "scout" | "review") {
                return true;
            }
        } else if a == "--tier" {
            if let Some(val) = args.get(i + 1) {
                if matches!(val.as_str(), "scout" | "review") {
                    return true;
                }
            }
            i += 1;
        }
        i += 1;
    }
    false
}
const OWNER_WAIT_KEYWORDS: [&str; 8] = [
    "点头", "确认", "真机", "对外", "发布", "删除", "不可逆", "所有者",
];
const OWNER_WAIT_WARNING: &str =
    "这项像要所有者点头或等前置，仍放进就绪；若是，加 --needs-owner nod 或 --after-applied / --after-time";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnerInfo {
    pub kind: String,
    pub ask: String,
    pub recommendation: String,
    pub if_undecided: String,
    pub cost: String,
}

impl OwnerInfo {
    pub fn kind_label(kind: &str) -> &'static str {
        match kind {
            "act" => "动手",
            "nod" => "点头",
            "card" => "看卡",
            "direction" => "方向",
            _ => "其他",
        }
    }

    pub fn kind_priority(kind: &str) -> u8 {
        match kind {
            "act" => 0,
            "nod" => 1,
            "card" => 2,
            "direction" => 3,
            _ => 4,
        }
    }

    fn from_value(v: &Value) -> Option<Self> {
        if !v.is_object() {
            return None;
        }
        let kind = s(v, "kind").trim().to_string();
        if !OWNER_KINDS.contains(&kind.as_str()) {
            return None;
        }
        Some(Self {
            kind,
            ask: s(v, "ask").to_string(),
            recommendation: s(v, "recommendation").to_string(),
            if_undecided: s(v, "ifUndecided").to_string(),
            cost: s(v, "cost").to_string(),
        })
    }

    fn to_json(&self) -> Value {
        let mut obj = json!({
            "kind": self.kind,
            "ask": self.ask,
        });
        if !self.recommendation.is_empty() {
            obj["recommendation"] = json!(self.recommendation);
        }
        if !self.if_undecided.is_empty() {
            obj["ifUndecided"] = json!(self.if_undecided);
        }
        if !self.cost.is_empty() {
            obj["cost"] = json!(self.cost);
        }
        obj
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FollowupItem {
    pub id: usize,
    pub kind: String,
    pub text: String,
    pub anchor: String,
    pub source_run: String,
    pub done: bool,
    pub created_at: String,
    pub done_at: Option<String>,
}

impl FollowupItem {
    fn from_value(v: &Value, fallback_id: usize) -> Option<Self> {
        let text = s(v, "text").trim().to_string();
        if text.is_empty() {
            return None;
        }
        let id = v["id"].as_u64().map(|x| x as usize).unwrap_or(fallback_id);
        let kind = {
            let k = s(v, "kind");
            if k.is_empty() {
                "note".to_string()
            } else {
                k.to_string()
            }
        };
        Some(Self {
            id,
            kind,
            text,
            anchor: s(v, "anchor").to_string(),
            source_run: s(v, "sourceRun").to_string(),
            done: b(v, "done"),
            created_at: s(v, "createdAt").to_string(),
            done_at: v["doneAt"]
                .as_str()
                .filter(|x| !x.is_empty())
                .map(str::to_string),
        })
    }

    fn to_json(&self) -> Value {
        let mut obj = json!({
            "id": self.id,
            "kind": self.kind,
            "text": self.text,
            "done": self.done,
            "createdAt": self.created_at,
        });
        if !self.anchor.is_empty() {
            obj["anchor"] = json!(self.anchor);
        }
        if !self.source_run.is_empty() {
            obj["sourceRun"] = json!(self.source_run);
        }
        if let Some(done_at) = &self.done_at {
            obj["doneAt"] = json!(done_at);
        }
        obj
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactItem {
    pub path: String,
    pub source_run: String,
    pub added_at: String,
    pub note: String,
}

impl ArtifactItem {
    fn from_value(v: &Value) -> Option<Self> {
        if let Some(path) = v.as_str() {
            let trimmed = path.trim();
            if trimmed.is_empty() {
                return None;
            }
            return Some(Self {
                path: trimmed.to_string(),
                source_run: String::new(),
                added_at: String::new(),
                note: String::new(),
            });
        }
        let path = s(v, "path").trim().to_string();
        if path.is_empty() {
            return None;
        }
        Some(Self {
            path,
            source_run: s(v, "sourceRun").to_string(),
            added_at: s(v, "addedAt").to_string(),
            note: s(v, "note").to_string(),
        })
    }

    fn to_json(&self) -> Value {
        let mut obj = json!({ "path": self.path });
        if !self.source_run.is_empty() {
            obj["sourceRun"] = json!(self.source_run);
        }
        if !self.added_at.is_empty() {
            obj["addedAt"] = json!(self.added_at);
        }
        if !self.note.is_empty() {
            obj["note"] = json!(self.note);
        }
        obj
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoDispatch {
    pub prompt_file: String,
    pub start_args: Vec<String>,
    pub cwd: String,
    pub repo: String,
    pub runs_dir: String,
    pub caller: String,
    pub dispatched_run: String,
}

impl AutoDispatch {
    fn from_value(v: &Value) -> Option<Self> {
        if !v.is_object() {
            return None;
        }
        let prompt_file = s(v, "promptFile").to_string();
        if prompt_file.is_empty() {
            return None;
        }
        Some(Self {
            prompt_file,
            start_args: strings(&v["startArgs"]),
            cwd: s(v, "cwd").to_string(),
            repo: s(v, "repo").to_string(),
            runs_dir: s(v, "runsDir").to_string(),
            caller: s(v, "caller").to_string(),
            dispatched_run: s(v, "dispatchedRun").to_string(),
        })
    }

    fn to_json(&self) -> Value {
        let mut obj = json!({
            "promptFile": self.prompt_file,
            "startArgs": self.start_args,
            "cwd": self.cwd,
            "repo": self.repo,
        });
        if !self.runs_dir.is_empty() {
            obj["runsDir"] = json!(self.runs_dir);
        }
        if !self.caller.is_empty() {
            obj["caller"] = json!(self.caller);
        }
        if !self.dispatched_run.is_empty() {
            obj["dispatchedRun"] = json!(self.dispatched_run);
        }
        obj
    }
}

#[derive(Clone, Debug, Default)]
pub struct WorkItem {
    pub id: String,
    pub title: String,
    pub repo: String,
    pub repos: Vec<String>,
    pub state: String,
    pub class: String,
    pub files: Vec<String>,
    pub closed_kind: String,
    pub done_when: String,
    pub needs_verify: bool,
    pub wait_on: Vec<String>,
    pub verify_on: Vec<String>,
    pub auto_dispatch: Option<AutoDispatch>,
    pub runs: Vec<String>,
    pub run_dirs: BTreeMap<String, String>,
    pub artifacts: Vec<ArtifactItem>,
    pub brief: String,
    pub followups: Vec<FollowupItem>,
    pub owner: Option<OwnerInfo>,
    pub owner_ref: String,
    pub defer_reason: String,
    pub defer_until: String,
    pub defer_streak: u32,
    pub created_at: String,
    pub updated_at: String,
    pub state_since: String,
    pub last_reminded_at: String,
    pub close_reason: String,
    pub close_evidence: String,
    pub origin: String,
    pub caller: String,
    pub note_root: String,
    pub applied: bool,
    pub cleaned: bool,
}

impl WorkItem {
    pub fn pending_followups(&self) -> usize {
        self.followups.iter().filter(|f| !f.done).count()
    }

    pub fn has_open_closure(&self) -> bool {
        self.needs_verify
            || !self.verify_on.is_empty()
            || !self.artifacts.is_empty()
            || self.pending_followups() > 0
    }

    /// True when an open non-direction owner item lacks any of the four required fields
    /// (`ask`, `recommendation`, `if_undecided`, `cost`).
    pub fn is_missing_owner_fields(&self) -> bool {
        let Some(ow) = &self.owner else {
            return false;
        };
        if self.state == "done" || ow.kind == "direction" {
            return false;
        }
        ow.ask.trim().is_empty()
            || ow.recommendation.trim().is_empty()
            || ow.if_undecided.trim().is_empty()
            || ow.cost.trim().is_empty()
    }

    /// True when an open non-owner work item lacks a valid value class (`feat|unblock|infra|docs`).
    pub fn is_missing_class(&self) -> bool {
        self.owner.is_none()
            && self.state != "done"
            && !WORK_CLASSES.contains(&self.class.as_str())
    }

    pub fn repo_label(&self) -> String {
        let base = short_repo_name(&self.repo);
        let extra = self
            .repos
            .iter()
            .filter(|r| !r.is_empty() && r.as_str() != self.repo.as_str())
            .count();
        if extra > 0 {
            format!("{base}+{extra}")
        } else {
            base.to_string()
        }
    }

    pub fn latest_run_id(&self) -> Option<&str> {
        self.runs.last().map(String::as_str)
    }

    pub fn matches_run(&self, run_id: &str) -> bool {
        if run_id.is_empty() {
            return false;
        }
        self.runs.iter().any(|r| r == run_id) || self.note_root == run_id
    }

    pub fn add_run(&mut self, run_id: &str, run_dir: Option<&Path>, repo_path: Option<&str>) {
        if !run_id.is_empty() && !self.runs.iter().any(|r| r == run_id) {
            self.runs.push(run_id.to_string());
        }
        if let Some(dir) = run_dir {
            let canon = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
            self.run_dirs
                .insert(run_id.to_string(), canon.to_string_lossy().into_owned());
        }
        if let Some(rp) = repo_path.filter(|p| !p.is_empty()) {
            if self.repo.is_empty() {
                self.repo = rp.to_string();
            } else if self.repo != rp && !self.repos.iter().any(|x| x == rp) {
                self.repos.push(rp.to_string());
            }
        }
    }

    pub fn set_state(&mut self, new_state: &str, now_iso: &str) -> bool {
        if self.state != new_state {
            self.state = new_state.to_string();
            self.state_since = now_iso.to_string();
            self.updated_at = now_iso.to_string();
            true
        } else {
            false
        }
    }

    fn from_value(v: &Value) -> Option<Self> {
        let id = s(v, "id").trim().to_string();
        if id.is_empty() {
            return None;
        }
        let state = {
            let st = s(v, "state");
            if STATES.contains(&st) {
                st.to_string()
            } else {
                "ready".to_string()
            }
        };
        let mut run_dirs = BTreeMap::new();
        if let Some(map) = v["runDirs"].as_object() {
            for (k, val) in map {
                if let Some(p) = val.as_str().filter(|x| !x.is_empty()) {
                    run_dirs.insert(k.clone(), p.to_string());
                }
            }
        }
        let artifacts = v["artifacts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(ArtifactItem::from_value)
            .collect();
        let followups = v["followups"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .filter_map(|(i, item)| FollowupItem::from_value(item, i + 1))
            .collect();
        let created_at = s(v, "createdAt").to_string();
        let state_since = {
            let ss = s(v, "stateSince");
            if ss.is_empty() {
                created_at.clone()
            } else {
                ss.to_string()
            }
        };
        Some(Self {
            id,
            title: s(v, "title").to_string(),
            repo: s(v, "repo").to_string(),
            repos: strings(&v["repos"]),
            state,
            class: s(v, "class").trim().to_string(),
            files: strings(&v["files"]),
            closed_kind: s(v, "closedKind").to_string(),
            done_when: s(v, "doneWhen").to_string(),
            needs_verify: b(v, "needsVerify"),
            wait_on: strings(&v["waitOn"]),
            verify_on: strings(&v["verifyOn"]),
            auto_dispatch: AutoDispatch::from_value(&v["autoDispatch"]),
            runs: strings(&v["runs"]),
            run_dirs,
            artifacts,
            brief: s(v, "brief").to_string(),
            followups,
            owner: OwnerInfo::from_value(&v["owner"]),
            owner_ref: s(v, "ownerRef").to_string(),
            defer_reason: s(v, "deferReason").to_string(),
            defer_until: s(v, "deferUntil").to_string(),
            defer_streak: v["deferStreak"].as_u64().unwrap_or(0) as u32,
            created_at,
            updated_at: s(v, "updatedAt").to_string(),
            state_since,
            last_reminded_at: s(v, "lastRemindedAt").to_string(),
            close_reason: s(v, "closeReason").to_string(),
            close_evidence: s(v, "closeEvidence").to_string(),
            origin: s(v, "origin").to_string(),
            caller: s(v, "caller").trim().to_string(),
            note_root: s(v, "noteRoot").to_string(),
            applied: b(v, "applied"),
            cleaned: b(v, "cleaned"),
        })
    }

    pub fn to_json(&self) -> Value {
        let mut obj = json!({
            "id": self.id,
            "title": self.title,
            "repo": self.repo,
            "state": self.state,
            "waitOn": self.wait_on,
            "runs": self.runs,
            "artifacts": self.artifacts.iter().map(ArtifactItem::to_json).collect::<Vec<_>>(),
            "followups": self.followups.iter().map(FollowupItem::to_json).collect::<Vec<_>>(),
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
            "stateSince": self.state_since,
        });
        if !self.repos.is_empty() {
            obj["repos"] = json!(self.repos);
        }
        if !self.class.is_empty() {
            obj["class"] = json!(self.class);
        }
        if !self.files.is_empty() {
            obj["files"] = json!(self.files);
        }
        if !self.closed_kind.is_empty() {
            obj["closedKind"] = json!(self.closed_kind);
        }
        if !self.done_when.is_empty() {
            obj["doneWhen"] = json!(self.done_when);
        }
        if self.needs_verify {
            obj["needsVerify"] = json!(true);
        }
        if !self.verify_on.is_empty() {
            obj["verifyOn"] = json!(self.verify_on);
        }
        if let Some(ad) = &self.auto_dispatch {
            obj["autoDispatch"] = ad.to_json();
        }
        if !self.run_dirs.is_empty() {
            obj["runDirs"] = json!(self.run_dirs);
        }
        if !self.brief.is_empty() {
            obj["brief"] = json!(self.brief);
        }
        if let Some(ow) = &self.owner {
            obj["owner"] = ow.to_json();
        }
        if !self.owner_ref.is_empty() {
            obj["ownerRef"] = json!(self.owner_ref);
        }
        if !self.defer_reason.is_empty() {
            obj["deferReason"] = json!(self.defer_reason);
        }
        if !self.defer_until.is_empty() {
            obj["deferUntil"] = json!(self.defer_until);
        }
        if self.defer_streak > 0 {
            obj["deferStreak"] = json!(self.defer_streak);
        }
        if !self.last_reminded_at.is_empty() {
            obj["lastRemindedAt"] = json!(self.last_reminded_at);
        }
        if !self.close_reason.is_empty() {
            obj["closeReason"] = json!(self.close_reason);
        }
        if !self.close_evidence.is_empty() {
            obj["closeEvidence"] = json!(self.close_evidence);
        }
        if !self.origin.is_empty() {
            obj["origin"] = json!(self.origin);
        }
        if !self.caller.is_empty() {
            obj["caller"] = json!(self.caller);
        }
        if !self.note_root.is_empty() {
            obj["noteRoot"] = json!(self.note_root);
        }
        if self.applied {
            obj["applied"] = json!(true);
        }
        if self.cleaned {
            obj["cleaned"] = json!(true);
        }
        obj
    }
}

pub fn work_path(state: &Path) -> PathBuf {
    state.join("work.jsonl")
}

fn work_seq_path(state: &Path) -> PathBuf {
    state.join("work.seq")
}

fn work_migrations_path(state: &Path) -> PathBuf {
    state.join("work-migrations.json")
}

pub fn short_repo_name(repo: &str) -> &str {
    if repo.is_empty() {
        return "-";
    }
    Path::new(repo)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(repo)
}

pub fn current_repo_str() -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Some(top) = git_top(&cwd) {
        return fs::canonicalize(&top)
            .unwrap_or(top)
            .to_string_lossy()
            .into_owned();
    }
    if !cwd.as_os_str().is_empty() {
        return fs::canonicalize(&cwd)
            .unwrap_or(cwd)
            .to_string_lossy()
            .into_owned();
    }
    String::new()
}

fn parse_prefixed_num(id: &str, upper: char, lower: char) -> Option<u64> {
    let rest = id.strip_prefix(upper).or_else(|| id.strip_prefix(lower))?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse::<u64>().ok()
}

fn parse_work_num(id: &str) -> Option<u64> {
    parse_prefixed_num(id, 'W', 'w')
}

pub fn normalize_work_id(raw: &str) -> Option<String> {
    let t = raw.trim();
    if let Some(n) = parse_prefixed_num(t, 'W', 'w') {
        return Some(format!("W{n:03}"));
    }
    if let Some(n) = parse_prefixed_num(t, 'Q', 'q') {
        return Some(format!("Q{n:02}"));
    }
    if let Some(n) = parse_prefixed_num(t, 'D', 'd') {
        return Some(format!("D{n:03}"));
    }
    None
}

fn is_owner_id(id: &str) -> bool {
    let t = id.trim();
    parse_prefixed_num(t, 'Q', 'q').is_some() || parse_prefixed_num(t, 'D', 'd').is_some()
}

fn next_owner_id(items: &[WorkItem], kind: &str) -> String {
    if kind == "direction" {
        let max_d = items
            .iter()
            .filter_map(|it| parse_prefixed_num(&it.id, 'D', 'd'))
            .max()
            .unwrap_or(0);
        format!("D{:03}", max_d.saturating_add(1))
    } else {
        let max_q = items
            .iter()
            .filter_map(|it| parse_prefixed_num(&it.id, 'Q', 'q'))
            .max()
            .unwrap_or(0);
        format!("Q{:02}", max_q.saturating_add(1))
    }
}

fn next_id_impl(state: &Path, items: &[WorkItem], persist: bool) -> String {
    let from_seq = read(work_seq_path(state)).trim().parse::<u64>().unwrap_or(0);
    let from_items = items
        .iter()
        .filter_map(|it| parse_work_num(&it.id))
        .max()
        .unwrap_or(0);
    let next = from_seq.max(from_items).saturating_add(1);
    if persist {
        let _ = write(work_seq_path(state), format!("{next}\n"));
    }
    format!("W{next:03}")
}

fn next_id_locked(state: &Path, items: &[WorkItem]) -> String {
    next_id_impl(state, items, true)
}

fn load_raw_unmigrated(state: &Path) -> Vec<WorkItem> {
    let path = work_path(state);
    let Ok(f) = OpenOptions::new().read(true).open(path) else {
        return Vec::new();
    };
    let reader = BufReader::new(f);
    let mut items = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            if let Some(item) = WorkItem::from_value(&v) {
                items.push(item);
            }
        }
    }
    items
}

struct ManualMigrationRecord {
    id: String,
    repo: String,
    removed: Vec<String>,
    state: String,
}

fn strip_legacy_manual_waits(
    state: &Path,
    items: &mut [WorkItem],
    prefetch: Option<&ReconcilePrefetch>,
) -> Vec<ManualMigrationRecord> {
    let now_epoch = epoch() as i64;
    let now_iso = iso();
    let snapshot: Vec<WorkItem> = items.to_vec();
    let mut records = Vec::new();

    for item in items.iter_mut() {
        let mut removed = Vec::new();
        let mut kept = Vec::new();
        for cond in item.wait_on.drain(..) {
            if cond.starts_with("manual:") {
                removed.push(cond);
            } else {
                kept.push(cond);
            }
        }
        item.wait_on = kept;
        if removed.is_empty() {
            continue;
        }

        let base_fid = item.followups.iter().map(|f| f.id).max().unwrap_or(0);
        for (idx, raw) in removed.iter().enumerate() {
            let note = raw
                .strip_prefix("manual:")
                .unwrap_or(raw)
                .replace(['\n', '\r'], " ");
            let text = format!("原手动前置：{}", note.trim());
            item.followups.push(FollowupItem {
                id: base_fid + idx + 1,
                kind: "note".to_string(),
                text,
                anchor: String::new(),
                source_run: String::new(),
                done: false,
                created_at: now_iso.clone(),
                done_at: None,
            });
        }

        if item.state == "blocked" {
            let all_remaining_ok = item.wait_on.is_empty()
                || item.wait_on.iter().all(|c| {
                    evaluate_condition_with_prefetch(
                        c, item, &snapshot, state, now_epoch, prefetch,
                    )
                    .satisfied
                });
            if all_remaining_ok {
                let target_st = match &item.owner {
                    Some(ow) if ow.kind == "direction" => "active",
                    Some(_) => "attention",
                    None => "ready",
                };
                item.set_state(target_st, &now_iso);
            }
        }
        item.updated_at = now_iso.clone();
        records.push(ManualMigrationRecord {
            id: item.id.clone(),
            repo: item.repo.clone(),
            removed,
            state: item.state.clone(),
        });
    }
    records
}

fn emit_manual_migration_event(state: &Path, records: &[ManualMigrationRecord]) {
    if records.is_empty() {
        return;
    }
    let work_ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
    let first_id = records[0].id.as_str();
    let first_repo = records[0].repo.as_str();
    let details: Vec<Value> = records
        .iter()
        .map(|r| {
            json!({
                "workId": r.id,
                "removed": r.removed,
                "state": r.state,
            })
        })
        .collect();
    let _ = events::emit_in(
        state,
        "",
        first_repo,
        "work_migrated",
        json!({
            "workId": first_id,
            "workIds": work_ids,
            "count": records.len(),
            "reason": "manual_wait_removed",
            "items": details,
        }),
    );
}

fn load_raw(state: &Path) -> Vec<WorkItem> {
    let mut items = load_raw_unmigrated(state);
    let has_manual = items
        .iter()
        .any(|w| w.wait_on.iter().any(|c| c.starts_with("manual:")));
    if !has_manual {
        return items;
    }
    if crate::locks::holding_state_lock() {
        let migrated = strip_legacy_manual_waits(state, &mut items, None);
        if !migrated.is_empty() {
            let _ = save_raw(state, &items);
            emit_manual_migration_event(state, &migrated);
        }
        return items;
    }
    let prefetch = prefetch_reconcile_conditions(state, &items, &[]);
    if let Ok(_guard) = crate::locks::acquire_state(state) {
        let mut locked_items = load_raw_unmigrated(state);
        let migrated = strip_legacy_manual_waits(state, &mut locked_items, Some(&prefetch));
        if !migrated.is_empty() {
            let _ = save_raw(state, &locked_items);
        }
        drop(_guard);
        if !migrated.is_empty() {
            emit_manual_migration_event(state, &migrated);
        }
        return locked_items;
    }
    let _ = strip_legacy_manual_waits(state, &mut items, Some(&prefetch));
    items
}

fn save_raw(state: &Path, items: &[WorkItem]) -> Res<()> {
    fs::create_dir_all(state).map_err(|e| format!("{}: {e}", state.display()))?;
    let mut max_n = read(work_seq_path(state)).trim().parse::<u64>().unwrap_or(0);
    let mut buf = String::new();
    for item in items {
        if let Some(n) = parse_work_num(&item.id) {
            max_n = max_n.max(n);
        }
        buf.push_str(&item.to_json().to_string());
        buf.push('\n');
    }
    atomic_write(work_path(state), buf).map_err(|e| format!("work.jsonl write: {e}"))?;
    let _ = atomic_write(work_seq_path(state), format!("{max_n}\n"));
    Ok(())
}

/// Parse ISO timestamp (`YYYY-MM-DDTHH:MM:SSZ`), date (`YYYY-MM-DD`), or relative duration (`4h`, `7d`).
pub fn parse_time_spec(spec: &str, now_epoch: i64) -> Res<String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err("empty time specification".into());
    }
    if iso_epoch(trimmed).is_some() {
        return Ok(trimmed.to_string());
    }
    // Date-only: YYYY-MM-DD -> YYYY-MM-DDT00:00:00Z
    if trimmed.len() == 10
        && trimmed.as_bytes()[4] == b'-'
        && trimmed.as_bytes()[7] == b'-'
    {
        let full = format!("{trimmed}T00:00:00Z");
        if iso_epoch(&full).is_some() {
            return Ok(full);
        }
    }
    // Relative positive duration: e.g. 4h, 24h, 7d, 30m
    if let Ok(secs) = seconds(trimmed) {
        let target = now_epoch.saturating_add(secs.round() as i64);
        return Ok(format_epoch(target, false, "%Y-%m-%dT%H:%M:%SZ"));
    }
    Err(format!(
        "invalid time '{spec}' (expected duration like 4h/7d, date YYYY-MM-DD, or ISO YYYY-MM-DDTHH:MM:SSZ)"
    ))
}

fn parse_iso_or_date_epoch(value: &str) -> Option<i64> {
    if let Some(ts) = iso_epoch(value) {
        return Some(ts);
    }
    if value.len() == 10 {
        return iso_epoch(&format!("{value}T00:00:00Z"));
    }
    None
}

fn format_age(since_iso: &str, now_epoch: i64) -> String {
    let Some(ts) = parse_iso_or_date_epoch(since_iso) else {
        return String::new();
    };
    let diff = (now_epoch - ts).max(0);
    if diff < 60 {
        format!("{diff}s")
    } else if diff < 3600 {
        format!("{}m", diff / 60)
    } else if diff < 86400 {
        format!("{}h", diff / 3600)
    } else {
        format!("{}d", diff / 86400)
    }
}

fn clip_chars(text: &str, max_chars: usize) -> String {
    let clean = text.replace(['\n', '\r', '\t'], " ");
    let clean = clean.trim();
    if clean.chars().count() <= max_chars {
        return clean.to_string();
    }
    let take = max_chars.saturating_sub(1);
    let mut out: String = clean.chars().take(take).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Discovery of repos / runs / history for evaluation & migration
// ---------------------------------------------------------------------------

fn registered_repo_runs(state: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut out: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    let mut push_pair = |repo: PathBuf, runs_dir: PathBuf| {
        let key = fs::canonicalize(&runs_dir).unwrap_or_else(|_| runs_dir.clone());
        if seen.insert(key.clone()) {
            out.push((repo, key));
        }
    };

    // Current repo / runs_root
    let cur_runs = runs_root();
    let cur_repo = {
        let r = current_repo_str();
        if r.is_empty() {
            cur_runs
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| cur_runs.clone())
        } else {
            PathBuf::from(r)
        }
    };
    push_pair(cur_repo, cur_runs);

    let doc = json(state.join("repos.json"));
    if let Some(map) = doc["repos"].as_object() {
        for entry in map.values() {
            let runs_s = s(entry, "runs");
            let repo_s = s(entry, "repo");
            if runs_s.is_empty() {
                continue;
            }
            let rdir = PathBuf::from(runs_s);
            let rrepo = if repo_s.is_empty() {
                rdir.parent()
                    .and_then(Path::parent)
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| rdir.clone())
            } else {
                PathBuf::from(repo_s)
            };
            push_pair(rrepo, rdir);
        }
    }
    out
}

fn file_sig(path: &Path) -> String {
    let Ok(md) = fs::metadata(path) else {
        return String::new();
    };
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}:{}", md.len(), mtime)
}

/// Automatically migrate legacy `<runs>/notes.json` and `<repo>/.local/delegate/deferred.tsv`
/// into `<state>/work.jsonl` once (idempotently). Never modifies `notes.json` or `deferred.tsv`.
fn migrate_legacy_locked(state: &Path, items: &mut Vec<WorkItem>) -> bool {
    migrate_legacy_impl(state, items, true)
}

fn migrate_legacy_impl(state: &Path, items: &mut Vec<WorkItem>, persist: bool) -> bool {
    let mig_path = work_migrations_path(state);
    let mut mig = json(&mig_path);
    if !mig.is_object() {
        mig = json!({});
    }
    let now_epoch = epoch() as i64;
    let now_iso = iso();
    let defer_hours = std::env::var("DEFER_HOURS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0 && v.is_finite())
        .unwrap_or(4.0);

    let mut changed = false;
    let mut mig_changed = false;

    for (repo_root, runs_dir) in registered_repo_runs(state) {
        let repo_str = fs::canonicalize(&repo_root)
            .unwrap_or_else(|_| repo_root.clone())
            .to_string_lossy()
            .into_owned();

        // 1. Migrate legacy `<runs>/notes.json`
        let notes_file = runs_dir.join("notes.json");
        if notes_file.is_file() {
            let key = format!("notes:{}", notes_file.display());
            let sig = file_sig(&notes_file);
            if s(&mig, &key) != sig {
                let chains = notes::load_in(&runs_dir);
                for chain in chains {
                    if chain.items.is_empty() {
                        continue;
                    }
                    // Find existing work item by note_root or any run id
                    let existing_pos = items.iter().position(|w| {
                        (!w.note_root.is_empty() && w.note_root == chain.root)
                            || chain.runs.iter().any(|r| w.matches_run(r))
                    });
                    let idx = if let Some(pos) = existing_pos {
                        pos
                    } else {
                        if chain.pending_count() == 0 {
                            continue;
                        }
                        let id = next_id_impl(state, items, persist);
                        let mut w = WorkItem {
                            id,
                            title: if chain.name.is_empty() {
                                chain.latest_run.clone()
                            } else {
                                chain.name.clone()
                            },
                            repo: repo_str.clone(),
                            state: if chain.applied || chain.cleaned {
                                "verify".to_string()
                            } else {
                                "attention".to_string()
                            },
                            created_at: now_iso.clone(),
                            updated_at: now_iso.clone(),
                            state_since: now_iso.clone(),
                            origin: "note-migration".to_string(),
                            note_root: chain.root.clone(),
                            applied: chain.applied,
                            cleaned: chain.cleaned,
                            ..Default::default()
                        };
                        for rid in &chain.runs {
                            let rdir = runs_dir.join(rid);
                            w.add_run(
                                rid,
                                rdir.is_dir().then_some(rdir.as_path()),
                                Some(&repo_str),
                            );
                        }
                        items.push(w);
                        changed = true;
                        items.len() - 1
                    };
                    let entry = &mut items[idx];
                    if entry.note_root.is_empty() {
                        entry.note_root = chain.root.clone();
                    }
                    entry.applied |= chain.applied;
                    entry.cleaned |= chain.cleaned;
                    for rid in &chain.runs {
                        let rdir = runs_dir.join(rid);
                        entry.add_run(
                            rid,
                            rdir.is_dir().then_some(rdir.as_path()),
                            Some(&repo_str),
                        );
                    }
                    for note_item in &chain.items {
                        let exists = entry
                            .followups
                            .iter()
                            .any(|f| f.text == note_item.text && f.created_at == note_item.created_at);
                        if !exists {
                            let next_fid = entry.followups.iter().map(|f| f.id).max().unwrap_or(0) + 1;
                            entry.followups.push(FollowupItem {
                                id: next_fid,
                                kind: "note".to_string(),
                                text: note_item.text.clone(),
                                anchor: notes_file.to_string_lossy().into_owned(),
                                source_run: chain.latest_run.clone(),
                                done: note_item.done,
                                created_at: note_item.created_at.clone(),
                                done_at: note_item.done_at.clone(),
                            });
                            changed = true;
                        }
                    }
                    if entry.pending_followups() > 0 && entry.state == "done" {
                        entry.set_state("verify", &now_iso);
                        changed = true;
                    }
                }
                mig[key] = json!(sig);
                mig_changed = true;
            }
        }

        // 2. Migrate legacy `<repo>/.local/delegate/deferred.tsv`
        let def_file = repo_root.join(".local/delegate/deferred.tsv");
        if def_file.is_file() {
            let key = format!("deferred:{}", def_file.display());
            let sig = file_sig(&def_file);
            if s(&mig, &key) != sig {
                let text = read(&def_file);
                let limit_secs = (defer_hours * 3600.0).round() as i64;
                for raw in text.lines() {
                    let trimmed = raw.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let cols: Vec<&str> = raw.split('\t').collect();
                    let rid = cols.first().map(|x| x.trim()).unwrap_or("");
                    if rid.is_empty() {
                        continue;
                    }
                    let reason = cols.get(1).map(|x| x.trim()).unwrap_or("延后处理");
                    let ts = cols
                        .get(2)
                        .and_then(|x| x.trim().parse::<i64>().ok())
                        .unwrap_or(now_epoch);
                    let until_epoch = ts.saturating_add(limit_secs);
                    let until_iso = format_epoch(until_epoch, false, "%Y-%m-%dT%H:%M:%SZ");
                    if until_epoch <= now_epoch {
                        // If a previously auto-migrated entry in work.jsonl now expired in deferred.tsv, sync it.
                        if let Some(pos) = items.iter().position(|w| {
                            w.origin == "deferred-migration" && w.matches_run(rid)
                        }) {
                            items[pos].defer_until = until_iso;
                            changed = true;
                        }
                        continue;
                    }
                    let run_dir = runs_dir.join(rid);
                    if run_dir.join(".applied").exists() || run_dir.join(".applied-cleaned").exists() {
                        continue;
                    }
                    if let Some(pos) = items.iter().position(|w| w.matches_run(rid)) {
                        if items[pos].state != "done" && items[pos].state != "deferred" {
                            items[pos].defer_reason = reason.to_string();
                            items[pos].defer_until = until_iso;
                            items[pos].set_state("deferred", &now_iso);
                            changed = true;
                        }
                    } else {
                        let meta = json(run_dir.join("meta.json"));
                        let title = {
                            let n = s(&meta, "name");
                            if n.is_empty() {
                                rid.to_string()
                            } else {
                                n.to_string()
                            }
                        };
                        let id = next_id_impl(state, items, persist);
                        let mut w = WorkItem {
                            id,
                            title,
                            repo: repo_str.clone(),
                            state: "deferred".to_string(),
                            defer_reason: reason.to_string(),
                            defer_until: until_iso,
                            created_at: now_iso.clone(),
                            updated_at: now_iso.clone(),
                            state_since: now_iso.clone(),
                            origin: "deferred-migration".to_string(),
                            ..Default::default()
                        };
                        w.add_run(
                            rid,
                            run_dir.is_dir().then_some(run_dir.as_path()),
                            Some(&repo_str),
                        );
                        items.push(w);
                        changed = true;
                    }
                }
                mig[key] = json!(sig);
                mig_changed = true;
            }
        }
    }

    if persist && mig_changed {
        let _ = fs::create_dir_all(state);
        let _ = write_json(&mig_path, &mig);
    }
    changed
}

// ---------------------------------------------------------------------------
// Run lookup & wait_on evaluation (zero token, pure deterministic)
// ---------------------------------------------------------------------------

fn is_archived_run_dir(dir: &Path) -> bool {
    if crate::archive::is_record(dir) {
        return true;
    }
    if dir.file_name().and_then(|n| n.to_str()) == Some("record")
        && dir
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            == Some("archive")
    {
        return true;
    }
    dir.parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        == Some(".applied")
}

/// Find a live (non-archived, non-cleaned) run directory across registered repos.
/// Never resolves `<cache>/archive/<run>/record` or legacy `<runs>/.applied/<run>`.
fn find_live_run_dir(
    run_ref: &str,
    preferred_dirs: &BTreeMap<String, String>,
    state: &Path,
) -> Option<PathBuf> {
    if let Some(p) = preferred_dirs.get(run_ref) {
        let pb = PathBuf::from(p);
        if pb.join("meta.json").is_file() && !is_archived_run_dir(&pb) {
            return Some(pb);
        }
    }
    let mut partial_matches = Vec::new();
    for (_, runs_dir) in registered_repo_runs(state) {
        let exact = runs_dir.join(run_ref);
        if exact.join("meta.json").is_file() && !is_archived_run_dir(&exact) {
            return Some(exact);
        }
        if let Ok(rd) = fs::read_dir(&runs_dir) {
            for ent in rd.flatten() {
                let p = ent.path();
                if !p.join("meta.json").is_file() || is_archived_run_dir(&p) {
                    continue;
                }
                let fname = ent.file_name().to_string_lossy().into_owned();
                if fname == run_ref {
                    return Some(p);
                }
                if fname.contains(run_ref) {
                    partial_matches.push(p);
                }
            }
        }
    }
    if partial_matches.len() == 1 {
        return partial_matches.into_iter().next();
    }
    None
}

fn delivered_pending_files(rdir: &Path, meta: &Value, sum: &Value) -> u64 {
    if let Some(pending) = sum.get("pendingChanges").filter(|v| v.is_object()) {
        return pending["files"].as_u64().unwrap_or(0);
    }
    let own = sum["changes"]["files"].as_u64().unwrap_or(0);
    if own > 0 {
        return own;
    }
    if meta["worktree"].is_object() {
        if let Some(parent_dir) = rdir.parent() {
            let all = runs::all_runs_in(parent_dir);
            let chain_f = runs::chain_pending_apply_files_opts(rdir, meta, sum, &all, false);
            if chain_f > 0 {
                return chain_f as u64;
            }
        }
    }
    0
}

fn is_zero_change_delivered(rdir: &Path, meta: &Value, sum: &Value) -> bool {
    runs::state(rdir) == "delivered"
        && s(meta, "mode") == "write"
        && !rdir.join(".applied").exists()
        && !rdir.join(".applied-cleaned").exists()
        && delivered_pending_files(rdir, meta, sum) == 0
}

fn find_run_dir(run_ref: &str, preferred_dirs: &BTreeMap<String, String>, state: &Path) -> Option<PathBuf> {
    if let Some(p) = find_live_run_dir(run_ref, preferred_dirs, state) {
        return Some(p);
    }
    if let Ok(p) = runs::resolve_head(run_ref) {
        if p.join("meta.json").is_file() {
            return Some(p);
        }
    }
    None
}

fn run_id_matches(candidate: &str, needle: &str) -> bool {
    if candidate.is_empty() || needle.is_empty() {
        return false;
    }
    candidate == needle
        || candidate.ends_with(&format!("-{needle}"))
        || candidate.ends_with(needle)
        || (needle.len() >= 4 && candidate.contains(needle))
}

fn history_files_for_state(state: &Path, extra_repo: Option<&str>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut add = |p: PathBuf| {
        if p.is_file() {
            let k = fs::canonicalize(&p).unwrap_or(p);
            if seen.insert(k.clone()) {
                dirs.push(k);
            }
        }
    };
    if let Some(rp) = extra_repo.filter(|x| !x.is_empty()) {
        add(Path::new(rp).join(".local/run/delegate/history.jsonl"));
    }
    add(runs_root().join("history.jsonl"));
    for (repo, runs_dir) in registered_repo_runs(state) {
        add(runs_dir.join("history.jsonl"));
        add(repo.join(".local/run/delegate/history.jsonl"));
    }
    dirs
}

fn history_entry_for_run(run_ref: &str, state: &Path, repo_hint: Option<&str>) -> Option<Value> {
    for hist_path in history_files_for_state(state, repo_hint) {
        let Ok(f) = OpenOptions::new().read(true).open(&hist_path) else {
            continue;
        };
        let mut matched: Option<Value> = None;
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            let rid = s(&v, "run");
            let name = s(&v, "name");
            if run_id_matches(rid, run_ref) || name == run_ref {
                matched = Some(v);
            }
        }
        if matched.is_some() {
            return matched;
        }
    }
    None
}

/// Find archived run record directory under `<cache>/archive/<run>/record`.
fn find_archived_record(run_ref: &str) -> Option<PathBuf> {
    let arch_root = cache_dir().join("archive");
    let exact = arch_root.join(run_ref).join("record");
    if exact.join("meta.json").is_file() {
        return Some(exact);
    }
    let rd = fs::read_dir(&arch_root).ok()?;
    let mut matches = Vec::new();
    for ent in rd.flatten() {
        let rec = ent.path().join("record");
        if !rec.join("meta.json").is_file() {
            continue;
        }
        let dir_name = ent.file_name().to_string_lossy().into_owned();
        if run_id_matches(&dir_name, run_ref) {
            matches.push(rec);
        }
    }
    (matches.len() == 1).then(|| matches.remove(0))
}

/// Pure deterministic read-only check: has `run_ref` been applied?
/// Never writes files or acquires locks.
pub fn is_run_applied(run_ref: &str, state: &Path, repo_hint: Option<&str>) -> bool {
    let empty_dirs = BTreeMap::new();
    if let Some(dir) = find_run_dir(run_ref, &empty_dirs, state) {
        if dir.join(".applied").exists() || dir.join(".applied-cleaned").exists() {
            return true;
        }
        let meta = json(dir.join("meta.json"));
        if !s(&meta, "appliedAt").is_empty() {
            return true;
        }
        if !runs::active(&runs::state(&dir))
            && crate::worktree::is_applied_in_repo_readonly(&dir, &meta)
        {
            return true;
        }
    }
    if let Some(h) = history_entry_for_run(run_ref, state, repo_hint) {
        if !s(&h, "appliedAt").is_empty() {
            return true;
        }
    }
    // Check archived record (handles runs cleaned after manual merge).
    if let Some(arch) = find_archived_record(run_ref) {
        if arch.join(".applied").exists() || arch.join(".applied-cleaned").exists() {
            return true;
        }
        let meta = json(arch.join("meta.json"));
        if !s(&meta, "appliedAt").is_empty() {
            return true;
        }
        if crate::worktree::is_applied_in_repo_readonly(&arch, &meta) {
            return true;
        }
    }
    // Fallback: check events.jsonl for `applied` event matching run_ref
    let ev_path = state.join("events.jsonl");
    if let Ok(f) = OpenOptions::new().read(true).open(ev_path) {
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() || !trimmed.contains("\"applied\"") {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                if s(&v, "kind") == "applied" && run_id_matches(s(&v, "run"), run_ref) {
                    return true;
                }
            }
        }
    }
    false
}

/// Pure deterministic check: has `run_ref` delivered cleanly?
pub fn is_run_delivered(run_ref: &str, state: &Path, repo_hint: Option<&str>) -> bool {
    let empty_dirs = BTreeMap::new();
    if let Some(dir) = find_run_dir(run_ref, &empty_dirs, state) {
        return matches!(dag_queue::evaluate_run_dir(&dir), dag_queue::UpstreamVerdict::Satisfied);
    }
    if let Some(h) = history_entry_for_run(run_ref, state, repo_hint) {
        let st = s(&h, "state");
        return matches!(st, "delivered" | "answered") && !b(&h, "question");
    }
    false
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnerStatus {
    Waiting,
    Resolved,
    Rejected,
    MissingFile,
}

/// Strictly read-only scan of `<repo>/docs/owner/README.md` for `owner_ref` (e.g. `Q18` or `D002`).
/// NEVER writes to or modifies `docs/owner/README.md`.
pub fn check_owner_readme(repo: &Path, owner_ref: &str) -> OwnerStatus {
    let path = repo.join("docs/owner/README.md");
    let Ok(content) = fs::read_to_string(&path) else {
        return OwnerStatus::MissingFile;
    };
    let needle = owner_ref.trim();
    if needle.is_empty() {
        return OwnerStatus::Resolved;
    }
    let mut in_rejected = false;
    let mut in_footer = false;
    let mut found_open = false;
    let mut found_rejected = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "---" {
            in_footer = true;
            continue;
        }
        if in_footer {
            continue;
        }
        if trimmed.starts_with("## ") {
            in_rejected = trimmed.contains("已否");
            continue;
        }
        if !trimmed.starts_with('|') {
            continue;
        }
        if contains_owner_token(trimmed, needle) {
            if in_rejected {
                found_rejected = true;
            } else {
                found_open = true;
            }
        }
    }
    if found_open {
        OwnerStatus::Waiting
    } else if found_rejected {
        OwnerStatus::Rejected
    } else {
        OwnerStatus::Resolved
    }
}

fn contains_owner_token(line: &str, token: &str) -> bool {
    let bytes = line.as_bytes();
    let tbytes = token.as_bytes();
    if tbytes.is_empty() || bytes.len() < tbytes.len() {
        return false;
    }
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = 0;
    while let Some(pos) = line[start..].find(token) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_word(bytes[abs - 1]);
        let after = abs + tbytes.len();
        let after_ok = after >= bytes.len() || !is_word(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

#[derive(Clone, Debug)]
pub struct CondEval {
    pub raw: String,
    pub satisfied: bool,
    pub detail: String,
    pub pointer: String,
}

fn resolve_git_common_dir_fs(repo: &Path) -> Option<PathBuf> {
    if !repo.is_dir() {
        return None;
    }
    if repo.join("HEAD").is_file() && repo.join("objects").is_dir() {
        return Some(repo.to_path_buf());
    }
    let dot_git = repo.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    if dot_git.is_file() {
        let content = fs::read_to_string(&dot_git).ok()?;
        let raw = content.trim().strip_prefix("gitdir:")?.trim();
        let gitdir = {
            let p = PathBuf::from(raw);
            if p.is_absolute() {
                p
            } else {
                repo.join(p)
            }
        };
        if let Ok(common_raw) = fs::read_to_string(gitdir.join("commondir")) {
            let trimmed = common_raw.trim();
            if !trimmed.is_empty() {
                let cp = PathBuf::from(trimmed);
                let resolved = if cp.is_absolute() {
                    cp
                } else {
                    gitdir.join(cp)
                };
                if resolved.is_dir() {
                    return Some(resolved);
                }
            }
        }
        if gitdir.is_dir() {
            return Some(gitdir);
        }
    }
    None
}

fn git_object_dirs_fs(git_dir: &Path) -> Vec<PathBuf> {
    let main = git_dir.join("objects");
    let mut dirs = vec![main.clone()];
    let mut seen: HashSet<PathBuf> = HashSet::new();
    seen.insert(main);
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

fn entry_matches_hex_prefix(entry: &[u8], full_bytes: &[u8], odd_nibble: Option<u8>) -> bool {
    if entry.len() < full_bytes.len() {
        return false;
    }
    if &entry[..full_bytes.len()] != full_bytes {
        return false;
    }
    if let Some(nib) = odd_nibble {
        if entry.len() <= full_bytes.len() || (entry[full_bytes.len()] >> 4) != nib {
            return false;
        }
    }
    true
}

fn sha_prefix_exists_fs(obj_dirs: &[PathBuf], sha_lower: &str) -> Option<bool> {
    if sha_lower.len() < 4
        || sha_lower.len() > 40
        || !sha_lower.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Some(false);
    }
    let prefix = &sha_lower[..2];
    let suffix = &sha_lower[2..];
    let b0 = u8::from_str_radix(prefix, 16).ok()? as usize;

    let mut full_bytes = Vec::with_capacity(sha_lower.len() / 2);
    let bytes = sha_lower.as_bytes();
    for pair in bytes.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)? as u8;
        let lo = (pair[1] as char).to_digit(16)? as u8;
        full_bytes.push((hi << 4) | lo);
    }
    let odd_nibble = if sha_lower.len() % 2 == 1 {
        Some((bytes[sha_lower.len() - 1] as char).to_digit(16)? as u8)
    } else {
        None
    };

    let mut unknown = false;
    for dir in obj_dirs {
        let loose_dir = dir.join(prefix);
        if sha_lower.len() == 40 {
            if loose_dir.join(suffix).is_file() {
                return Some(true);
            }
        } else if let Ok(entries) = fs::read_dir(&loose_dir) {
            for ent in entries.flatten() {
                let name = ent.file_name();
                if let Some(ns) = name.to_str() {
                    if ns.starts_with(suffix) && ent.path().is_file() {
                        return Some(true);
                    }
                }
            }
        }

        let pack_dir = dir.join("pack");
        let Ok(entries) = fs::read_dir(&pack_dir) else {
            continue;
        };
        for ent in entries.flatten() {
            let path = ent.path();
            if path.extension().and_then(|e| e.to_str()) != Some("idx") {
                continue;
            }
            let Ok(data) = fs::read(&path) else {
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
            let mut lo = if b0 == 0 { 0 } else { u32_at(8 + (b0 - 1) * 4) };
            let mut hi = u32_at(8 + b0 * 4);
            let total = u32_at(8 + 255 * 4);
            let table_end = 1032usize.saturating_add(total.saturating_mul(20));
            if lo > hi || hi > total || data.len() < table_end {
                unknown = true;
                continue;
            }
            // Binary search for lower bound of full_bytes prefix within [lo, hi)
            let orig_hi = hi;
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let off = 1032 + mid * 20;
                if &data[off..off + full_bytes.len()] < full_bytes.as_slice() {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            let mut scan = lo;
            while scan < orig_hi {
                let off = 1032 + scan * 20;
                let entry = &data[off..off + 20];
                if &entry[..full_bytes.len()] != full_bytes.as_slice() {
                    break;
                }
                if entry_matches_hex_prefix(entry, &full_bytes, odd_nibble) {
                    return Some(true);
                }
                scan += 1;
            }
        }
    }
    if unknown { None } else { Some(false) }
}

pub fn commit_exists_in_repo(repo: &Path, sha: &str) -> bool {
    let sha_lower = sha.trim().to_ascii_lowercase();
    if sha_lower.len() < 4
        || sha_lower.len() > 40
        || !sha_lower.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return false;
    }
    let Some(git_dir) = resolve_git_common_dir_fs(repo) else {
        return false;
    };
    let obj_dirs = git_object_dirs_fs(&git_dir);
    match sha_prefix_exists_fs(&obj_dirs, &sha_lower) {
        Some(found) => found,
        None if crate::locks::holding_state_lock() => false,
        None => Command::new("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["cat-file", "-e", &format!("{sha_lower}^{{commit}}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|st| st.success()),
    }
}

#[derive(Default)]
struct ReconcilePrefetch {
    /// Precomputed `is_run_applied(run_ref, state, repo_hint)` keyed by `(repo, run_ref)`.
    run_applied: HashMap<(String, String), bool>,
    /// Precomputed `commit_exists_in_repo(repo, sha)` keyed by `(repo, sha)`.
    commit_exists: HashMap<(String, String), bool>,
}

/// Precompute potentially slow git/worktree-backed condition checks (`run_applied:*`,
/// `commit_exists:*`) strictly OUTSIDE `state/.state.lock` (P8: lock holds only state I/O).
fn prefetch_reconcile_conditions(
    state: &Path,
    items: &[WorkItem],
    extra: &[(&str, &str)],
) -> ReconcilePrefetch {
    let mut out = ReconcilePrefetch::default();
    let mut record = |repo: &str, cond: &str| {
        if let Some(run_ref) = cond.strip_prefix("run_applied:") {
            let key = (repo.to_string(), run_ref.to_string());
            out.run_applied.entry(key).or_insert_with(|| {
                let hint = (!repo.is_empty()).then_some(repo);
                is_run_applied(run_ref, state, hint)
            });
        } else if let Some(sha_raw) = cond.strip_prefix("commit_exists:") {
            let sha = sha_raw.trim();
            if !sha.is_empty() {
                let repo_str = if repo.is_empty() {
                    current_repo_str()
                } else {
                    repo.to_string()
                };
                let key = (repo_str.clone(), sha.to_string());
                out.commit_exists
                    .entry(key)
                    .or_insert_with(|| commit_exists_in_repo(Path::new(&repo_str), sha));
            }
        }
    };
    for item in items {
        if item.state == "done" {
            continue;
        }
        for c in &item.wait_on {
            record(&item.repo, c);
        }
        for c in &item.verify_on {
            record(&item.repo, c);
        }
    }
    for (repo, cond) in extra {
        record(repo, cond);
    }
    out
}

fn prefetch_reconcile_for_state(state: &Path, extra: &[(&str, &str)]) -> ReconcilePrefetch {
    let items = load_raw(state);
    prefetch_reconcile_conditions(state, &items, extra)
}

pub fn evaluate_condition(
    cond: &str,
    item: &WorkItem,
    all_items: &[WorkItem],
    state: &Path,
    now_epoch: i64,
) -> CondEval {
    evaluate_condition_with_prefetch(cond, item, all_items, state, now_epoch, None)
}

fn evaluate_condition_with_prefetch(
    cond: &str,
    item: &WorkItem,
    all_items: &[WorkItem],
    state: &Path,
    now_epoch: i64,
    prefetch: Option<&ReconcilePrefetch>,
) -> CondEval {
    if let Some(run_ref) = cond.strip_prefix("run_applied:") {
        let ok = all_items
            .iter()
            .any(|w| w.applied && w.matches_run(run_ref))
            || prefetch
                .and_then(|p| {
                    p.run_applied
                        .get(&(item.repo.clone(), run_ref.to_string()))
                        .copied()
                })
                .unwrap_or_else(|| is_run_applied(run_ref, state, Some(&item.repo)));
        return CondEval {
            raw: cond.to_string(),
            satisfied: ok,
            detail: if ok {
                format!("上游 {run_ref} 已合入")
            } else {
                format!("等 {run_ref} 合入")
            },
            pointer: format!("delegate show {run_ref}"),
        };
    }
    if let Some(run_ref) = cond.strip_prefix("run_delivered:") {
        let ok = is_run_delivered(run_ref, state, Some(&item.repo));
        return CondEval {
            raw: cond.to_string(),
            satisfied: ok,
            detail: if ok {
                format!("上游 {run_ref} 已交付")
            } else {
                format!("等 {run_ref} 交付")
            },
            pointer: format!("delegate show {run_ref}"),
        };
    }
    if let Some(wid) = cond.strip_prefix("work_applied:") {
        let norm = normalize_work_id(wid).unwrap_or_else(|| wid.to_string());
        let target = all_items.iter().find(|w| w.id == norm);
        let (ok, detail) = match target {
            Some(w) if w.state == "done" && w.closed_kind == "dropped" => {
                (false, format!("工作项 {norm} 已放弃（需人工确认）"))
            }
            Some(w) if w.applied => (true, format!("工作项 {norm} 已合入")),
            Some(w) if w.state == "done" => (true, format!("工作项 {norm} 已关闭")),
            Some(w) => (false, format!("等 {norm} 合入（当前 {}）", w.state)),
            None => (false, format!("找不到工作项 {norm}")),
        };
        return CondEval {
            raw: cond.to_string(),
            satisfied: ok,
            detail,
            pointer: format!("delegate work show {norm}"),
        };
    }
    if let Some(wid) = cond.strip_prefix("work_closed:") {
        let norm = normalize_work_id(wid).unwrap_or_else(|| wid.to_string());
        let target = all_items.iter().find(|w| w.id == norm);
        let (ok, detail) = match target {
            Some(w) if w.state == "done" && w.closed_kind != "dropped" => {
                (true, format!("工作项 {norm} 已关闭"))
            }
            Some(w) if w.state == "done" && w.closed_kind == "dropped" => {
                (false, format!("工作项 {norm} 已放弃（需人工确认）"))
            }
            Some(w) => (false, format!("等 {norm} 关闭（当前 {}）", w.state)),
            None => (false, format!("找不到工作项 {norm}")),
        };
        return CondEval {
            raw: cond.to_string(),
            satisfied: ok,
            detail,
            pointer: format!("delegate work show {norm}"),
        };
    }
    if let Some(oref) = cond.strip_prefix("owner:") {
        let trimmed = oref.trim();
        let norm = normalize_work_id(trimmed).unwrap_or_else(|| trimmed.to_string());
        if let Some(owner_item) = all_items
            .iter()
            .find(|w| w.id == norm || w.id.eq_ignore_ascii_case(trimmed))
        {
            let (ok, detail) = if owner_item.state == "done" && owner_item.closed_kind != "dropped" {
                if owner_item.close_evidence.is_empty() {
                    (true, format!("所有者 {} 已定", owner_item.id))
                } else {
                    (
                        true,
                        format!(
                            "所有者 {} 已定：{}",
                            owner_item.id,
                            clip_chars(&owner_item.close_evidence, 22)
                        ),
                    )
                }
            } else if owner_item.state == "done" && owner_item.closed_kind == "dropped" {
                (false, format!("所有者 {} 已否（需人工确认）", owner_item.id))
            } else {
                (false, format!("等所有者 {}", owner_item.id))
            };
            return CondEval {
                raw: cond.to_string(),
                satisfied: ok,
                detail,
                pointer: format!("delegate work show {}", owner_item.id),
            };
        }
        let repo_path = if item.repo.is_empty() {
            PathBuf::from(current_repo_str())
        } else {
            PathBuf::from(&item.repo)
        };
        let readme = repo_path.join("docs/owner/README.md");
        let (ok, detail) = match check_owner_readme(&repo_path, oref) {
            OwnerStatus::Waiting => (false, format!("等所有者 {oref}")),
            OwnerStatus::Resolved => (true, format!("所有者 {oref} 已拍板/移出案头")),
            OwnerStatus::Rejected => (true, format!("所有者 {oref} 已否")),
            OwnerStatus::MissingFile => (false, format!("等所有者 {oref}（未找到工作项或 docs/owner/README.md）")),
        };
        return CondEval {
            raw: cond.to_string(),
            satisfied: ok,
            detail,
            pointer: readme.to_string_lossy().into_owned(),
        };
    }
    if let Some(sha_raw) = cond.strip_prefix("commit_exists:") {
        let sha = sha_raw.trim();
        let repo_str = if item.repo.is_empty() {
            current_repo_str()
        } else {
            item.repo.clone()
        };
        let repo_path = PathBuf::from(&repo_str);
        let ok = !sha.is_empty()
            && prefetch
                .and_then(|p| {
                    p.commit_exists
                        .get(&(repo_str.clone(), sha.to_string()))
                        .copied()
                })
                .unwrap_or_else(|| commit_exists_in_repo(&repo_path, sha));
        return CondEval {
            raw: cond.to_string(),
            satisfied: ok,
            detail: if ok {
                format!("提交 {sha} 已存在")
            } else {
                format!("等提交 {sha}")
            },
            pointer: if repo_path.as_os_str().is_empty() {
                format!("git show --stat {sha}")
            } else {
                format!("git -C {} show --stat {sha}", repo_path.display())
            },
        };
    }
    if let Some(ts_str) = cond.strip_prefix("after_time:") {
        let ok = parse_iso_or_date_epoch(ts_str).is_some_and(|t| now_epoch >= t);
        return CondEval {
            raw: cond.to_string(),
            satisfied: ok,
            detail: if ok {
                format!("已到时间 {ts_str}")
            } else {
                format!("等到 {ts_str}")
            },
            pointer: String::new(),
        };
    }
    CondEval {
        raw: cond.to_string(),
        satisfied: false,
        detail: format!("未知前置：{cond}"),
        pointer: String::new(),
    }
}

fn latest_live_run_excluding(
    item: &WorkItem,
    state: &Path,
    exclude_run: Option<&str>,
) -> Option<(String, PathBuf)> {
    for rid in item.runs.iter().rev() {
        if exclude_run.is_some_and(|ex| ex == rid.as_str()) {
            continue;
        }
        if let Some(dir) = find_live_run_dir(rid, &item.run_dirs, state) {
            return Some((rid.clone(), dir));
        }
    }
    None
}

fn latest_live_run(item: &WorkItem, state: &Path) -> Option<(String, PathBuf)> {
    latest_live_run_excluding(item, state, None)
}

#[derive(Clone, Debug)]
struct FillCandidate {
    item: WorkItem,
    active_writes: usize,
}

struct ReconcileOutcome {
    changed: bool,
    unlocked_ready: Vec<WorkItem>,
    to_auto_dispatch: Vec<WorkItem>,
    to_fill_dispatch: Vec<FillCandidate>,
    defer_expired: Vec<WorkItem>,
    stale_items: Vec<(WorkItem, String)>,
}

fn canonical_repo_key(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    fs::canonicalize(trimmed)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| trimmed.trim_end_matches('/').to_string())
}

fn load_repo_fill_and_hotspots(repo: &str) -> Option<(crate::config::FillConfig, Vec<String>)> {
    let trimmed = repo.trim();
    if trimmed.is_empty() {
        return None;
    }
    let cfg_path = Path::new(trimmed).join(".delegate.json");
    let doc = crate::config::read_file(&cfg_path).ok().flatten()?;
    let fill = crate::config::parse_fill_config(doc.get("fill"))?;
    let hotspots = crate::config::parse_hotspots_config(doc.get("hotspots"));
    Some((fill, hotspots))
}

fn waiting_out_degree(id: &str, items: &[WorkItem]) -> usize {
    items
        .iter()
        .filter(|w| {
            w.state != "done"
                && w.wait_on.iter().any(|c| {
                    c.strip_prefix("work_closed:")
                        .or_else(|| c.strip_prefix("work_applied:"))
                        .and_then(normalize_work_id)
                        .as_deref()
                        == Some(id)
                })
        })
        .count()
}

fn select_fill_candidates(
    state: &Path,
    items: &[WorkItem],
    exclude_run: Option<&str>,
    already_dispatched_ids: &HashSet<String>,
) -> Vec<FillCandidate> {
    // Collect distinct canonical repos that have at least one potentially eligible ready item.
    let mut candidate_repos: Vec<String> = Vec::new();
    let mut seen_repos: HashSet<String> = HashSet::new();
    for item in items {
        if item.owner.is_some()
            || item.state != "ready"
            || item.brief.trim().is_empty()
            || !WORK_CLASSES.contains(&item.class.as_str())
            || already_dispatched_ids.contains(&item.id)
        {
            continue;
        }
        let Some(ad) = &item.auto_dispatch else {
            continue;
        };
        if !ad.dispatched_run.is_empty() || is_readonly_start_args(&ad.start_args) {
            continue;
        }
        let rkey = canonical_repo_key(&item.repo);
        if !rkey.is_empty() && seen_repos.insert(rkey.clone()) {
            candidate_repos.push(rkey);
        }
    }
    if candidate_repos.is_empty() {
        return Vec::new();
    }
    candidate_repos.sort();

    // Pre-scan live active write runs once across registered repo run dirs & item run_dirs.
    let mut seen_run_dirs: HashSet<PathBuf> = HashSet::new();
    let mut active_runs_by_repo: HashMap<String, Vec<(String, PathBuf, Value)>> = HashMap::new();
    let mut inspect_run_dir = |rdir: PathBuf, fallback_repo: &str| {
        let canon_dir = fs::canonicalize(&rdir).unwrap_or_else(|_| rdir.clone());
        if !seen_run_dirs.insert(canon_dir) {
            return;
        }
        if !rdir.join("meta.json").is_file() || is_archived_run_dir(&rdir) {
            return;
        }
        let meta = json(rdir.join("meta.json"));
        let rid = notes::run_id(&rdir, &meta);
        if rid.is_empty() || exclude_run.is_some_and(|ex| ex == rid) {
            return;
        }
        let st = runs::state(&rdir);
        if !runs::active(&st) {
            return;
        }
        // Read-only / scout / research runs never count against fill.target.
        if s(&meta, "mode") == "read-only" {
            return;
        }
        let run_repo = {
            let from_meta = events::repo_for_meta(&meta);
            if !from_meta.is_empty() {
                canonical_repo_key(&from_meta)
            } else {
                canonical_repo_key(fallback_repo)
            }
        };
        if !run_repo.is_empty() && seen_repos.contains(&run_repo) {
            active_runs_by_repo
                .entry(run_repo)
                .or_default()
                .push((rid, rdir, meta));
        }
    };

    for (repo_root, runs_dir) in registered_repo_runs(state) {
        if !runs_dir.is_dir() {
            continue;
        }
        let fallback = repo_root.to_string_lossy().into_owned();
        if let Ok(rd) = fs::read_dir(&runs_dir) {
            for ent in rd.flatten() {
                let p = ent.path();
                if p.is_dir() {
                    inspect_run_dir(p, &fallback);
                }
            }
        }
    }
    for item in items {
        for dir_s in item.run_dirs.values() {
            let p = PathBuf::from(dir_s);
            if p.is_dir() {
                inspect_run_dir(p, &item.repo);
            }
        }
    }

    let mut out = Vec::new();
    for rkey in candidate_repos {
        let Some((fill_cfg, hotspots)) = load_repo_fill_and_hotspots(&rkey) else {
            continue;
        };

        let mut counted_runs: HashSet<String> = HashSet::new();
        let mut active_writes: usize = 0;
        let mut active_infra_docs: usize = 0;
        let mut active_files: Vec<String> = Vec::new();

        if let Some(live_runs) = active_runs_by_repo.get(&rkey) {
            for (rid, _rdir, meta) in live_runs {
                if !counted_runs.insert(rid.clone()) {
                    continue;
                }
                active_writes += 1;
                active_files.extend(strings(&meta["files"]));
                let work_id = s(meta, "workId");
                let linked = items.iter().find(|w| {
                    (!work_id.is_empty() && w.id == work_id) || w.matches_run(rid)
                });
                if let Some(w) = linked {
                    if is_infra_or_docs(&w.class) {
                        active_infra_docs += 1;
                    }
                    active_files.extend(w.files.iter().cloned());
                }
            }
        }

        // Also account for active work items (or items already selected in this reconcile pass)
        // whose live run dir was not counted above (e.g. synthetic active items or freshly queued).
        for w in items {
            if w.owner.is_some() || canonical_repo_key(&w.repo) != rkey {
                continue;
            }
            let is_active_item =
                w.state == "active" || already_dispatched_ids.contains(&w.id);
            if !is_active_item {
                continue;
            }
            if w.runs.iter().any(|r| counted_runs.contains(r)) {
                continue;
            }
            // If it has a live run and that live run is read-only, skip it!
            if let Some((_, rdir)) = latest_live_run_excluding(w, state, exclude_run) {
                let meta = json(rdir.join("meta.json"));
                if s(&meta, "mode") == "read-only" {
                    continue;
                }
            } else if w
                .auto_dispatch
                .as_ref()
                .is_some_and(|ad| is_readonly_start_args(&ad.start_args))
            {
                continue;
            }
            active_writes += 1;
            if is_infra_or_docs(&w.class) {
                active_infra_docs += 1;
            }
            active_files.extend(w.files.iter().cloned());
        }

        if active_writes >= fill_cfg.target {
            continue;
        }

        let max_infra_docs =
            crate::fill::max_infra_docs_slots(fill_cfg.target, fill_cfg.max_infra_docs_share);
        let active_hotspots = crate::fill::active_hotspots_from_files(&active_files, &hotspots);

        let mut cands: Vec<(&WorkItem, u8, usize, i64)> = items
            .iter()
            .filter(|w| {
                w.owner.is_none()
                    && w.state == "ready"
                    && canonical_repo_key(&w.repo) == rkey
                    && !w.brief.trim().is_empty()
                    && WORK_CLASSES.contains(&w.class.as_str())
                    && !already_dispatched_ids.contains(&w.id)
                    && w.auto_dispatch.as_ref().is_some_and(|ad| {
                        ad.dispatched_run.is_empty() && !is_readonly_start_args(&ad.start_args)
                    })
            })
            .map(|w| {
                let prio = work_class_priority(&w.class);
                let out_deg = waiting_out_degree(&w.id, items);
                let ready_ts = parse_iso_or_date_epoch(&w.state_since)
                    .or_else(|| parse_iso_or_date_epoch(&w.created_at))
                    .unwrap_or(i64::MAX);
                (w, prio, out_deg, ready_ts)
            })
            .collect();

        // Sort: feat > unblock > infra > docs; then out-degree DESC; then ready time ASC; then id ASC.
        cands.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| a.3.cmp(&b.3))
                .then_with(|| a.0.state_since.cmp(&b.0.state_since))
                .then_with(|| a.0.id.cmp(&b.0.id))
        });

        for (cand, _, _, _) in cands {
            if is_infra_or_docs(&cand.class) && active_infra_docs + 1 > max_infra_docs {
                continue;
            }
            if crate::fill::candidate_hits_active_hotspot(&cand.files, &active_hotspots) {
                continue;
            }
            out.push(FillCandidate {
                item: cand.clone(),
                active_writes,
            });
            break;
        }
    }
    out
}

#[cfg(test)]
fn reconcile_all_locked(state: &Path, items: &mut [WorkItem]) -> ReconcileOutcome {
    reconcile_all_locked_full(state, items, None, None, true)
}

fn reconcile_all_locked_with_prefetch(
    state: &Path,
    items: &mut [WorkItem],
    exclude_run: Option<&str>,
    prefetch: Option<&ReconcilePrefetch>,
) -> ReconcileOutcome {
    reconcile_all_locked_full(state, items, exclude_run, prefetch, true)
}

fn reconcile_all_locked_full(
    state: &Path,
    items: &mut [WorkItem],
    exclude_run: Option<&str>,
    prefetch: Option<&ReconcilePrefetch>,
    compute_fill: bool,
) -> ReconcileOutcome {
    let now_epoch = epoch() as i64;
    let now_iso = iso();
    let verify_stale_secs = std::env::var("DELEGATE_WORK_STALE_VERIFY_HOURS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_STALE_VERIFY_HOURS)
        * 3600.0;
    let ready_stale_secs = std::env::var("DELEGATE_WORK_STALE_READY_HOURS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_STALE_READY_HOURS)
        * 3600.0;
    let cooldown_secs = std::env::var("DELEGATE_WORK_REMIND_COOLDOWN_HOURS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_REMIND_COOLDOWN_HOURS)
        * 3600.0;

    let mut out = ReconcileOutcome {
        changed: false,
        unlocked_ready: Vec::new(),
        to_auto_dispatch: Vec::new(),
        to_fill_dispatch: Vec::new(),
        defer_expired: Vec::new(),
        stale_items: Vec::new(),
    };

    for _pass in 0..4 {
        let mut any_auto_closed = false;
        let snapshot: Vec<WorkItem> = items.to_vec();

        for item in items.iter_mut() {
            if item.state == "done" {
                continue;
            }

            // 1. Check deferred expiry first
            if item.state == "deferred" {
                if let Some(until_ts) = parse_iso_or_date_epoch(&item.defer_until) {
                    if now_epoch < until_ts {
                        continue;
                    }
                    // Expired!
                    item.defer_until.clear();
                    out.defer_expired.push(item.clone());
                    out.changed = true;
                } else if !item.defer_until.is_empty() {
                    continue;
                }
            }

            // 2. Inspect latest live (uncleaned) run, if any.
            let mut live_state: Option<String> = None;
            let mut live_zero_change_delivered = false;
            if let Some((_, rdir)) = latest_live_run_excluding(item, state, exclude_run) {
                let st = runs::state(&rdir);
                let meta = json(rdir.join("meta.json"));
                let sum = json(rdir.join("summary.json"));
                if runs::active(&st) {
                    if rdir.join("stall-warning.json").is_file() {
                        live_state = Some("attention".into());
                    } else {
                        live_state = Some("active".into());
                    }
                } else {
                    let is_applied = rdir.join(".applied").exists() || rdir.join(".applied-cleaned").exists();
                    let is_question = b(&sum, "question");
                    let review_verdict = {
                        let rf = json(rdir.join("review.json"));
                        if rf.is_object() {
                            s(&rf, "verdict").to_string()
                        } else {
                            s(&sum["review"], "verdict").to_string()
                        }
                    };
                    if is_question
                        || b(&sum, "policyRefused")
                        || review_verdict == "NEEDS_CONTROLLER"
                        || matches!(
                            st.as_str(),
                            "rejected" | "failed" | "malformed" | "timeout" | "stalled" | "killed" | "crashed"
                        )
                    {
                        if !is_applied {
                            live_state = Some("attention".into());
                        }
                    } else if st == "delivered" && !is_applied && s(&meta, "mode") == "write" {
                        if delivered_pending_files(&rdir, &meta, &sum) > 0 {
                            live_state = Some("attention".into());
                        } else {
                            // Zero-change delivery needs no merge; let wait_on take precedence,
                            // otherwise show as 已收取 in verify.
                            live_zero_change_delivered = true;
                        }
                    } else if st == "answered"
                        && !rdir.join(".delivered").exists()
                        && s(&meta, "resultReadAt").is_empty()
                    {
                        live_state = Some("attention".into());
                    } else if is_applied && !item.applied {
                        item.applied = true;
                        any_auto_closed = true;
                        out.changed = true;
                    }
                }
            }

            if let Some(target_st) = live_state {
                if item.set_state(&target_st, &now_iso) {
                    out.changed = true;
                }
                continue;
            }

            // 3. Evaluate `wait_on` conditions
            if !item.wait_on.is_empty() {
                let evals: Vec<CondEval> = item
                    .wait_on
                    .iter()
                    .map(|c| {
                        evaluate_condition_with_prefetch(c, item, &snapshot, state, now_epoch, prefetch)
                    })
                    .collect();
                let all_ok = evals.iter().all(|e| e.satisfied);
                if !all_ok {
                    if item.set_state("blocked", &now_iso) {
                        out.changed = true;
                    }
                    continue;
                }
                // All `wait_on` satisfied!
                if item.state == "blocked" {
                    if let Some(ow) = &item.owner {
                        let target_st = if ow.kind == "direction" {
                            "active"
                        } else {
                            "attention"
                        };
                        if item.set_state(target_st, &now_iso) {
                            out.changed = true;
                        }
                        out.unlocked_ready.push(item.clone());
                    } else if item
                        .auto_dispatch
                        .as_ref()
                        .is_some_and(|ad| ad.dispatched_run.is_empty())
                    {
                        if item.set_state("ready", &now_iso) {
                            out.changed = true;
                        }
                        if compute_fill
                            && WORK_CLASSES.contains(&item.class.as_str())
                            && load_repo_fill_and_hotspots(&item.repo).is_some()
                        {
                            out.unlocked_ready.push(item.clone());
                        } else {
                            out.to_auto_dispatch.push(item.clone());
                        }
                    } else {
                        if item.set_state("ready", &now_iso) {
                            out.changed = true;
                        }
                        out.unlocked_ready.push(item.clone());
                    }
                    continue;
                }
            }

            // 4. If item fell out of deferred or active without an active/attention run:
            if let Some(ow) = &item.owner {
                if item.state == "deferred" {
                    let target_st = if ow.kind == "direction" {
                        "active"
                    } else {
                        "attention"
                    };
                    if item.set_state(target_st, &now_iso) {
                        out.changed = true;
                    }
                }
                continue;
            }
            if matches!(item.state.as_str(), "deferred" | "active" | "attention") {
                let next_st = if live_zero_change_delivered
                    || (item.has_open_closure()
                        && (item.applied || item.cleaned || !item.runs.is_empty()))
                {
                    "verify"
                } else {
                    "ready"
                };
                if item.set_state(next_st, &now_iso) {
                    out.changed = true;
                }
            }

            // 4b. Auto-close `verify` items when all `verify_on` conditions are satisfied
            // and no other structural closure blockers remain.
            // Items in `verify` without `verify_on` never auto-close here.
            if item.state == "verify"
                && !item.verify_on.is_empty()
                && !item.needs_verify
                && item.artifacts.is_empty()
                && item.pending_followups() == 0
                && item.owner.is_none()
            {
                let evals: Vec<CondEval> = item
                    .verify_on
                    .iter()
                    .map(|c| {
                        evaluate_condition_with_prefetch(c, item, &snapshot, state, now_epoch, prefetch)
                    })
                    .collect();
                if evals.iter().all(|e| e.satisfied) {
                    let evidence = evals
                        .iter()
                        .map(|e| format!("{}={}", e.raw, e.detail))
                        .collect::<Vec<_>>()
                        .join("；");
                    item.closed_kind = "auto".into();
                    item.close_evidence = evidence;
                    item.defer_streak = 0;
                    item.set_state("done", &now_iso);
                    out.changed = true;
                    any_auto_closed = true;
                    continue;
                }
            }

            // 5. Stale reminder check (default 4h verify / 8h ready, 6h cooldown per item)
            if matches!(item.state.as_str(), "verify" | "ready") {
                let threshold = if item.state == "verify" {
                    verify_stale_secs
                } else {
                    ready_stale_secs
                };
                if let Some(since_ts) = parse_iso_or_date_epoch(&item.state_since) {
                    let elapsed = (now_epoch - since_ts) as f64;
                    if elapsed >= threshold {
                        let cooled = item.last_reminded_at.is_empty()
                            || parse_iso_or_date_epoch(&item.last_reminded_at)
                                .is_none_or(|t| (now_epoch - t) as f64 >= cooldown_secs);
                        if cooled {
                            let hours = (elapsed / 3600.0).floor() as i64;
                            let reason = if item.state == "verify" {
                                format!("待闭环停滞 {hours}h")
                            } else {
                                format!("就绪未派 {hours}h")
                            };
                            item.last_reminded_at = now_iso.clone();
                            item.updated_at = now_iso.clone();
                            out.changed = true;
                            out.stale_items.push((item.clone(), reason));
                        }
                    }
                }
            }
        }

        if !any_auto_closed {
            break;
        }
    }

    if compute_fill {
        let already: HashSet<String> = out
            .to_auto_dispatch
            .iter()
            .map(|w| w.id.clone())
            .collect();
        out.to_fill_dispatch = select_fill_candidates(state, items, exclude_run, &already);
    }

    out
}

fn fill_claims_path(state: &Path) -> PathBuf {
    state.join("fill-claims.json")
}

fn try_claim_fill_item(state: &Path, work_id: &str) -> bool {
    let Ok(_guard) = crate::locks::acquire_state(state) else {
        return false;
    };
    let items = load_raw(state);
    let Some(w) = items.iter().find(|w| w.id == work_id) else {
        return false;
    };
    if w.state != "ready"
        || !w
            .auto_dispatch
            .as_ref()
            .is_some_and(|ad| ad.dispatched_run.is_empty())
    {
        return false;
    }
    let path = fill_claims_path(state);
    let mut doc = json(&path);
    if !doc.is_object() {
        doc = json!({});
    }
    let now_ep = epoch() as i64;
    let map = doc.as_object_mut().unwrap();
    // Prune stale or dead-PID claims.
    map.retain(|_, v| {
        let pid = v["pid"].as_u64().unwrap_or(0);
        let at = v["epoch"].as_i64().unwrap_or(0);
        pid > 0
            && (now_ep - at) < 120
            && Path::new(&format!("/proc/{pid}")).exists()
    });
    if map.contains_key(work_id) {
        return false;
    }
    map.insert(
        work_id.to_string(),
        json!({"pid": std::process::id(), "epoch": now_ep}),
    );
    let _ = write_json(&path, &doc);
    true
}

fn release_fill_claim(state: &Path, work_id: &str) {
    let Ok(_guard) = crate::locks::acquire_state(state) else {
        return;
    };
    let path = fill_claims_path(state);
    let mut doc = json(&path);
    if let Some(map) = doc.as_object_mut() {
        if map.remove(work_id).is_some() {
            let _ = write_json(&path, &doc);
        }
    }
}

fn emit_reconcile_events_and_dispatch(
    state: &Path,
    outcome: &ReconcileOutcome,
) -> Vec<(WorkItem, Result<String, String>)> {
    for item in &outcome.unlocked_ready {
        events::emit(
            item.latest_run_id().unwrap_or(""),
            &item.repo,
            "work_ready",
            json!({
                "workId": item.id,
                "name": item.title,
                "brief": item.brief,
                "waitOn": item.wait_on,
            }),
        );
    }
    for item in &outcome.defer_expired {
        events::emit(
            item.latest_run_id().unwrap_or(""),
            &item.repo,
            "work_defer_expired",
            json!({
                "workId": item.id,
                "name": item.title,
                "reason": item.defer_reason,
            }),
        );
    }
    for (item, reason) in &outcome.stale_items {
        events::emit(
            item.latest_run_id().unwrap_or(""),
            &item.repo,
            "work_stale",
            json!({
                "workId": item.id,
                "name": item.title,
                "state": item.state,
                "reason": reason,
            }),
        );
    }

    let mut dispatched = Vec::new();
    for item in &outcome.to_auto_dispatch {
        let Some(ad) = &item.auto_dispatch else {
            continue;
        };
        let res = dispatch_work_auto(item, ad);
        match &res {
            Ok(new_run) => {
                // Record dispatchedRun on the work item
                if let Ok(_guard) = crate::locks::acquire_state(state) {
                    let mut items = load_raw(state);
                    if let Some(w) = items.iter_mut().find(|w| w.id == item.id) {
                        if let Some(wad) = w.auto_dispatch.as_mut() {
                            wad.dispatched_run = new_run.clone();
                        }
                        w.add_run(new_run, None, Some(&ad.repo));
                        let now_iso = iso();
                        w.set_state("active", &now_iso);
                        let _ = save_raw(state, &items);
                    }
                }
                events::emit(
                    new_run,
                    &item.repo,
                    "work_dispatched",
                    json!({
                        "workId": item.id,
                        "name": item.title,
                        "nextRun": new_run,
                        "brief": ad.prompt_file,
                    }),
                );
            }
            Err(err) => {
                events::emit(
                    "",
                    &item.repo,
                    "work_ready",
                    json!({
                        "workId": item.id,
                        "name": item.title,
                        "autoDispatchError": err,
                    }),
                );
            }
        }
        dispatched.push((item.clone(), res));
    }

    for cand in &outcome.to_fill_dispatch {
        let item = &cand.item;
        let Some(ad) = &item.auto_dispatch else {
            continue;
        };
        if !try_claim_fill_item(state, &item.id) {
            continue;
        }
        let res = dispatch_work_auto(item, ad);
        match &res {
            Ok(new_run) => {
                if let Ok(_guard) = crate::locks::acquire_state(state) {
                    let mut items = load_raw(state);
                    if let Some(w) = items.iter_mut().find(|w| w.id == item.id) {
                        if let Some(wad) = w.auto_dispatch.as_mut() {
                            wad.dispatched_run = new_run.clone();
                        }
                        w.add_run(new_run, None, Some(&ad.repo));
                        let now_iso = iso();
                        w.set_state("active", &now_iso);
                        let _ = save_raw(state, &items);
                    }
                    // Also remove claim while holding state lock.
                    let cpath = fill_claims_path(state);
                    let mut doc = json(&cpath);
                    if let Some(map) = doc.as_object_mut() {
                        if map.remove(&item.id).is_some() {
                            let _ = write_json(&cpath, &doc);
                        }
                    }
                } else {
                    release_fill_claim(state, &item.id);
                }
                events::emit(
                    new_run,
                    &item.repo,
                    "work_dispatched",
                    json!({
                        "workId": item.id,
                        "name": item.title,
                        "nextRun": new_run,
                        "brief": ad.prompt_file,
                        "reason": "fill",
                        "class": item.class,
                        "activeWriteRuns": cand.active_writes,
                    }),
                );
            }
            Err(err) => {
                release_fill_claim(state, &item.id);
                events::emit(
                    "",
                    &item.repo,
                    "work_ready",
                    json!({
                        "workId": item.id,
                        "name": item.title,
                        "reason": "fill",
                        "autoDispatchError": err,
                    }),
                );
            }
        }
        dispatched.push((item.clone(), res));
    }
    dispatched
}

fn dispatch_work_auto(item: &WorkItem, ad: &AutoDispatch) -> Res<String> {
    let mut cmd = Command::new(script());
    cmd.arg("start")
        .arg("--json")
        .arg("--work")
        .arg(&item.id)
        .arg("--name")
        .arg(clip_chars(&item.title, 24))
        .arg("--prompt-file")
        .arg(&ad.prompt_file)
        .args(&ad.start_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !ad.cwd.is_empty() && Path::new(&ad.cwd).is_dir() {
        cmd.current_dir(&ad.cwd);
    } else if !item.repo.is_empty() && Path::new(&item.repo).is_dir() {
        cmd.current_dir(&item.repo);
    }
    for guard in GUARDS {
        cmd.env_remove(guard);
    }
    if !ad.runs_dir.is_empty() {
        cmd.env("DELEGATE_RUNS", &ad.runs_dir);
    }
    let effective_caller = if !ad.caller.is_empty() {
        ad.caller.as_str()
    } else {
        item.caller.as_str()
    };
    if !effective_caller.is_empty() {
        cmd.env("DELEGATE_CALLER", effective_caller);
    }
    let out = cmd.output().map_err(|e| format!("spawn delegate start: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return Err(format!(
            "delegate start exited {}: {}",
            out.status.code().unwrap_or(1),
            clip(stderr.trim(), 200)
        ));
    }
    for line in stdout.lines() {
        let t = line.trim();
        if t.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<Value>(t) {
                let rid = s(&v, "run");
                if !rid.is_empty() {
                    return Ok(rid.to_string());
                }
            }
        }
    }
    Err("delegate start produced no run JSON".into())
}

/// Lock-free check: does any legacy `notes.json` or `deferred.tsv` still need migration?
fn needs_legacy_migration(state: &Path) -> bool {
    let mig = json(work_migrations_path(state));
    for (repo_root, runs_dir) in registered_repo_runs(state) {
        let notes_file = runs_dir.join("notes.json");
        if notes_file.is_file() {
            let key = format!("notes:{}", notes_file.display());
            if s(&mig, &key) != file_sig(&notes_file) {
                return true;
            }
        }
        let def_file = repo_root.join(".local/delegate/deferred.tsv");
        if def_file.is_file() {
            let key = format!("deferred:{}", def_file.display());
            if s(&mig, &key) != file_sig(&def_file) {
                return true;
            }
        }
    }
    false
}

/// Fast check whether any work item (or legacy notes.json) has followups/notes, so `status`
/// can skip O(N) chain-note lookups on repos with zero notes.
pub fn any_notes_exist() -> bool {
    let state = state_dir();
    if let Ok(content) = fs::read_to_string(work_path(&state)) {
        if content.contains("\"followups\":[{") {
            return true;
        }
    }
    runs_root().join("notes.json").is_file()
}

/// Pure read-only snapshot of work items (strictly zero locks, zero disk writes, zero side effects).
/// Reads the atomically-renamed `work.jsonl` directly, merges any un-migrated legacy `notes.json` /
/// `deferred.tsv` in memory only, and evaluates state transitions in memory.
#[allow(dead_code)]
pub fn load_readonly() -> Vec<WorkItem> {
    load_readonly_in(&state_dir())
}

pub fn load_readonly_json(state: &Path) -> Vec<Value> {
    let items = load_readonly_in(state);
    let now_epoch = epoch() as i64;
    items
        .iter()
        .map(|w| {
            let mut obj = w.to_json();
            obj["repoLabel"] = json!(w.repo_label());
            obj["focus"] = json!(focus_for_item(w, &items, state, now_epoch));
            obj["next"] = json!(next_action_for(w, &items, state));
            if !w.wait_on.is_empty() {
                obj["waitOnStatus"] = json!(w
                    .wait_on
                    .iter()
                    .map(|c| {
                        let ev = evaluate_condition(c, w, &items, state, now_epoch);
                        json!({
                            "condition": ev.raw,
                            "satisfied": ev.satisfied,
                            "detail": ev.detail,
                            "pointer": ev.pointer,
                        })
                    })
                    .collect::<Vec<_>>());
            }
            if !w.verify_on.is_empty() {
                obj["verifyOnStatus"] = json!(w
                    .verify_on
                    .iter()
                    .map(|c| {
                        let ev = evaluate_condition(c, w, &items, state, now_epoch);
                        json!({
                            "condition": ev.raw,
                            "satisfied": ev.satisfied,
                            "detail": ev.detail,
                            "pointer": ev.pointer,
                        })
                    })
                    .collect::<Vec<_>>());
            }
            obj
        })
        .collect()
}

pub fn load_readonly_in(state: &Path) -> Vec<WorkItem> {
    let mut items = load_raw(state);
    if needs_legacy_migration(state) {
        let _ = migrate_legacy_impl(state, &mut items, false);
    }
    let _ = reconcile_all_locked_full(state, &mut items, None, None, false);
    items
}

fn load_synced_in(state: &Path) -> Res<Vec<WorkItem>> {
    // Fast lock-free path: if no legacy migration is pending and no state changed,
    // return the atomically-written snapshot directly without touching `.state.lock`.
    let needs_mig = needs_legacy_migration(state);
    let mut snapshot = load_raw(state);
    let prefetch = prefetch_reconcile_conditions(state, &snapshot, &[]);
    let preview = reconcile_all_locked_with_prefetch(state, &mut snapshot, None, Some(&prefetch));
    if !needs_mig && !preview.changed && preview.to_fill_dispatch.is_empty() {
        return Ok(snapshot);
    }

    fs::create_dir_all(state).map_err(|e| format!("{}: {e}", state.display()))?;
    let guard = crate::locks::acquire_state(state).map_err(|e| e.to_string())?;
    let (items, outcome) = {
        let _guard = guard;
        let mut items = load_raw(state);
        let mig = migrate_legacy_locked(state, &mut items);
        let outcome = reconcile_all_locked_with_prefetch(state, &mut items, None, Some(&prefetch));
        if mig || outcome.changed {
            save_raw(state, &items)?;
        }
        (items, outcome)
    };
    let dispatched = emit_reconcile_events_and_dispatch(state, &outcome);
    if !dispatched.is_empty() {
        return Ok(load_raw(state));
    }
    Ok(items)
}

/// Opportunistic zero-token stale/expiry check (called ONLY from write paths and background workers,
/// never from read paths like `status` / `work` / `snapshot` / TUI).
pub fn check_stale_and_expired() {
    let state = state_dir();
    if !work_path(&state).is_file() {
        return;
    }
    let mut snapshot = load_raw(&state);
    let prefetch = prefetch_reconcile_conditions(&state, &snapshot, &[]);
    let preview = reconcile_all_locked_with_prefetch(&state, &mut snapshot, None, Some(&prefetch));
    if !preview.changed && preview.to_fill_dispatch.is_empty() {
        return;
    }
    let Ok(_guard) = crate::locks::try_acquire_state(&state) else {
        return;
    };
    let outcome = {
        let mut items = load_raw(&state);
        let mig = migrate_legacy_locked(&state, &mut items);
        let outcome =
            reconcile_all_locked_with_prefetch(&state, &mut items, None, Some(&prefetch));
        if mig || outcome.changed {
            let _ = save_raw(&state, &items);
        }
        outcome
    };
    drop(_guard);
    let _ = emit_reconcile_events_and_dispatch(&state, &outcome);
}

// ---------------------------------------------------------------------------
// Lifecycle hooks for start / reply / finish / result / apply / merge / clean
// ---------------------------------------------------------------------------

pub fn validate_work_id(raw: &str) -> Res<WorkItem> {
    let state = state_dir();
    let norm = normalize_work_id(raw).ok_or_else(|| {
        format!("invalid work id '{raw}' (expected W001...; list with: delegate work --all)")
    })?;
    let items = load_readonly_in(&state);
    items.into_iter().find(|w| w.id == norm).ok_or_else(|| {
        format!("work item '{norm}' not found (list with: delegate work --all)")
    })
}

pub fn on_run_started(work_id: &str, run: &Path, meta: &Value) {
    let state = state_dir();
    let Some(norm) = normalize_work_id(work_id) else {
        return;
    };
    let Ok(_guard) = crate::locks::acquire_state(&state) else {
        eprintln!("delegate: work table lock failed on start ({norm})");
        return;
    };
    let mut items = load_raw(&state);
    let _ = migrate_legacy_locked(&state, &mut items);
    let rid = events::run_id_for(run, meta);
    let repo = events::repo_for_meta(meta);
    let now_iso = iso();
    if let Some(w) = items.iter_mut().find(|w| w.id == norm) {
        w.add_run(&rid, Some(run), Some(&repo));
        let run_caller = s(meta, "caller").trim();
        let eff_caller = if !run_caller.is_empty() {
            Some(run_caller.to_string())
        } else {
            caller()
        };
        if let Some(c) = eff_caller {
            w.caller = c;
        }
        w.cleaned = false;
        w.defer_streak = 0;
        w.set_state("active", &now_iso);
        if let Err(err) = save_raw(&state, &items) {
            eprintln!("delegate: work table write failed on start ({norm}): {err}");
        }
    }
}

pub fn on_run_finished(run: &Path, meta: &Value, summary: &Value) {
    let state = state_dir();
    let rid = events::run_id_for(run, meta);
    let work_id = s(meta, "workId").to_string();
    let prefetch = prefetch_reconcile_for_state(&state, &[]);
    let outcome = {
        let Ok(_guard) = crate::locks::acquire_state(&state) else {
            return;
        };
        let mut items = load_raw(&state);
        if items.is_empty() && work_id.is_empty() {
            return;
        }
        let mut changed = migrate_legacy_locked(&state, &mut items);
        let now_iso = iso();
        let st = s(summary, "state");
        let is_question = b(summary, "question");

        if let Some(w) = items
            .iter_mut()
            .find(|w| (!work_id.is_empty() && w.id == work_id) || w.matches_run(&rid))
        {
            let repo = events::repo_for_meta(meta);
            w.add_run(&rid, Some(run), Some(&repo));
            let next_st: Option<&str> = if is_question
                || b(summary, "policyRefused")
                || matches!(
                    st,
                    "rejected" | "failed" | "malformed" | "timeout" | "stalled" | "killed" | "crashed"
                )
                || (st == "delivered"
                    && s(meta, "mode") == "write"
                    && delivered_pending_files(run, meta, summary) > 0)
                || st == "answered"
            {
                Some("attention")
            } else {
                None
            };
            if let Some(ns) = next_st {
                if w.state != "done" && w.set_state(ns, &now_iso) {
                    changed = true;
                }
            }
        }

        let rec = reconcile_all_locked_with_prefetch(&state, &mut items, None, Some(&prefetch));
        if changed || rec.changed {
            let _ = save_raw(&state, &items);
        }
        rec
    };
    let _ = emit_reconcile_events_and_dispatch(&state, &outcome);
}

pub fn on_runs_adopted(run_ids: &[String], work_ids: &[String], new_caller: &str) {
    if new_caller.trim().is_empty() || (run_ids.is_empty() && work_ids.is_empty()) {
        return;
    }
    let state = state_dir();
    if !work_path(&state).is_file() {
        return;
    }
    let Ok(_guard) = crate::locks::acquire_state(&state) else {
        return;
    };
    let mut items = load_raw(&state);
    let mut changed = false;
    let now_iso = iso();
    for w in &mut items {
        if w.state == "done" {
            continue;
        }
        let matched = work_ids.iter().any(|wid| !wid.is_empty() && w.id == *wid)
            || run_ids.iter().any(|rid| !rid.is_empty() && w.matches_run(rid));
        if matched && w.caller != new_caller {
            w.caller = new_caller.to_string();
            w.updated_at = now_iso.clone();
            changed = true;
        }
    }
    if changed {
        let _ = save_raw(&state, &items);
    }
}

pub fn on_run_result_read(run: &Path, meta: &Value) {
    let state = state_dir();
    if !work_path(&state).is_file() {
        return;
    }
    let rid = events::run_id_for(run, meta);
    let work_id = s(meta, "workId").to_string();
    let sum = json(run.join("summary.json"));
    if s(&sum, "state") != "answered"
        || b(&sum, "question")
        || b(&sum, "policyRefused")
        || s(meta, "mode") == "write"
    {
        return;
    }
    let prefetch = prefetch_reconcile_for_state(&state, &[]);
    let outcome = {
        let Ok(_guard) = crate::locks::acquire_state(&state) else {
            return;
        };
        let mut items = load_raw(&state);
        let mut changed = false;
        let now_iso = iso();
        if let Some(w) = items
            .iter_mut()
            .find(|w| (!work_id.is_empty() && w.id == work_id) || w.matches_run(&rid))
        {
            if w.state == "attention" {
                if w.has_open_closure() {
                    w.set_state("verify", &now_iso);
                } else {
                    w.closed_kind = "auto".into();
                    w.close_evidence = format!("read-only answered ({rid})");
                    w.set_state("done", &now_iso);
                }
                changed = true;
            }
        }
        let rec = reconcile_all_locked_with_prefetch(&state, &mut items, None, Some(&prefetch));
        if changed || rec.changed {
            let _ = save_raw(&state, &items);
        }
        rec
    };
    let _ = emit_reconcile_events_and_dispatch(&state, &outcome);
}

fn verify_reasons_with_extra(item: &WorkItem, extra: &[String]) -> Vec<String> {
    let mut parts = Vec::new();
    for r in extra {
        if !r.trim().is_empty() && !parts.contains(r) {
            parts.push(r.clone());
        }
    }
    if item.needs_verify {
        let label = "标记了 --needs-verify".to_string();
        if !parts.contains(&label) {
            parts.push(label);
        }
    }
    if !item.verify_on.is_empty() {
        parts.push(format!("{} 条闭环条件待满足", item.verify_on.len()));
    }
    if !item.artifacts.is_empty() {
        let first = &item.artifacts[0].path;
        if item.artifacts.len() == 1 {
            parts.push(format!("未接线产物 {first}"));
        } else {
            parts.push(format!("未接线产物 {} 等 {} 项", first, item.artifacts.len()));
        }
    }
    let pending = item.pending_followups();
    if pending > 0 {
        parts.push(format!("{pending} 条未完成待办"));
    }
    if parts.is_empty() {
        parts.push("待人工验过闭环".to_string());
    }
    parts
}

/// Walk a run's `parent` chain up to its root run, returning `[leaf_id, parent_id, ..., root_id]`.
/// Strictly read-only, zero locks, zero writes.
pub fn run_chain_ids_from_dir(run_dir: &Path, state: &Path) -> Vec<String> {
    let meta = json(run_dir.join("meta.json"));
    let rid = events::run_id_for(run_dir, &meta);
    let repo_hint = {
        let r = events::repo_for_meta(&meta);
        (!r.is_empty()).then_some(r)
    };
    let runs_parent = run_dir.parent();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    if !rid.is_empty() && seen.insert(rid.clone()) {
        out.push(rid);
    }
    let mut cur_parent = s(&meta, "parent").trim().to_string();
    let empty_dirs = BTreeMap::new();
    for _ in 0..64 {
        if cur_parent.is_empty() || !seen.insert(cur_parent.clone()) {
            break;
        }
        out.push(cur_parent.clone());
        let next_meta = runs_parent
            .map(|p| p.join(&cur_parent).join("meta.json"))
            .filter(|p| p.is_file())
            .map(json)
            .or_else(|| {
                find_live_run_dir(&cur_parent, &empty_dirs, state)
                    .map(|d| json(d.join("meta.json")))
            })
            .or_else(|| find_archived_record(&cur_parent).map(|d| json(d.join("meta.json"))))
            .or_else(|| history_entry_for_run(&cur_parent, state, repo_hint.as_deref()));
        cur_parent = next_meta
            .as_ref()
            .map(|m| s(m, "parent").trim().to_string())
            .unwrap_or_default();
    }
    out
}

/// Walk a run ID's `parent` chain up to its root run without requiring a pre-resolved directory.
/// Strictly read-only, zero locks, zero writes.
pub fn run_chain_ids_for_ref(rid: &str, repo_hint: Option<&str>, state: &Path) -> Vec<String> {
    let trimmed = rid.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let empty_dirs = BTreeMap::new();
    if let Some(dir) = find_run_dir(trimmed, &empty_dirs, state) {
        return run_chain_ids_from_dir(&dir, state);
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut cur = trimmed.to_string();
    for _ in 0..64 {
        if cur.is_empty() || !seen.insert(cur.clone()) {
            break;
        }
        out.push(cur.clone());
        let next_meta = find_archived_record(&cur)
            .map(|d| json(d.join("meta.json")))
            .or_else(|| history_entry_for_run(&cur, state, repo_hint));
        cur = next_meta
            .as_ref()
            .map(|m| s(m, "parent").trim().to_string())
            .unwrap_or_default();
    }
    out
}

/// Compute the sorted, deduplicated list of work item IDs transitively blocked by a run
/// (matching any ID along its `parent` chain as well as any owning work item IDs).
/// Strictly pure in-memory calculation over `items`.
pub fn blocked_downstream_in(
    items: &[WorkItem],
    chain_ids: &[String],
    owning_work_ids: &[String],
) -> Vec<String> {
    if items.is_empty() || (chain_ids.is_empty() && owning_work_ids.is_empty()) {
        return Vec::new();
    }
    let by_id: HashMap<&str, &WorkItem> = items.iter().map(|w| (w.id.as_str(), w)).collect();

    let mut owning_set: HashSet<String> = HashSet::new();
    for wid in owning_work_ids {
        if let Some(norm) = normalize_work_id(wid) {
            owning_set.insert(norm);
        } else if !wid.is_empty() {
            owning_set.insert(wid.clone());
        }
    }
    for w in items {
        if chain_ids.iter().any(|cid| w.matches_run(cid)) {
            owning_set.insert(w.id.clone());
        }
    }

    // Build active blocking adjacency: upstream_work_id -> downstream_work_ids
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for w in items {
        if w.state == "done" {
            continue;
        }
        for cond in &w.wait_on {
            if let Some(raw) = cond.strip_prefix("work_applied:") {
                if let Some(u) = normalize_work_id(raw) {
                    if by_id
                        .get(u.as_str())
                        .is_some_and(|t| !t.applied && t.state != "done")
                    {
                        adj.entry(u).or_default().push(w.id.clone());
                    }
                }
            } else if let Some(raw) = cond.strip_prefix("work_closed:") {
                if let Some(u) = normalize_work_id(raw) {
                    if by_id.get(u.as_str()).is_some_and(|t| t.state != "done") {
                        adj.entry(u).or_default().push(w.id.clone());
                    }
                }
            } else if let Some(raw) = cond.strip_prefix("owner:") {
                if let Some(u) = normalize_work_id(raw) {
                    if by_id.get(u.as_str()).is_some_and(|t| t.state != "done") {
                        adj.entry(u).or_default().push(w.id.clone());
                    }
                }
            } else if let Some(rref) = cond.strip_prefix("run_applied:") {
                for u in items {
                    if u.state == "done" || u.applied || u.id == w.id {
                        continue;
                    }
                    let u_matches = u.runs.iter().any(|r| run_id_matches(r, rref))
                        || (!u.note_root.is_empty() && run_id_matches(&u.note_root, rref));
                    if u_matches {
                        adj.entry(u.id.clone()).or_default().push(w.id.clone());
                    }
                }
            }
        }
    }

    // Seed direct downstream items blocked on this run chain or its owning work items.
    let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    for w in items {
        if w.state == "done" || owning_set.contains(&w.id) {
            continue;
        }
        let directly_blocked = w.wait_on.iter().any(|cond| {
            if let Some(rref) = cond.strip_prefix("run_applied:") {
                return chain_ids.iter().any(|cid| run_id_matches(cid, rref));
            }
            if let Some(raw) = cond.strip_prefix("work_applied:") {
                if let Some(u) = normalize_work_id(raw) {
                    return owning_set.contains(&u)
                        && by_id
                            .get(u.as_str())
                            .is_none_or(|t| !t.applied && t.state != "done");
                }
            }
            if let Some(raw) = cond.strip_prefix("work_closed:") {
                if let Some(u) = normalize_work_id(raw) {
                    return owning_set.contains(&u)
                        && by_id.get(u.as_str()).is_none_or(|t| t.state != "done");
                }
            }
            false
        });
        if directly_blocked && visited.insert(w.id.clone()) {
            queue.push_back(w.id.clone());
        }
    }

    while let Some(cur) = queue.pop_front() {
        if let Some(nexts) = adj.get(&cur) {
            for n in nexts {
                if !owning_set.contains(n) && visited.insert(n.clone()) {
                    queue.push_back(n.clone());
                }
            }
        }
    }

    let mut out: Vec<String> = visited.into_iter().collect();
    out.sort();
    out
}

/// Compute transitive blocked downstream work item IDs for a run directory using a preloaded
/// read-only `items` snapshot (and optional `parent_map` to avoid redundant meta reads).
pub fn blocked_downstream_for_run_with_items(
    items: &[WorkItem],
    run_dir: &Path,
    meta: &Value,
    state: &Path,
    parent_map: Option<&HashMap<String, String>>,
) -> Vec<String> {
    if items.is_empty() {
        return Vec::new();
    }
    let rid = events::run_id_for(run_dir, meta);
    let chain_ids = if let Some(pmap) = parent_map {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        if !rid.is_empty() && seen.insert(rid.clone()) {
            out.push(rid.clone());
        }
        let mut cur = s(meta, "parent").trim().to_string();
        for _ in 0..64 {
            if cur.is_empty() || !seen.insert(cur.clone()) {
                break;
            }
            out.push(cur.clone());
            if let Some(next) = pmap.get(&cur) {
                cur = next.clone();
            } else {
                // Fall back to disk/archive/history for cleaned ancestors.
                let repo_str = events::repo_for_meta(meta);
                let tail = run_chain_ids_for_ref(
                    &cur,
                    (!repo_str.is_empty()).then_some(repo_str.as_str()),
                    state,
                );
                for t in tail.into_iter().skip(1) {
                    if seen.insert(t.clone()) {
                        out.push(t);
                    }
                }
                break;
            }
        }
        out
    } else {
        run_chain_ids_from_dir(run_dir, state)
    };
    let mut owning = Vec::new();
    let wid = s(meta, "workId").trim();
    if !wid.is_empty() {
        owning.push(wid.to_string());
    }
    blocked_downstream_in(items, &chain_ids, &owning)
}

/// Convenience wrapper loading `work.jsonl` read-only for a single run directory.
#[allow(dead_code)]
pub fn blocked_downstream_for_run(run_dir: &Path) -> Vec<String> {
    let state = state_dir();
    if !work_path(&state).is_file() {
        return Vec::new();
    }
    let items = load_readonly_in(&state);
    let meta = json(run_dir.join("meta.json"));
    blocked_downstream_for_run_with_items(&items, run_dir, &meta, &state, None)
}

pub fn ready_command_for(item: &WorkItem) -> String {
    if !item.brief.is_empty() {
        format!("delegate start --work {} --prompt-file {}", item.id, item.brief)
    } else {
        format!(
            "delegate start --work {} {}",
            item.id,
            shell_quote(&clip_chars(&item.title, 36))
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct ApplyWorkOptions {
    pub closes_work_id: Option<String>,
    pub keep_open: bool,
    pub forced: bool,
    /// Run IDs that landed as git commits (`--commit`, `--keep-commits`, or detected in HEAD).
    pub committed_runs: HashSet<String>,
    /// Run IDs whose post-apply verification failed.
    pub verify_failed_runs: HashSet<String>,
}

fn format_apply_evidence(rid: &str, head: &str, committed: bool) -> String {
    let short_head = if head.len() >= 7 { &head[..7] } else { head };
    if committed {
        if short_head.is_empty() {
            format!("applied {rid}")
        } else {
            format!("applied {rid} ({short_head})")
        }
    } else if short_head.is_empty() {
        format!("applied {rid} (working-tree)")
    } else {
        format!("applied {rid} (working-tree @ {short_head})")
    }
}

fn run_rejected_or_failed_reason(
    rid: &str,
    rdir: Option<&Path>,
    verify_failed: bool,
    forced: bool,
) -> Option<String> {
    if verify_failed {
        return Some(format!("合并后验收未通过（{}）", short_run_id(rid)));
    }
    let dir = rdir?;
    let st = runs::state(dir);
    let sum = json(dir.join("summary.json"));
    let sum_st = s(&sum, "state");
    let accept_failed = sum["accept"].is_object() && sum["accept"]["ok"] == json!(false);
    if st == "rejected"
        || sum_st == "rejected"
        || accept_failed
        || b(&sum, "policyRefused")
        || matches!(
            st.as_str(),
            "failed" | "timeout" | "stalled" | "malformed" | "killed" | "crashed"
        )
    {
        let prefix = if forced { "--force 合入了" } else { "合入了" };
        return Some(format!(
            "{prefix}被拒或验收未通过的运行 {}",
            short_run_id(rid)
        ));
    }
    None
}

fn other_active_or_question_run_reasons(
    w: &WorkItem,
    applied_ids: &HashSet<String>,
    extra_runs_dirs: &[PathBuf],
    state: &Path,
) -> Vec<String> {
    let mut reasons = Vec::new();
    let mut checked = HashSet::new();
    let mut inspect_dir = |other_rid: &str, rdir: &Path| {
        if applied_ids.contains(other_rid) || !checked.insert(other_rid.to_string()) {
            return;
        }
        if rdir.join(".applied").exists() || rdir.join(".applied-cleaned").exists() {
            return;
        }
        let st = runs::state(rdir);
        if runs::active(&st) {
            reasons.push(format!("仍有在跑中的运行 {}", short_run_id(other_rid)));
        } else {
            let sum = json(rdir.join("summary.json"));
            if b(&sum, "question") {
                reasons.push(format!("仍有提问中的运行 {}", short_run_id(other_rid)));
            }
        }
    };
    for other_rid in &w.runs {
        if applied_ids.contains(other_rid) {
            continue;
        }
        if let Some(rdir) = find_live_run_dir(other_rid, &w.run_dirs, state) {
            inspect_dir(other_rid, &rdir);
        }
    }
    for parent_dir in extra_runs_dirs {
        for cand in runs::all_runs_in(parent_dir) {
            let m = json(cand.join("meta.json"));
            if s(&m, "workId") != w.id {
                continue;
            }
            let cand_id = events::run_id_for(&cand, &m);
            if !cand_id.is_empty() {
                inspect_dir(&cand_id, &cand);
            }
        }
    }
    reasons
}

/// Called after `delegate apply` succeeds on one or more runs (default options).
pub fn on_runs_applied(
    applied_runs: &[(PathBuf, Value, Value)],
    closes_work_id: Option<&str>,
) -> Vec<String> {
    let mut opts = ApplyWorkOptions {
        closes_work_id: closes_work_id.map(str::to_string),
        ..Default::default()
    };
    for (run_dir, meta, applied_json) in applied_runs {
        if s(applied_json, "appliedBy") == "detected" {
            opts.committed_runs.insert(events::run_id_for(run_dir, meta));
        }
    }
    on_runs_applied_with_options(applied_runs, &opts)
}

/// Called after `delegate apply` succeeds on one or more runs.
/// Returns 0..=4 timely hint lines for stdout tail (each with a Host-written pointer).
pub fn on_runs_applied_with_options(
    applied_runs: &[(PathBuf, Value, Value)],
    opts: &ApplyWorkOptions,
) -> Vec<String> {
    let state = state_dir();
    let closes_norm = opts
        .closes_work_id
        .as_deref()
        .and_then(normalize_work_id);
    let mut hints: Vec<String> = Vec::new();

    struct EffectiveRun {
        rid: String,
        wid: String,
        rdir: Option<PathBuf>,
        head: String,
        chain_ids: Vec<String>,
        committed: bool,
        verify_failed: bool,
    }

    // Expand merged source runs and parent chains so leaf applies match root-linked work items.
    let mut effective_runs: Vec<EffectiveRun> = Vec::new();
    let mut all_applied_ids: HashSet<String> = HashSet::new();
    let mut extra_runs_dirs: Vec<PathBuf> = Vec::new();
    for (run_dir, meta, applied_json) in applied_runs {
        let rid = events::run_id_for(run_dir, meta);
        let head = s(applied_json, "head").to_string();
        let wid = s(meta, "workId").to_string();
        let chain_ids = run_chain_ids_from_dir(run_dir, &state);
        for cid in &chain_ids {
            all_applied_ids.insert(cid.clone());
        }
        all_applied_ids.insert(rid.clone());
        if let Some(p) = run_dir.parent() {
            let pb = p.to_path_buf();
            if !extra_runs_dirs.contains(&pb) {
                extra_runs_dirs.push(pb);
            }
        }
        let committed = opts.committed_runs.contains(&rid)
            || s(applied_json, "appliedBy") == "detected";
        let verify_failed = opts.verify_failed_runs.contains(&rid);
        effective_runs.push(EffectiveRun {
            rid: rid.clone(),
            wid,
            rdir: Some(run_dir.clone()),
            head: head.clone(),
            chain_ids,
            committed,
            verify_failed,
        });

        if b(meta, "isMerge") {
            let report = json(run_dir.join("merge-report.json"));
            let parent_runs = run_dir.parent().unwrap_or(Path::new("/"));
            for item in report["items"].as_array().into_iter().flatten() {
                if s(item, "status") != "MERGED" {
                    continue;
                }
                let src_id = s(item, "run");
                if src_id.is_empty() {
                    continue;
                }
                let src_dir = parent_runs.join(src_id);
                let src_meta = json(src_dir.join("meta.json"));
                let src_wid = s(&src_meta, "workId").to_string();
                let src_chain = if src_dir.is_dir() {
                    run_chain_ids_from_dir(&src_dir, &state)
                } else {
                    run_chain_ids_for_ref(src_id, None, &state)
                };
                for cid in &src_chain {
                    all_applied_ids.insert(cid.clone());
                }
                all_applied_ids.insert(src_id.to_string());
                effective_runs.push(EffectiveRun {
                    rid: src_id.to_string(),
                    wid: src_wid,
                    rdir: src_dir.is_dir().then_some(src_dir),
                    head: head.clone(),
                    chain_ids: src_chain,
                    committed,
                    verify_failed,
                });
            }
        }
    }

    let prefetch = prefetch_reconcile_for_state(&state, &[]);
    let (outcome, verify_stays, auto_closed) = {
        let Ok(_guard) = crate::locks::acquire_state(&state) else {
            return hints;
        };
        let mut items = load_raw(&state);
        if items.is_empty() && closes_norm.is_none() {
            return hints;
        }
        let mut changed = migrate_legacy_locked(&state, &mut items);
        let now_iso = iso();
        let mut stays: Vec<(WorkItem, Vec<String>)> = Vec::new();
        let mut closed: Vec<WorkItem> = Vec::new();
        let mut handled_work_ids: HashSet<String> = HashSet::new();

        for er in &effective_runs {
            let Some(pos) = items.iter().position(|w| {
                (!er.wid.is_empty() && w.id == er.wid)
                    || er.chain_ids.iter().any(|cid| w.matches_run(cid))
            }) else {
                continue;
            };
            let w = &mut items[pos];
            w.applied = true;
            w.defer_streak = 0;
            if let Some(dir) = &er.rdir {
                w.add_run(&er.rid, Some(dir), None);
            }
            changed = true;
            if !handled_work_ids.insert(w.id.clone()) {
                continue;
            }
            let explicit_close = closes_norm.as_deref() == Some(w.id.as_str());
            let evidence = format_apply_evidence(&er.rid, &er.head, er.committed);

            if explicit_close {
                w.closed_kind = "closed".into();
                w.close_evidence = evidence;
                w.set_state("done", &now_iso);
                closed.push(w.clone());
                continue;
            }

            // Collect all structured reasons that prevent automatic closure.
            let mut block_reasons: Vec<String> = Vec::new();
            if w.owner.is_some() {
                block_reasons.push("所有者项需人工闭环".to_string());
            }
            if opts.keep_open {
                block_reasons.push("指定了 apply --keep-open".to_string());
            }
            if let Some(rej_reason) = run_rejected_or_failed_reason(
                &er.rid,
                er.rdir.as_deref(),
                er.verify_failed,
                opts.forced,
            ) {
                block_reasons.push(rej_reason);
            }
            let active_reasons = other_active_or_question_run_reasons(
                w,
                &all_applied_ids,
                &extra_runs_dirs,
                &state,
            );
            block_reasons.extend(active_reasons);

            if !w.has_open_closure() && block_reasons.is_empty() {
                w.closed_kind = "auto".into();
                w.close_evidence = evidence;
                w.set_state("done", &now_iso);
                closed.push(w.clone());
            } else {
                if w.owner.is_none() {
                    w.set_state("verify", &now_iso);
                }
                let full_reasons = verify_reasons_with_extra(w, &block_reasons);
                stays.push((w.clone(), full_reasons));
            }
        }

        // Handle explicit `--closes <Wxx>` even if the applied run wasn't pre-linked to Wxx
        if let Some(cid) = &closes_norm {
            if !closed.iter().any(|w| w.id == *cid) {
                if let Some(w) = items.iter_mut().find(|w| w.id == *cid) {
                    let first_rid = effective_runs
                        .first()
                        .map(|e| e.rid.as_str())
                        .unwrap_or("");
                    let first_head = effective_runs
                        .first()
                        .map(|e| e.head.as_str())
                        .unwrap_or("");
                    let first_committed = effective_runs
                        .first()
                        .is_some_and(|e| e.committed);
                    w.applied = true;
                    w.defer_streak = 0;
                    w.closed_kind = "closed".into();
                    w.close_evidence = format_apply_evidence(first_rid, first_head, first_committed);
                    w.set_state("done", &now_iso);
                    closed.push(w.clone());
                    changed = true;
                }
            }
        }

        let rec = reconcile_all_locked_with_prefetch(&state, &mut items, None, Some(&prefetch));
        let mut final_stays: Vec<(WorkItem, Vec<String>)> = Vec::new();
        for (s_item, reasons) in stays {
            if let Some(cur) = items.iter().find(|w| w.id == s_item.id) {
                if cur.state == "done" {
                    if !closed.iter().any(|c| c.id == cur.id) {
                        closed.push(cur.clone());
                    }
                } else {
                    final_stays.push((cur.clone(), reasons));
                }
            }
        }
        if changed || rec.changed {
            let _ = save_raw(&state, &items);
        }
        (rec, final_stays, closed)
    };

    let dispatched = emit_reconcile_events_and_dispatch(&state, &outcome);

    // 1. Unlocked (ready) downstream items
    for w in outcome.unlocked_ready.iter().take(3) {
        let cmd = ready_command_for(w);
        hints.push(format!(
            "已解锁 [{}] {} → {}（卡片：delegate work show {}）",
            w.id,
            clip_chars(&w.title, 30),
            cmd,
            w.id
        ));
    }

    // 2. Auto-dispatched downstream items
    for (w, res) in dispatched.iter().take(3) {
        match res {
            Ok(new_run) => {
                hints.push(format!(
                    "已自动派发 [{}] {} -> {}（卡片：delegate work show {}）",
                    w.id,
                    clip_chars(&w.title, 28),
                    new_run,
                    w.id
                ));
            }
            Err(err) => {
                hints.push(format!(
                    "自动派发失败 [{}] {}（{}）→ {}（卡片：delegate work show {}）",
                    w.id,
                    clip_chars(&w.title, 24),
                    clip_chars(err, 40),
                    ready_command_for(w),
                    w.id
                ));
            }
        }
    }

    // 3. Work item stayed open (`verify` or kept `active`/`attention` due to other runs/owner)
    for (w, reasons) in verify_stays.iter().take(2) {
        let why = reasons.join("；");
        if w.state == "verify" {
            hints.push(format!(
                "工作项 [{}] 留在 verify（{}）· 闭环：delegate work close {} --evidence \"…\"（卡片：delegate work show {}）",
                w.id, why, w.id, w.id
            ));
        } else {
            hints.push(format!(
                "工作项 [{}] 未自动关闭，保持 {}（{}）（卡片：delegate work show {}）",
                w.id, w.state, why, w.id
            ));
        }
    }

    // 4. Closed / auto-closed work items
    for w in auto_closed.iter().take(2) {
        let verb = if w.closed_kind == "auto" {
            "已自动关闭工作项"
        } else {
            "已关闭工作项"
        };
        hints.push(format!(
            "{verb} [{}] {}（{} · 卡片：delegate work show {}）",
            w.id,
            clip_chars(&w.title, 30),
            w.close_evidence,
            w.id
        ));
    }

    hints.truncate(4);
    hints
}

/// Timely tail hints for `delegate wait` / `wait --since` (1–4 lines, each with pointer).
pub fn hints_for_wait(settled_runs: &[PathBuf]) -> Vec<String> {
    let state = state_dir();
    if settled_runs.is_empty() || !work_path(&state).is_file() {
        return Vec::new();
    }
    let items = load_readonly_in(&state);
    if items.is_empty() {
        return Vec::new();
    }

    let mut hints = Vec::new();
    let mut seen_work = HashSet::new();

    for run_dir in settled_runs {
        let meta = json(run_dir.join("meta.json"));
        let rid = events::run_id_for(run_dir, &meta);
        let wid = s(&meta, "workId");
        let chain_ids = run_chain_ids_from_dir(run_dir, &state);

        // 1. Owning work item (1 line, matching across the full parent chain)
        let mut owning_wids: HashSet<String> = HashSet::new();
        if !wid.is_empty() {
            owning_wids.insert(wid.to_string());
        }
        if let Some(w) = items.iter().find(|w| {
            (!wid.is_empty() && w.id == wid) || chain_ids.iter().any(|cid| w.matches_run(cid))
        }) {
            owning_wids.insert(w.id.clone());
            if seen_work.insert(w.id.clone()) {
                let ptr = if !w.brief.is_empty() {
                    format!("卡片：delegate work show {} · 说明：{}", w.id, w.brief)
                } else {
                    format!("卡片：delegate work show {}", w.id)
                };
                hints.push(format!(
                    "所属工作项：[{}] {}（{} · {}）",
                    w.id,
                    clip_chars(&w.title, 30),
                    w.state,
                    ptr
                ));
            }
        }

        // 2. Downstream blocked/ready items waiting on this run (or any ancestor in parent chain / owning work)
        let mut downstream_count = 0usize;
        for w in &items {
            if w.state == "done" || owning_wids.contains(&w.id) || downstream_count >= 3 {
                continue;
            }
            let waiting_applied = w.wait_on.iter().any(|c| {
                c.strip_prefix("run_applied:")
                    .is_some_and(|r| chain_ids.iter().any(|cid| run_id_matches(cid, r)))
                    || c.strip_prefix("work_applied:")
                        .and_then(normalize_work_id)
                        .is_some_and(|u| owning_wids.contains(&u))
                    || c.strip_prefix("work_closed:")
                        .and_then(normalize_work_id)
                        .is_some_and(|u| owning_wids.contains(&u))
            });
            let waiting_delivered = w.wait_on.iter().any(|c| {
                c.strip_prefix("run_delivered:")
                    .is_some_and(|r| chain_ids.iter().any(|cid| run_id_matches(cid, r)))
            });
            if !waiting_applied && !waiting_delivered {
                continue;
            }
            if !seen_work.insert(format!("down:{}", w.id)) {
                continue;
            }
            downstream_count += 1;
            if waiting_applied {
                hints.push(format!(
                    "关联下游：[{}] {}（等 {} 合入后解锁 · 卡片：delegate work show {}）",
                    w.id,
                    clip_chars(&w.title, 28),
                    rid,
                    w.id
                ));
            } else if w.state == "ready" {
                hints.push(format!(
                    "关联下游已就绪：[{}] {} → {}（卡片：delegate work show {}）",
                    w.id,
                    clip_chars(&w.title, 26),
                    ready_command_for(w),
                    w.id
                ));
            } else {
                hints.push(format!(
                    "关联下游：[{}] {}（{} · 卡片：delegate work show {}）",
                    w.id,
                    clip_chars(&w.title, 28),
                    w.state,
                    w.id
                ));
            }
        }
    }

    hints.truncate(4);
    hints
}

/// Timely tail hints for `delegate wait --since` based on returned events.
pub fn hints_for_events(evs: &[Value]) -> Vec<String> {
    let state = state_dir();
    if evs.is_empty() || !work_path(&state).is_file() {
        return Vec::new();
    }
    let items = load_readonly_in(&state);
    if items.is_empty() {
        return Vec::new();
    }
    let mut hints = Vec::new();
    let mut seen = HashSet::new();
    for ev in evs {
        let kind = s(ev, "kind");
        let rid = s(ev, "run");
        if matches!(kind, "work_ready" | "work_defer_expired" | "work_stale") {
            let wid = s(&ev["payload"], "workId");
            if let Some(w) = items.iter().find(|w| w.id == wid) {
                if seen.insert(w.id.clone()) {
                    hints.push(format!(
                        "工作项 [{}] {}（{}）→ {}（卡片：delegate work show {}）",
                        w.id,
                        clip_chars(&w.title, 28),
                        w.state,
                        next_action_for(w, &items, &state),
                        w.id
                    ));
                }
            }
            continue;
        }
        if !matches!(kind, "delivered" | "question" | "rejected" | "failed") || rid.is_empty() {
            continue;
        }
        let repo_hint = {
            let r = s(ev, "repo");
            (!r.is_empty()).then_some(r)
        };
        let chain_ids = run_chain_ids_for_ref(rid, repo_hint, &state);
        let owning_wids: HashSet<String> = items
            .iter()
            .filter(|w| chain_ids.iter().any(|cid| w.matches_run(cid)))
            .map(|w| w.id.clone())
            .collect();
        for w in &items {
            if w.state == "done" || owning_wids.contains(&w.id) {
                continue;
            }
            let matches_down = w.wait_on.iter().any(|c| {
                c.strip_prefix("run_applied:")
                    .is_some_and(|r| chain_ids.iter().any(|cid| run_id_matches(cid, r)))
                    || c.strip_prefix("work_applied:")
                        .and_then(normalize_work_id)
                        .is_some_and(|u| owning_wids.contains(&u))
                    || c.strip_prefix("work_closed:")
                        .and_then(normalize_work_id)
                        .is_some_and(|u| owning_wids.contains(&u))
            });
            if matches_down && seen.insert(format!("down:{}", w.id)) {
                hints.push(format!(
                    "关联下游：[{}] {}（等 {} 合入后解锁 · 卡片：delegate work show {}）",
                    w.id,
                    clip_chars(&w.title, 28),
                    rid,
                    w.id
                ));
            }
        }
    }
    hints.truncate(4);
    hints
}

/// Timely tail hints for `delegate merge` output.
pub fn hints_for_merge(merge_run_id: &str, report: &Value) -> Vec<String> {
    let state = state_dir();
    if !work_path(&state).is_file() {
        return Vec::new();
    }
    let items = load_readonly_in(&state);
    if items.is_empty() {
        return Vec::new();
    }
    let merged_runs: Vec<String> = report["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|it| s(it, "status") == "MERGED")
        .map(|it| s(it, "run").to_string())
        .filter(|r| !r.is_empty())
        .collect();
    if merged_runs.is_empty() {
        return Vec::new();
    }
    let mut expanded_runs: Vec<String> = Vec::new();
    for mr in &merged_runs {
        for cid in run_chain_ids_for_ref(mr, None, &state) {
            if !expanded_runs.contains(&cid) {
                expanded_runs.push(cid);
            }
        }
    }
    let merged_wids: HashSet<String> = items
        .iter()
        .filter(|w| expanded_runs.iter().any(|cid| w.matches_run(cid)))
        .map(|w| w.id.clone())
        .collect();
    let report_path = s(report, "reportPath");
    let mut hints = Vec::new();
    let mut seen = HashSet::new();

    for w in &items {
        if w.state == "done" || merged_wids.contains(&w.id) {
            continue;
        }
        let unlocks = w.wait_on.iter().any(|c| {
            c.strip_prefix("run_applied:")
                .is_some_and(|r| expanded_runs.iter().any(|mr| run_id_matches(mr, r)))
                || c.strip_prefix("work_applied:")
                    .and_then(normalize_work_id)
                    .is_some_and(|u| merged_wids.contains(&u))
                || c.strip_prefix("work_closed:")
                    .and_then(normalize_work_id)
                    .is_some_and(|u| merged_wids.contains(&u))
        });
        if unlocks && seen.insert(w.id.clone()) {
            hints.push(format!(
                "合入后将解锁 [{}] {} → delegate apply --keep-commits {}（卡片：delegate work show {} · 报告：{}）",
                w.id,
                clip_chars(&w.title, 24),
                merge_run_id,
                w.id,
                report_path
            ));
        }
    }
    hints.truncate(3);
    hints
}

/// Called by `clean` when removing a run directory.
/// Updates `cleaned` flag and returns 1 confirmation line (<= 80 chars) if the work item
/// remains active/verify/open across clean. NEVER blocks clean, NEVER emits Stop hook alert.
pub fn on_run_cleaned(run: &Path, meta: &Value) -> Option<String> {
    let state = state_dir();
    if !work_path(&state).is_file() {
        return None;
    }
    let rid = events::run_id_for(run, meta);
    let wid = s(meta, "workId").to_string();
    let parent_dir = run.parent();

    let prefetch = prefetch_reconcile_for_state(&state, &[]);
    let (snapshot, outcome) = {
        let Ok(_guard) = crate::locks::acquire_state(&state) else {
            return None;
        };
        let mut items = load_raw(&state);
        let now_iso = iso();
        let is_applied = run.join(".applied").exists()
            || run.join(".applied-cleaned").exists()
            || !s(meta, "appliedAt").is_empty();
        let mut snap: Option<WorkItem> = None;
        let mut changed = false;
        let mut target_pos: Option<usize> = None;
        if let Some(pos) = items
            .iter()
            .position(|w| (!wid.is_empty() && w.id == wid) || w.matches_run(&rid))
        {
            target_pos = Some(pos);
            let entry = &mut items[pos];
            if is_applied {
                entry.applied = true;
            }
            let others_live = entry.runs.iter().any(|other| {
                other != &rid
                    && parent_dir.is_some_and(|p| {
                        let cand = p.join(other);
                        cand.join("meta.json").is_file() && !is_archived_run_dir(&cand)
                    })
            });
            if !others_live {
                entry.cleaned = true;
            }
            entry.updated_at = now_iso;
            changed = true;
        }
        // Reconcile immediately treating `rid` as already cleaned (before trash rename),
        // so cleaned runs never keep a work item stuck in `attention` and applied runs
        // unlock downstream `run_applied:<rid>` dependencies right away.
        let outcome = if is_applied || target_pos.is_some() {
            let o = reconcile_all_locked_with_prefetch(
                &state,
                &mut items,
                Some(&rid),
                Some(&prefetch),
            );
            if o.changed {
                changed = true;
            }
            Some(o)
        } else {
            None
        };
        if let Some(pos) = target_pos {
            snap = Some(items[pos].clone());
        }
        if changed {
            let _ = save_raw(&state, &items);
        }
        (snap, outcome)
    };
    if let Some(o) = &outcome {
        let _ = emit_reconcile_events_and_dispatch(&state, o);
    }

    if let Some(snapshot) = snapshot {
        if snapshot.state != "done" {
            return Some(format!(
                "工作项保留：[{}] {}（{} · 卡片：delegate work show {}）",
                snapshot.id,
                clip_chars(&snapshot.title, 22),
                snapshot.state,
                snapshot.id
            ));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Thin-shell backend for `delegate note` (reads/writes work.jsonl, zero notes.json writes)
// ---------------------------------------------------------------------------

fn work_item_to_chain_notes(w: &WorkItem, all_runs_in_repo: &[PathBuf]) -> Option<notes::ChainNotes> {
    if w.followups.is_empty() {
        return None;
    }
    let root = if !w.note_root.is_empty() {
        w.note_root.clone()
    } else {
        w.runs.first().cloned().unwrap_or_else(|| w.id.clone())
    };
    let latest_run = w
        .runs
        .last()
        .cloned()
        .unwrap_or_else(|| root.clone());
    let live_ids: HashSet<String> = all_runs_in_repo
        .iter()
        .map(|p| notes::run_id(p, &json(p.join("meta.json"))))
        .collect();
    let has_live = w.runs.iter().any(|r| live_ids.contains(r));
    let items: Vec<notes::NoteItem> = w
        .followups
        .iter()
        .map(|f| notes::NoteItem {
            text: f.text.clone(),
            created_at: f.created_at.clone(),
            done: f.done,
            done_at: f.done_at.clone(),
        })
        .collect();
    Some(notes::ChainNotes {
        root,
        runs: w.runs.clone(),
        name: if w.title.is_empty() {
            latest_run.clone()
        } else {
            w.title.clone()
        },
        latest_run,
        cleaned: w.cleaned || !has_live,
        applied: w.applied,
        items,
    })
}

fn item_belongs_to_runs_root(w: &WorkItem, root: &Path) -> bool {
    let root_canon = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    for p in w.run_dirs.values() {
        if let Some(parent) = Path::new(p).parent() {
            let pc = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
            if pc == root_canon {
                return true;
            }
        }
    }
    let cur_repo = current_repo_str();
    if !cur_repo.is_empty() && !w.repo.is_empty() && w.repo == cur_repo {
        return true;
    }
    // Check if root is `<w.repo>/.local/run/delegate`
    if !w.repo.is_empty() {
        let candidate = Path::new(&w.repo).join(".local/run/delegate");
        let cc = fs::canonicalize(&candidate).unwrap_or(candidate);
        if cc == root_canon {
            return true;
        }
    }
    // Fallback: if run dirs are empty and repo is empty
    w.run_dirs.is_empty() && w.repo.is_empty()
}

fn load_items_for_notes(state: &Path) -> Vec<WorkItem> {
    let mut items = load_raw(state);
    if items.is_empty() && needs_legacy_migration(state) {
        let _ = migrate_legacy_impl(state, &mut items, false);
    }
    items
}

pub fn note_chains_for_current_root(all: &[PathBuf]) -> Vec<notes::ChainNotes> {
    let root = runs_root();
    let items = load_items_for_notes(&state_dir());
    items
        .iter()
        .filter(|w| !w.followups.is_empty() && item_belongs_to_runs_root(w, &root))
        .filter_map(|w| work_item_to_chain_notes(w, all))
        .collect()
}

pub fn note_chain_for_run(run: &Path, all: &[PathBuf]) -> Option<notes::ChainNotes> {
    let (_, _, members, _) = notes::chain_info_for(run, all);
    let items = load_items_for_notes(&state_dir());
    items
        .iter()
        .find(|w| !w.followups.is_empty() && members.iter().any(|m| w.matches_run(m)))
        .and_then(|w| work_item_to_chain_notes(w, all))
}

pub fn note_add_for_run(
    run_opt: Option<&Path>,
    cleaned_run_id: Option<&str>,
    text: &str,
    all: &[PathBuf],
) -> Res<(notes::ChainNotes, usize)> {
    let state = state_dir();
    fs::create_dir_all(&state).map_err(|e| e.to_string())?;
    let _guard = crate::locks::acquire_state(&state).map_err(|e| e.to_string())?;
    let mut items = load_raw(&state);
    let _ = migrate_legacy_locked(&state, &mut items);
    let now_iso = iso();

    let (pos, latest_id) = if let Some(run) = run_opt {
        let (root_id, chain_name, members, latest_run) = notes::chain_info_for(run, all);
        let meta = json(run.join("meta.json"));
        let wid = s(&meta, "workId");
        let repo = events::repo_for_meta(&meta);
        let applied = members.iter().any(|id| {
            all.iter().any(|p| {
                notes::run_id(p, &json(p.join("meta.json"))) == *id
                    && (p.join(".applied").exists() || p.join(".applied-cleaned").exists())
            })
        }) || run.join(".applied").exists()
            || run.join(".applied-cleaned").exists();

        let existing = items.iter().position(|w| {
            (!wid.is_empty() && w.id == wid)
                || (!w.note_root.is_empty() && w.note_root == root_id)
                || members.iter().any(|m| w.matches_run(m))
        });
        let idx = if let Some(p) = existing {
            p
        } else {
            let id = next_id_locked(&state, &items);
            let w = WorkItem {
                id,
                title: chain_name.clone(),
                repo: repo.clone(),
                state: if applied {
                    "verify".into()
                } else {
                    "attention".into()
                },
                created_at: now_iso.clone(),
                updated_at: now_iso.clone(),
                state_since: now_iso.clone(),
                origin: "note".into(),
                note_root: root_id.clone(),
                applied,
                cleaned: false,
                ..Default::default()
            };
            items.push(w);
            items.len() - 1
        };
        let entry = &mut items[idx];
        if entry.note_root.is_empty() {
            entry.note_root = root_id;
        }
        if entry.title.is_empty() {
            entry.title = chain_name;
        }
        entry.applied |= applied;
        entry.cleaned = false;
        for m in &members {
            let rdir = all
                .iter()
                .find(|p| notes::run_id(p, &json(p.join("meta.json"))) == *m)
                .map(PathBuf::as_path)
                .or_else(|| (notes::run_id(run, &meta) == *m).then_some(run));
            entry.add_run(m, rdir, Some(&repo));
        }
        (idx, latest_run)
    } else if let Some(cid) = cleaned_run_id {
        let idx = items
            .iter()
            .position(|w| w.matches_run(cid))
            .ok_or_else(|| format!("run '{cid}' not found"))?;
        (idx, cid.to_string())
    } else {
        return Err("missing target run".into());
    };

    let entry = &mut items[pos];
    let next_fid = entry.followups.iter().map(|f| f.id).max().unwrap_or(0) + 1;
    entry.followups.push(FollowupItem {
        id: next_fid,
        kind: "note".into(),
        text: text.to_string(),
        anchor: String::new(),
        source_run: latest_id,
        done: false,
        created_at: now_iso.clone(),
        done_at: None,
    });
    let added_index = entry.followups.len();
    if entry.state == "done" {
        entry.set_state("verify", &now_iso);
    } else {
        entry.updated_at = now_iso;
    }
    let chain = work_item_to_chain_notes(entry, all).unwrap();
    save_raw(&state, &items)?;
    drop(_guard);
    refresh_index_for_chain(&chain, run_opt, all);
    Ok((chain, added_index))
}

pub fn note_done_for_run(
    run_opt: Option<&Path>,
    cleaned_run_id: Option<&str>,
    all_flag: bool,
    index_args: &[String],
    all: &[PathBuf],
) -> Res<Option<notes::ChainNotes>> {
    let state = state_dir();
    fs::create_dir_all(&state).map_err(|e| e.to_string())?;
    let _guard = crate::locks::acquire_state(&state).map_err(|e| e.to_string())?;
    let mut items = load_raw(&state);
    let _ = migrate_legacy_locked(&state, &mut items);
    let now_iso = iso();

    let pos = if let Some(run) = run_opt {
        let (root_id, _, members, _) = notes::chain_info_for(run, all);
        items.iter().position(|w| {
            !w.followups.is_empty()
                && ((!w.note_root.is_empty() && w.note_root == root_id)
                    || members.iter().any(|m| w.matches_run(m)))
        })
    } else if let Some(cid) = cleaned_run_id {
        items
            .iter()
            .position(|w| !w.followups.is_empty() && w.matches_run(cid))
    } else {
        None
    };

    let Some(pos) = pos else {
        return Ok(None);
    };

    let entry = &mut items[pos];
    let pending_indices: Vec<usize> = entry
        .followups
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.done)
        .map(|(i, _)| i)
        .collect();

    if pending_indices.is_empty() {
        let chain = work_item_to_chain_notes(entry, all).unwrap();
        return Ok(Some(chain));
    }

    let to_mark: Vec<usize> = if all_flag {
        pending_indices
    } else if !index_args.is_empty() {
        let total = entry.followups.len();
        let mut chosen = Vec::new();
        for raw in index_args {
            let idx = raw.parse::<usize>().map_err(|_| {
                format!("invalid note index '{raw}': expected integer 1..={total} or --all")
            })?;
            if idx == 0 || idx > total {
                return Err(format!("note index {idx} out of range (valid: 1..={total})"));
            }
            chosen.push(idx - 1);
        }
        chosen
    } else if pending_indices.len() == 1 {
        pending_indices
    } else {
        let latest = entry
            .latest_run_id()
            .unwrap_or(entry.id.as_str())
            .to_string();
        let listing = entry
            .followups
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let mark = if f.done { "x" } else { " " };
                format!("  {}. [{}] {} ({})", i + 1, mark, f.text, f.created_at)
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "{} 有 {} 条未完成待办，请指定序号（如 delegate note {} --done 1）或 --all：\n{listing}",
            entry.title,
            pending_indices.len(),
            latest
        ));
    };

    for idx in to_mark {
        if !entry.followups[idx].done {
            entry.followups[idx].done = true;
            entry.followups[idx].done_at = Some(now_iso.clone());
        }
    }
    entry.updated_at = now_iso.clone();
    if !entry.has_open_closure() && (entry.applied || entry.cleaned) {
        entry.closed_kind = "auto".into();
        entry.close_evidence = "all notes done".into();
        entry.set_state("done", &now_iso);
    }
    let chain = work_item_to_chain_notes(entry, all).unwrap();
    save_raw(&state, &items)?;
    drop(_guard);
    refresh_index_for_chain(&chain, run_opt, all);
    Ok(Some(chain))
}

fn refresh_index_for_chain(chain: &notes::ChainNotes, run_opt: Option<&Path>, all: &[PathBuf]) {
    let mut updated = HashSet::new();
    for p in all {
        let rid = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if chain.runs.iter().any(|r| r == rid) && updated.insert(p.clone()) {
            crate::index::upsert_run(p);
        }
    }
    if let Some(r) = run_opt {
        if updated.insert(r.to_path_buf()) {
            crate::index::upsert_run(r);
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering: Layer 0 (summary) + Layer 1 (6-col table) + Layer 2 (card)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct TableRow {
    id: String,
    repo: String,
    title: String,
    state: String,
    focus: String,
    next: String,
    is_ephemeral: bool,
    run_id: String,
    missing_class: bool,
}

fn state_priority(st: &str) -> u8 {
    match st {
        "attention" => 0,
        "ready" => 1,
        "verify" => 2,
        "active" => 3,
        "blocked" => 4,
        "deferred" => 5,
        "done" => 6,
        _ => 7,
    }
}

fn focus_for_item(item: &WorkItem, all_items: &[WorkItem], state: &Path, now_epoch: i64) -> String {
    let base = focus_for_item_core(item, all_items, state, now_epoch);
    if item.defer_streak >= 3 && item.state != "done" {
        format!("反复暂缓 · {base}")
    } else {
        base
    }
}

fn focus_for_item_core(item: &WorkItem, all_items: &[WorkItem], state: &Path, now_epoch: i64) -> String {
    if let Some(ow) = &item.owner {
        if item.state == "done" {
            return if item.closed_kind == "dropped" {
                format!("已否：{}", clip_chars(&item.close_reason, 20))
            } else if !item.close_evidence.is_empty() {
                format!("已定：{}", clip_chars(&item.close_evidence, 20))
            } else {
                "已定".into()
            };
        }
        let kl = OwnerInfo::kind_label(&ow.kind);
        if !ow.recommendation.is_empty() {
            return format!("{kl} · 建议：{}", clip_chars(&ow.recommendation, 20));
        }
        if !ow.cost.is_empty() {
            return format!("{kl} · {}", clip_chars(&ow.cost, 20));
        }
        return format!("等所有者（{kl}）");
    }
    match item.state.as_str() {
        "active" => {
            if let Some((rid, rdir)) = latest_live_run(item, state) {
                let short = short_run_id(&rid);
                let meta = json(rdir.join("meta.json"));
                let elapsed = format_age(s(&meta, "startedAt"), now_epoch);
                let st = runs::state(&rdir);
                if st == "waiting" {
                    format!("run {short} 排队中 {elapsed}")
                } else {
                    format!("run {short} 运行中 {elapsed}")
                }
            } else {
                "推进中".into()
            }
        }
        "attention" => {
            if let Some((rid, rdir)) = latest_live_run(item, state) {
                let st = runs::state(&rdir);
                let sum = json(rdir.join("summary.json"));
                if rdir.join("stall-warning.json").is_file() {
                    return format!("run {} 卡住提醒", short_run_id(&rid));
                }
                if b(&sum, "question") {
                    return format!("run {} 提问等决定", short_run_id(&rid));
                }
                let review_v = {
                    let rf = json(rdir.join("review.json"));
                    if rf.is_object() {
                        s(&rf, "verdict").to_string()
                    } else {
                        s(&sum["review"], "verdict").to_string()
                    }
                };
                if review_v == "NEEDS_CONTROLLER" {
                    return format!("run {} 需主控审查", short_run_id(&rid));
                }
                if st == "delivered" {
                    let meta = json(rdir.join("meta.json"));
                    let files = delivered_pending_files(&rdir, &meta, &sum);
                    if files == 0 {
                        return "已收取".into();
                    }
                    let rev_tag = if review_v == "PASS" { " 审PASS" } else { "" };
                    let acc_tag = if sum["accept"]["ok"] == json!(true) {
                        " 验✓"
                    } else {
                        ""
                    };
                    return format!("待合 +{files}f{acc_tag}{rev_tag}");
                }
                if st == "answered" {
                    return format!("run {} 有新答复", short_run_id(&rid));
                }
                return format!("run {} {st}", short_run_id(&rid));
            }
            "需主控处理".into()
        }
        "blocked" => {
            for cond in &item.wait_on {
                let ev = evaluate_condition(cond, item, all_items, state, now_epoch);
                if !ev.satisfied {
                    return ev.detail;
                }
            }
            "等前置".into()
        }
        "ready" => {
            if !item.wait_on.is_empty() {
                if let Some(first) = item.wait_on.first() {
                    let ev = evaluate_condition(first, item, all_items, state, now_epoch);
                    if ev.satisfied {
                        return ev.detail;
                    }
                }
            }
            let age = format_age(&item.state_since, now_epoch);
            if age.is_empty() {
                "可派发".into()
            } else {
                format!("可派发 {age}")
            }
        }
        "verify" => {
            if let Some(art) = item.artifacts.first() {
                return format!("未接线 {}", clip_chars(&art.path, 22));
            }
            let pending = item.pending_followups();
            if pending > 0 {
                return format!("待办 {pending} 条");
            }
            if let Some((_, rdir)) = latest_live_run(item, state) {
                let meta = json(rdir.join("meta.json"));
                let sum = json(rdir.join("summary.json"));
                if is_zero_change_delivered(&rdir, &meta, &sum) {
                    return if !item.done_when.is_empty() {
                        format!("已收取 · 待验({})", clip_chars(&item.done_when, 12))
                    } else {
                        "已收取".into()
                    };
                }
            }
            let age = format_age(&item.state_since, now_epoch);
            if !item.done_when.is_empty() {
                format!("待验({})", clip_chars(&item.done_when, 16))
            } else {
                format!("待闭环 {age}")
            }
        }
        "deferred" => {
            if !item.defer_until.is_empty() {
                let short_t = &item.defer_until[..item.defer_until.len().min(16)];
                format!("暂缓至 {short_t}")
            } else if !item.defer_reason.is_empty() {
                clip_chars(&item.defer_reason, 20)
            } else {
                "已暂缓".into()
            }
        }
        "done" => {
            if item.closed_kind == "dropped" {
                format!("已放弃：{}", clip_chars(&item.close_reason, 18))
            } else if !item.close_evidence.is_empty() {
                format!("已完成：{}", clip_chars(&item.close_evidence, 18))
            } else {
                "已完成".into()
            }
        }
        other => other.to_string(),
    }
}

fn next_action_for(item: &WorkItem, _all_items: &[WorkItem], state: &Path) -> String {
    if let Some(ow) = &item.owner {
        if item.state == "done" {
            return format!("work show {}", item.id);
        }
        if ow.kind == "direction" {
            return format!("work show {}", item.id);
        }
        return format!("work close {} --evidence \"所有者：…\"", item.id);
    }
    match item.state.as_str() {
        "attention" => {
            if let Some((rid, rdir)) = latest_live_run(item, state) {
                let st = runs::state(&rdir);
                let sum = json(rdir.join("summary.json"));
                let short = short_run_id(&rid);
                if b(&sum, "question") {
                    return format!("reply {short} \"…\"");
                }
                if st == "delivered" {
                    let meta = json(rdir.join("meta.json"));
                    if delivered_pending_files(&rdir, &meta, &sum) == 0 {
                        return format!("result {short}");
                    }
                    return format!("apply {short}");
                }
                if st == "answered" {
                    return format!("result {short}");
                }
                return format!("status {short}");
            }
            format!("work show {}", item.id)
        }
        "ready" => {
            if !item.brief.is_empty() {
                format!("start --work {} --prompt-file {}", item.id, item.brief)
            } else {
                format!("start --work {} \"…\"", item.id)
            }
        }
        "verify" => format!("work close {} --evidence \"…\"", item.id),
        "active" => {
            if let Some((rid, _)) = latest_live_run(item, state) {
                format!("wait {}", short_run_id(&rid))
            } else {
                "wait".into()
            }
        }
        "blocked" => {
            if item.auto_dispatch.is_some() {
                "满足后自动派".into()
            } else {
                format!("work show {}", item.id)
            }
        }
        "deferred" => format!("work show {}", item.id),
        "done" => format!("work show {}", item.id),
        _ => format!("work show {}", item.id),
    }
}

fn short_run_id(rid: &str) -> String {
    if rid.len() <= 16 {
        return rid.to_string();
    }
    // Format is usually YYYYMMDD-HHMMSS-<slug>: return trailing slug if >= 4 chars
    if let Some((_, rest)) = rid.split_once('-') {
        if let Some((_, slug)) = rest.split_once('-') {
            if slug.chars().count() >= 4 {
                return clip_chars(slug, 14);
            }
        }
    }
    clip_chars(rid, 14)
}

/// Collect ephemeral unattached runs (not bound to any work item), folded by reply chain.
fn ephemeral_run_rows(items: &[WorkItem], state: &Path, now_epoch: i64) -> Vec<TableRow> {
    let mut attached_runs: HashSet<String> = HashSet::new();
    for w in items {
        for r in &w.runs {
            attached_runs.insert(r.clone());
        }
        if !w.note_root.is_empty() {
            attached_runs.insert(w.note_root.clone());
        }
    }

    let mut rows = Vec::new();
    let mut seen_run = HashSet::new();

    for (repo_root, runs_dir) in registered_repo_runs(state) {
        if !runs_dir.is_dir() {
            continue;
        }
        let mut repo_runs = Vec::new();
        if let Ok(rd) = fs::read_dir(&runs_dir) {
            for ent in rd.flatten() {
                let p = ent.path();
                if p.join("meta.json").is_file() {
                    repo_runs.push(p);
                }
            }
        }
        if repo_runs.is_empty() {
            continue;
        }
        repo_runs.sort();

        // Single pass: read each run's meta.json ONCE to avoid O(N^2) chain scans.
        let mut metas: Vec<(PathBuf, String, Value)> = Vec::with_capacity(repo_runs.len());
        let mut parents: HashSet<String> = HashSet::new();
        let mut by_id: HashMap<String, usize> = HashMap::with_capacity(repo_runs.len());
        for r in repo_runs {
            let meta = json(r.join("meta.json"));
            let rid = notes::run_id(&r, &meta);
            let p = s(&meta, "parent");
            if !p.is_empty() {
                parents.insert(p.to_string());
            }
            let idx = metas.len();
            if !rid.is_empty() {
                by_id.insert(rid.clone(), idx);
            }
            metas.push((r, rid, meta));
        }

        let repo_label = short_repo_name(&repo_root.to_string_lossy()).to_string();

        for (r, rid, meta) in &metas {
            if rid.is_empty() || parents.contains(rid) || !seen_run.insert(rid.clone()) {
                continue;
            }
            if !s(meta, "workId").is_empty() {
                continue;
            }
            // Fast O(chain_depth) walk using pre-loaded metas map (zero extra disk reads).
            let mut members = Vec::new();
            let mut cur_id = rid.clone();
            let mut cur_meta = meta;
            let mut visited = HashSet::new();
            let mut root_name = s(cur_meta, "name").to_string();
            while visited.insert(cur_id.clone()) {
                members.push(cur_id.clone());
                let raw_name = s(cur_meta, "name");
                if !raw_name.is_empty() {
                    root_name = notes::strip_reply_prefixes(raw_name).0.to_string();
                }
                let parent = s(cur_meta, "parent");
                if parent.is_empty() {
                    break;
                }
                if let Some(&pidx) = by_id.get(parent) {
                    cur_id = metas[pidx].1.clone();
                    cur_meta = &metas[pidx].2;
                } else {
                    members.push(parent.to_string());
                    break;
                }
            }
            let chain_name = if root_name.is_empty() {
                rid.clone()
            } else {
                root_name
            };
            if members.iter().any(|m| attached_runs.contains(m)) {
                continue;
            }
            let st = runs::state(r);
            let sum = json(r.join("summary.json"));
            let is_applied = r.join(".applied").exists() || r.join(".applied-cleaned").exists();
            let is_question = b(&sum, "question");

            let (work_state, focus, next) = if runs::active(&st) {
                let elapsed = format_age(s(meta, "startedAt"), now_epoch);
                if r.join("stall-warning.json").is_file() {
                    (
                        "attention".to_string(),
                        format!("卡住提醒 {elapsed}"),
                        format!("status {}", short_run_id(rid)),
                    )
                } else {
                    (
                        "active".to_string(),
                        format!(
                            "{} {elapsed}",
                            if st == "waiting" { "排队中" } else { "运行中" }
                        ),
                        format!("wait {}", short_run_id(rid)),
                    )
                }
            } else if is_applied {
                continue;
            } else if is_question {
                (
                    "attention".to_string(),
                    "提问等决定".to_string(),
                    format!("reply {} \"…\"", short_run_id(rid)),
                )
            } else if st == "delivered" && s(meta, "mode") == "write" {
                let files = delivered_pending_files(r, meta, &sum);
                if files == 0 {
                    (
                        "verify".to_string(),
                        "已收取".to_string(),
                        format!("clean {}", short_run_id(rid)),
                    )
                } else {
                    (
                        "attention".to_string(),
                        format!("待合 +{files}f"),
                        format!("apply {}", short_run_id(rid)),
                    )
                }
            } else if st == "answered"
                && !r.join(".delivered").exists()
                && s(meta, "resultReadAt").is_empty()
            {
                (
                    "attention".to_string(),
                    "有新答复".to_string(),
                    format!("result {}", short_run_id(rid)),
                )
            } else if matches!(
                st.as_str(),
                "rejected" | "failed" | "malformed" | "timeout" | "stalled" | "crashed"
            ) {
                (
                    "attention".to_string(),
                    format!("异常 {st}"),
                    format!("status {}", short_run_id(rid)),
                )
            } else {
                // Finished read-only run already read -> exits table cleanly!
                continue;
            };

            let short = short_run_id(rid);
            rows.push(TableRow {
                id: format!("R:{short}"),
                repo: repo_label.clone(),
                title: clip_chars(&chain_name, 30),
                state: work_state,
                focus,
                next,
                is_ephemeral: true,
                run_id: rid.clone(),
                missing_class: false,
            });
        }
    }
    rows
}

fn format_table_row(row: &TableRow) -> String {
    let title = clip_chars(&row.title, 26);
    let focus = clip_chars(&row.focus, 22);
    let next = clip_chars(&row.next, 32);
    let line = format!(
        "[{}] [{}] {} · {} · {} · 下一步：{}",
        row.id, row.repo, title, row.state, focus, next
    );
    if row.missing_class {
        let suffix = " · 缺类别";
        let max_body = 100usize.saturating_sub(suffix.chars().count());
        format!("{}{}", clip_chars(&line, max_body), suffix)
    } else {
        clip_chars(&line, 100)
    }
}

fn render_layer0_summary(counts: &HashMap<&str, usize>) -> String {
    format!(
        "工作表：需处理 {} · 就绪 {} · 待闭环 {} · 推进中 {} · 等待 {} · 暂缓 {}",
        counts.get("attention").copied().unwrap_or(0),
        counts.get("ready").copied().unwrap_or(0),
        counts.get("verify").copied().unwrap_or(0),
        counts.get("active").copied().unwrap_or(0),
        counts.get("blocked").copied().unwrap_or(0),
        counts.get("deferred").copied().unwrap_or(0),
    )
}

fn waiting_work_for_owner<'a>(owner_id: &str, all_items: &'a [WorkItem]) -> Vec<&'a WorkItem> {
    let mut waiters: Vec<&WorkItem> = all_items
        .iter()
        .filter(|w| {
            w.owner.is_none()
                && w.state != "done"
                && w.wait_on.iter().any(|c| {
                    c.strip_prefix("owner:").is_some_and(|raw| {
                        let trimmed = raw.trim();
                        normalize_work_id(trimmed).as_deref() == Some(owner_id)
                            || trimmed.eq_ignore_ascii_case(owner_id)
                    })
                })
        })
        .collect();
    waiters.sort_by(|a, b| a.id.cmp(&b.id));
    waiters
}

fn render_card(item: &WorkItem, all_items: &[WorkItem], state: &Path, now_epoch: i64) -> String {
    let mut lines = Vec::new();
    let age = format_age(&item.state_since, now_epoch);
    let age_str = if age.is_empty() {
        String::new()
    } else {
        format!("（已停留 {age}）")
    };
    lines.push(format!(
        "[{}] {} · [{}] · 状态：{}{}",
        item.id,
        item.title,
        item.repo_label(),
        item.state,
        age_str
    ));
    if let Some(ow) = &item.owner {
        lines.push(format!(
            "所有者事项：{}（{}）",
            OwnerInfo::kind_label(&ow.kind),
            ow.kind
        ));
        if !ow.ask.is_empty() {
            lines.push(format!("要你回答：{}", ow.ask));
        }
        if !ow.recommendation.is_empty() {
            lines.push(format!("主控建议：{}", ow.recommendation));
        }
        if !ow.if_undecided.is_empty() {
            lines.push(format!("不定会怎样：{}", ow.if_undecided));
        }
        if !ow.cost.is_empty() {
            lines.push(format!("成本：{}", ow.cost));
        }
        let waiters = waiting_work_for_owner(&item.id, all_items);
        if !waiters.is_empty() {
            lines.push("在等它的工作：".to_string());
            for w in waiters {
                let mut parts = vec![format!("[{}] {}", w.id, w.title)];
                if !w.done_when.trim().is_empty() {
                    parts.push(format!("完成条件：{}", w.done_when.trim()));
                }
                if !w.brief.trim().is_empty() {
                    parts.push(format!("说明：{}", w.brief.trim()));
                }
                lines.push(format!("  - {}", parts.join(" · ")));
            }
        }
    }
    if !item.class.is_empty() {
        lines.push(format!("价值类别：{}", item.class));
    } else if item.is_missing_class() {
        lines.push("价值类别：缺类别（不参与补位，用 delegate work wait <ID> --class feat|unblock|infra|docs 补齐）".to_string());
    }
    if !item.files.is_empty() {
        lines.push(format!("改动路径：{}", item.files.join("、")));
    }
    if !item.done_when.is_empty() {
        lines.push(format!("完成条件：{}", item.done_when));
    }
    if item.needs_verify {
        lines.push("闭环要求：合入后需人工验证（--needs-verify）".to_string());
    }
    if !item.brief.is_empty() {
        let label = if item.owner.is_some() { "卡片" } else { "任务说明" };
        lines.push(format!("{label}：{}", item.brief));
    }
    if !item.owner_ref.is_empty() {
        let owner_path = if item.repo.is_empty() {
            "docs/owner/README.md".to_string()
        } else {
            format!("{}/docs/owner/README.md", item.repo)
        };
        lines.push(format!("所有者案头：{}（{}）", item.owner_ref, owner_path));
    }
    if !item.wait_on.is_empty() {
        lines.push("前置条件：".to_string());
        for cond in &item.wait_on {
            let ev = evaluate_condition(cond, item, all_items, state, now_epoch);
            let mark = if ev.satisfied { "✓" } else { "⏳" };
            let ptr = if ev.pointer.is_empty() {
                String::new()
            } else {
                format!("（{}）", ev.pointer)
            };
            lines.push(format!("  {mark} {} — {}{}", ev.raw, ev.detail, ptr));
        }
    }
    if !item.verify_on.is_empty() {
        lines.push("闭环条件：".to_string());
        for cond in &item.verify_on {
            let ev = evaluate_condition(cond, item, all_items, state, now_epoch);
            let mark = if ev.satisfied { "✓" } else { "⏳" };
            let ptr = if ev.pointer.is_empty() {
                String::new()
            } else {
                format!("（{}）", ev.pointer)
            };
            lines.push(format!("  {mark} {} — {}{}", ev.raw, ev.detail, ptr));
        }
    }
    if let Some(ad) = &item.auto_dispatch {
        let status = if ad.dispatched_run.is_empty() {
            "待触发".to_string()
        } else {
            format!("已派 {}", ad.dispatched_run)
        };
        lines.push(format!(
            "自动派发：{status} · prompt={}{}",
            ad.prompt_file,
            if ad.start_args.is_empty() {
                String::new()
            } else {
                format!(" · args={}", ad.start_args.join(" "))
            }
        ));
    }
    if !item.runs.is_empty() {
        lines.push(format!("关联运行（{}）：", item.runs.len()));
        for rid in item.runs.iter().rev().take(5).rev() {
            if let Some(rdir) = find_live_run_dir(rid, &item.run_dirs, state) {
                let st = runs::state(&rdir);
                let meta = json(rdir.join("meta.json"));
                let sum = json(rdir.join("summary.json"));
                let applied_tag = if rdir.join(".applied").exists() || rdir.join(".applied-cleaned").exists() {
                    " · 已合入"
                } else if is_zero_change_delivered(&rdir, &meta, &sum) {
                    " · 已收取"
                } else {
                    ""
                };
                lines.push(format!("  - {rid} · {st}{applied_tag}（delegate show {rid}）"));
            } else if let Some(h) = history_entry_for_run(rid, state, Some(&item.repo)) {
                let st = s(&h, "state");
                let head = s(&h, "appliedHead");
                let head_tag = if head.is_empty() {
                    String::new()
                } else {
                    format!(" · sha {}", &head[..head.len().min(7)])
                };
                lines.push(format!("  - {rid} · {st}{head_tag}（已清理 · delegate show {rid}）"));
            } else {
                lines.push(format!("  - {rid}（delegate show {rid}）"));
            }
        }
    }
    if !item.artifacts.is_empty() {
        lines.push("待接线产物：".to_string());
        for art in &item.artifacts {
            let src = if art.source_run.is_empty() {
                String::new()
            } else {
                format!("（来源：{}）", art.source_run)
            };
            let note = if art.note.is_empty() {
                String::new()
            } else {
                format!(" — {}", art.note)
            };
            lines.push(format!("  - {}{}{}", art.path, note, src));
        }
    }
    if !item.followups.is_empty() {
        lines.push(format!(
            "细分待办（未完成 {} / 共 {}）：",
            item.pending_followups(),
            item.followups.len()
        ));
        for (i, f) in item.followups.iter().enumerate() {
            let mark = if f.done { "x" } else { " " };
            let anc = if f.anchor.is_empty() {
                String::new()
            } else {
                format!(" — {}", f.anchor)
            };
            lines.push(format!("  {}. [{}] [{}] {}{}", i + 1, mark, f.kind, f.text, anc));
        }
    }
    if item.state == "deferred" || !item.defer_reason.is_empty() {
        let streak_tag = if item.defer_streak >= 3 {
            format!(" · 反复暂缓（连续 {} 次）", item.defer_streak)
        } else {
            String::new()
        };
        lines.push(format!(
            "暂缓：{}（至 {}）{}",
            item.defer_reason,
            if item.defer_until.is_empty() {
                "已到期"
            } else {
                item.defer_until.as_str()
            },
            streak_tag
        ));
    }
    if item.state == "done" {
        if item.closed_kind == "dropped" {
            lines.push(format!("放弃理由：{}", item.close_reason));
        } else {
            lines.push(format!("关闭证据：{}", item.close_evidence));
        }
    }

    // Layer-3 original pointers written deterministically by Host
    let mut ptrs = Vec::new();
    if let Some(rid) = item.latest_run_id() {
        ptrs.push(format!("delegate result {rid}"));
        ptrs.push(format!("delegate diff {rid} --total"));
    }
    if !item.brief.is_empty() {
        ptrs.push(item.brief.clone());
    }
    if !item.owner_ref.is_empty() {
        ptrs.push("docs/owner/README.md".into());
    }
    if !ptrs.is_empty() {
        lines.push(format!("原文：{}", ptrs.join(" · ")));
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// `delegate work import-md <file> [--dry-run]`
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct ImportedDraft {
    line_no: usize,
    section: String,
    raw_text: String,
    title: String,
    state: String,
    wait_on: Vec<String>,
    brief: String,
    owner_ref: String,
    done_when: String,
    suspected_stale: Option<String>,
    duplicate_of: Option<String>,
}

impl ImportedDraft {
    fn to_json(&self, file_str: &str) -> Value {
        let mut obj = json!({
            "source": format!("{file_str}:{}", self.line_no),
            "section": self.section,
            "title": self.title,
            "state": self.state,
            "waitOn": self.wait_on,
            "raw": self.raw_text,
        });
        if !self.brief.is_empty() {
            obj["brief"] = json!(self.brief);
        }
        if !self.owner_ref.is_empty() {
            obj["ownerRef"] = json!(self.owner_ref);
        }
        if !self.done_when.is_empty() {
            obj["doneWhen"] = json!(self.done_when);
        }
        if let Some(stale) = &self.suspected_stale {
            obj["suspectedStale"] = json!(stale);
        }
        if let Some(dup) = &self.duplicate_of {
            obj["duplicateOf"] = json!(dup);
        }
        obj
    }
}

fn extract_brief_path(text: &str) -> String {
    for token in text.split(|c: char| {
        c.is_whitespace() || "（）()[]【】、，,;；\"'`：:".contains(c)
    }) {
        if (token.contains(".local/plan/") || token.starts_with("brief-")) && token.ends_with(".md")
        {
            return token.to_string();
        }
    }
    String::new()
}

fn extract_hex_run_ref(text: &str) -> Option<String> {
    // Match 4-8 char hex run suffix mentioned near 合入/交付/链 (e.g. `007d`, `83cb`)
    for raw in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_') {
        let tok = raw.trim();
        if tok.len() >= 4
            && tok.len() <= 8
            && tok.bytes().all(|b| b.is_ascii_hexdigit())
            && tok.bytes().any(|b| b.is_ascii_digit())
            && tok.bytes().any(|b| matches!(b, b'a'..=b'f' | b'A'..=b'F'))
        {
            return Some(tok.to_lowercase());
        }
    }
    None
}

fn extract_owner_ref(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 2 <= bytes.len() {
        let ch = bytes[i];
        if matches!(ch, b'Q' | b'D') && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric()) {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let digits = j - (i + 1);
            let after_ok = j >= bytes.len() || !bytes[j].is_ascii_alphanumeric();
            if (1..=4).contains(&digits) && after_ok {
                return Some(text[i..j].to_string());
            }
        }
        i += 1;
    }
    None
}

fn extract_future_mmdd(text: &str, current_year: i32, now_epoch: i64) -> Option<String> {
    let bytes = text.as_bytes();
    let mut best: Option<(i64, String)> = None;
    let mut i = 0;
    while i + 5 <= bytes.len() {
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_digit();
        let after_ok = i + 5 >= bytes.len() || !bytes[i + 5].is_ascii_digit();
        if before_ok
            && after_ok
            && bytes[i].is_ascii_digit()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2] == b'-'
            && bytes[i + 3].is_ascii_digit()
            && bytes[i + 4].is_ascii_digit()
        {
            let m: i32 = text[i..i + 2].parse().unwrap_or(0);
            let d: i32 = text[i + 3..i + 5].parse().unwrap_or(0);
            if (1..=12).contains(&m) && (1..=31).contains(&d) {
                let iso_str = format!("{current_year:04}-{m:02}-{d:02}T00:00:00Z");
                if let Some(ts) = iso_epoch(&iso_str) {
                    if ts > now_epoch && best.as_ref().is_none_or(|(b_ts, _)| ts < *b_ts) {
                        best = Some((ts, iso_str));
                    }
                }
            }
        }
        i += 1;
    }
    best.map(|(_, s)| s)
}

fn extract_title_from_bullet(text: &str) -> String {
    let split = text
        .split(['：', '。', '；'])
        .next()
        .unwrap_or(text)
        .trim();
    clip_chars(if split.is_empty() { text } else { split }, 36)
}

fn tokenize_for_match(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            cur.push(ch.to_ascii_lowercase());
        } else {
            if cur.len() >= 3 {
                out.insert(cur.clone());
            }
            cur.clear();
        }
    }
    if cur.len() >= 3 {
        out.insert(cur);
    }
    // CJK bigrams
    let cjk: Vec<char> = text
        .chars()
        .filter(|c| ('\u{4E00}'..='\u{9FFF}').contains(c))
        .collect();
    for win in cjk.windows(2) {
        out.insert(format!("{}{}", win[0], win[1]));
    }
    out
}

fn load_applied_history_records(state: &Path, extra_repo: Option<&str>) -> Vec<Value> {
    let mut out = Vec::new();
    let mut seen_runs = HashSet::new();
    for path in history_files_for_state(state, extra_repo) {
        let Ok(f) = OpenOptions::new().read(true).open(&path) else {
            continue;
        };
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if s(&v, "appliedAt").is_empty() {
                continue;
            }
            let rid = s(&v, "run").to_string();
            if !rid.is_empty() && seen_runs.insert(rid) {
                out.push(v);
            }
        }
    }
    out
}

fn parse_markdown_backlog(
    file: &Path,
    state: &Path,
    repo_hint: Option<&str>,
    existing_items: &[WorkItem],
) -> Res<Vec<ImportedDraft>> {
    let content = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let now_epoch = epoch() as i64;
    let current_year = format_epoch(now_epoch, false, "%Y")
        .parse::<i32>()
        .unwrap_or(2026);
    let hist = load_applied_history_records(state, repo_hint);
    let hist_tokens: Vec<(String, String, HashSet<String>)> = hist
        .iter()
        .map(|h| {
            let rid = s(h, "run").to_string();
            let applied_at = s(h, "appliedAt").to_string();
            let combined = format!(
                "{} {} {}",
                s(h, "name"),
                s(h, "promptFirstLine"),
                s(h, "conclusion")
            );
            (rid, applied_at, tokenize_for_match(&combined))
        })
        .collect();

    let mut section = String::new();
    let mut drafts = Vec::new();

    for (idx, raw_line) in content.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw_line.trim();
        if let Some(heading) = trimmed.strip_prefix("## ") {
            section = heading.trim().to_string();
            continue;
        }
        let bullet_text = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .map(str::trim);
        let Some(text) = bullet_text.filter(|t| !t.is_empty()) else {
            continue;
        };

        let brief = extract_brief_path(text);
        let owner_ref = if section.contains("所有者") || section.to_ascii_lowercase().contains("owner")
        {
            extract_owner_ref(text).unwrap_or_default()
        } else {
            String::new()
        };

        let mut wait_on: Vec<String> = Vec::new();
        if !owner_ref.is_empty() {
            wait_on.push(format!("owner:{owner_ref}"));
        }
        if let Some(run_ref) = extract_hex_run_ref(text) {
            if text.contains("合入后") || text.contains("合入") {
                wait_on.push(format!("run_applied:{run_ref}"));
            } else if text.contains("交付") {
                wait_on.push(format!("run_delivered:{run_ref}"));
            }
        }
        if let Some(future_iso) = extract_future_mmdd(text, current_year, now_epoch) {
            wait_on.push(format!("after_time:{future_iso}"));
        }

        let proposed_state = if !wait_on.is_empty() {
            "blocked".to_string()
        } else if section.contains("暂缓") {
            "deferred".to_string()
        } else if section.contains("闭环") || section.contains("验证") {
            "verify".to_string()
        } else {
            "ready".to_string()
        };

        let title = extract_title_from_bullet(text);
        let done_when = if text.chars().count() > 36 {
            clip_chars(text, 160)
        } else {
            String::new()
        };

        // Check suspected stale / already-applied against history.jsonl
        let mut suspected_stale: Option<String> = None;
        let item_tokens = tokenize_for_match(text);
        if item_tokens.len() >= 3 {
            let mut best_match: Option<(f64, usize, String, String)> = None;
            for (rid, applied_at, htoks) in &hist_tokens {
                let shared = item_tokens.intersection(htoks).count();
                if shared >= 3 {
                    let ratio = shared as f64 / item_tokens.len().min(htoks.len()).max(1) as f64;
                    if ratio >= 0.45
                        && best_match
                            .as_ref()
                            .is_none_or(|(br, bs, _, _)| (ratio, shared) > (*br, *bs))
                    {
                        best_match = Some((ratio, shared, rid.clone(), applied_at.clone()));
                    }
                }
            }
            if let Some((_, shared, rid, applied_at)) = best_match {
                let short_at = &applied_at[..applied_at.len().min(10)];
                suspected_stale = Some(format!(
                    "history 命中已合入 {rid}（{short_at}，重合 {shared} 词 · delegate show {rid}）"
                ));
            }
        }
        // Also flag if a `run_applied:<ref>` wait_on is ALREADY applied in history
        if suspected_stale.is_none() {
            for w in &wait_on {
                if let Some(rref) = w.strip_prefix("run_applied:") {
                    if is_run_applied(rref, state, repo_hint) {
                        suspected_stale = Some(format!(
                            "前置 {rref} 已合入，导入后可直接转 ready（delegate show {rref}）"
                        ));
                    }
                }
            }
        }

        // Check duplicates in existing work.jsonl
        let duplicate_of = existing_items
            .iter()
            .find(|w| {
                w.state != "done"
                    && ((!brief.is_empty() && w.brief == brief)
                        || (!owner_ref.is_empty() && w.owner_ref == owner_ref)
                        || w.title == title)
            })
            .map(|w| w.id.clone());

        drafts.push(ImportedDraft {
            line_no,
            section: section.clone(),
            raw_text: text.to_string(),
            title,
            state: proposed_state,
            wait_on,
            brief,
            owner_ref,
            done_when,
            suspected_stale,
            duplicate_of,
        });
    }
    Ok(drafts)
}

// ---------------------------------------------------------------------------
// Cycle check for `work_closed:<Wxx>` / `work_applied:<Wxx>` / `owner:<Qxx>`
// ---------------------------------------------------------------------------

fn has_work_cycle(items: &[WorkItem], from_id: &str, new_wait_on: &[String]) -> bool {
    let extract_dep = |c: &str| -> Option<String> {
        c.strip_prefix("work_closed:")
            .or_else(|| c.strip_prefix("work_applied:"))
            .or_else(|| c.strip_prefix("owner:"))
            .and_then(normalize_work_id)
    };
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for item in items {
        let deps: Vec<String> = item
            .wait_on
            .iter()
            .filter_map(|c| extract_dep(c))
            .collect();
        adj.insert(item.id.clone(), deps);
    }
    let added: Vec<String> = new_wait_on
        .iter()
        .filter_map(|c| extract_dep(c))
        .collect();
    adj.entry(from_id.to_string()).or_default().extend(added.clone());

    let mut stack = added;
    let mut visited = HashSet::new();
    while let Some(cur) = stack.pop() {
        if cur == from_id {
            return true;
        }
        if !visited.insert(cur.clone()) {
            continue;
        }
        if let Some(nexts) = adj.get(&cur) {
            for n in nexts {
                stack.push(n.clone());
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// CLI: `delegate work ...`
// ---------------------------------------------------------------------------

pub fn command(args: &[String]) -> Res<i32> {
    let Some((first, rest)) = args.split_first() else {
        return list_cmd(&[]);
    };
    if first.starts_with('-') {
        return list_cmd(args);
    }
    match first.as_str() {
        "list" | "ls" => list_cmd(rest),
        "show" => show_cmd(rest),
        "add" | "new" => add_cmd(rest),
        "wait" => wait_cmd(rest),
        "close" => close_cmd(rest),
        "drop" => drop_cmd(rest),
        "defer" => defer_cmd(rest),
        "import-md" => import_md_cmd(rest),
        "import-dag" => import_dag_cmd(rest),
        other => Err(format!(
            "unknown work subcommand: '{other}' (expected: list, show, add, wait, close, drop, defer, import-md, import-dag)"
        )),
    }
}

fn list_cmd(args: &[String]) -> Res<i32> {
    let mut show_all = false;
    let mut owner_only = false;
    let mut rebuild = false;
    let mut state_filter: Option<String> = None;
    let mut repo_filter: Option<String> = None;
    let mut json_mode = crate::output::json_enabled();

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (key, inline) = a
            .split_once('=')
            .map_or((a.as_str(), None), |(k, v)| (k, Some(v)));
        match key {
            "--all" => show_all = true,
            "--owner" => owner_only = true,
            "--rebuild" => rebuild = true,
            "--json" => json_mode = true,
            "--state" => {
                let v = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --state: expected one argument")?
                    }
                };
                if !STATES.contains(&v.as_str()) {
                    return Err(format!(
                        "invalid state '{v}' (expected: {})",
                        STATES.join("|")
                    ));
                }
                state_filter = Some(v);
            }
            "--repo" => {
                let v = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --repo: expected one argument")?
                    }
                };
                repo_filter = Some(v);
            }
            other => return Err(format!("unrecognized arguments: {other}")),
        }
        i += 1;
    }

    let state = state_dir();
    let items = if rebuild {
        // Force migration + full reconcile pass
        let _ = fs::remove_file(work_migrations_path(&state));
        load_synced_in(&state).unwrap_or_else(|_| load_raw(&state))
    } else {
        load_readonly_in(&state)
    };
    let now_epoch = epoch() as i64;

    if owner_only {
        return render_owner_list(&items, &state, now_epoch, show_all, state_filter.as_deref(), repo_filter.as_deref(), json_mode);
    }
    let ephemeral = ephemeral_run_rows(&items, &state, now_epoch);

    let mut all_rows: Vec<TableRow> = items
        .iter()
        .map(|w| TableRow {
            id: w.id.clone(),
            repo: w.repo_label(),
            title: w.title.clone(),
            state: w.state.clone(),
            focus: focus_for_item(w, &items, &state, now_epoch),
            next: next_action_for(w, &items, &state),
            is_ephemeral: false,
            run_id: w.latest_run_id().unwrap_or("").to_string(),
            missing_class: w.is_missing_class(),
        })
        .collect();
    all_rows.extend(ephemeral);

    if let Some(rf) = &repo_filter {
        let rf_low = rf.to_ascii_lowercase();
        all_rows.retain(|r| r.repo.to_ascii_lowercase().contains(&rf_low));
    }

    let mut counts: HashMap<&str, usize> = HashMap::new();
    for r in &all_rows {
        for &st in &STATES {
            if r.state == st {
                *counts.entry(st).or_insert(0) += 1;
            }
        }
    }

    let mut visible: Vec<&TableRow> = all_rows
        .iter()
        .filter(|r| {
            if let Some(sf) = &state_filter {
                return &r.state == sf;
            }
            if show_all {
                return r.state != "done";
            }
            matches!(r.state.as_str(), "attention" | "ready" | "verify" | "active")
        })
        .collect();
    visible.sort_by(|a, b| {
        state_priority(&a.state)
            .cmp(&state_priority(&b.state))
            .then_with(|| a.id.cmp(&b.id))
    });

    if json_mode {
        for r in &visible {
            if r.is_ephemeral {
                println!(
                    "{}",
                    json!({
                        "id": r.id,
                        "repo": r.repo,
                        "title": r.title,
                        "state": r.state,
                        "focus": r.focus,
                        "next": r.next,
                        "ephemeral": true,
                        "run": r.run_id,
                    })
                );
            } else if let Some(w) = items.iter().find(|w| w.id == r.id) {
                let mut obj = w.to_json();
                obj["repoLabel"] = json!(r.repo);
                obj["focus"] = json!(r.focus);
                obj["next"] = json!(r.next);
                println!("{obj}");
            }
        }
        println!(
            "{}",
            json!({
                "workSummary": {
                    "attention": counts.get("attention").copied().unwrap_or(0),
                    "ready": counts.get("ready").copied().unwrap_or(0),
                    "verify": counts.get("verify").copied().unwrap_or(0),
                    "active": counts.get("active").copied().unwrap_or(0),
                    "blocked": counts.get("blocked").copied().unwrap_or(0),
                    "deferred": counts.get("deferred").copied().unwrap_or(0),
                    "done": counts.get("done").copied().unwrap_or(0),
                }
            })
        );
        return Ok(0);
    }

    println!("{}", render_layer0_summary(&counts));
    if visible.is_empty() {
        println!("（当前无待处理项）");
    } else {
        for r in &visible {
            println!("{}", format_table_row(r));
        }
    }
    if !show_all && state_filter.is_none() {
        let b_cnt = counts.get("blocked").copied().unwrap_or(0);
        let d_cnt = counts.get("deferred").copied().unwrap_or(0);
        if b_cnt > 0 || d_cnt > 0 {
            println!("已折叠：等待 {b_cnt} · 暂缓 {d_cnt}（查看全部：delegate work --all）");
        }
    }
    let done_cnt = counts.get("done").copied().unwrap_or(0);
    let done_hint = if done_cnt > 0 {
        format!(" · 已关闭 {done_cnt}：delegate work --state done")
    } else {
        String::new()
    };
    println!("卡片：delegate work show <ID> · 全部：delegate work --all{done_hint}");
    Ok(0)
}

fn render_owner_list(
    items: &[WorkItem],
    state: &Path,
    now_epoch: i64,
    show_all: bool,
    state_filter: Option<&str>,
    repo_filter: Option<&str>,
    json_mode: bool,
) -> Res<i32> {
    let mut owner_items: Vec<&WorkItem> = items
        .iter()
        .filter(|w| w.owner.is_some())
        .filter(|w| {
            if let Some(rf) = repo_filter {
                w.repo_label()
                    .to_ascii_lowercase()
                    .contains(&rf.to_ascii_lowercase())
            } else {
                true
            }
        })
        .filter(|w| {
            if let Some(sf) = state_filter {
                w.state == sf
            } else {
                true
            }
        })
        .collect();

    let mut open: Vec<&WorkItem> = Vec::new();
    let mut dropped: Vec<&WorkItem> = Vec::new();
    let mut closed: Vec<&WorkItem> = Vec::new();
    for w in owner_items.drain(..) {
        if w.state == "done" {
            if w.closed_kind == "dropped" {
                dropped.push(w);
            } else {
                closed.push(w);
            }
        } else {
            open.push(w);
        }
    }

    open.sort_by(|a, b| {
        let ka = a
            .owner
            .as_ref()
            .map(|o| OwnerInfo::kind_priority(&o.kind))
            .unwrap_or(9);
        let kb = b
            .owner
            .as_ref()
            .map(|o| OwnerInfo::kind_priority(&o.kind))
            .unwrap_or(9);
        ka.cmp(&kb).then_with(|| a.id.cmp(&b.id))
    });
    dropped.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
    closed.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));

    let count_kind = |k: &str| -> usize {
        open.iter()
            .filter(|w| w.owner.as_ref().is_some_and(|o| o.kind == k))
            .count()
    };
    let n_act = count_kind("act");
    let n_nod = count_kind("nod");
    let n_card = count_kind("card");
    let n_dir = count_kind("direction");

    if json_mode {
        for w in &open {
            let mut obj = w.to_json();
            obj["repoLabel"] = json!(w.repo_label());
            obj["focus"] = json!(focus_for_item(w, items, state, now_epoch));
            obj["next"] = json!(next_action_for(w, items, state));
            println!("{obj}");
        }
        for w in &dropped {
            let mut obj = w.to_json();
            obj["repoLabel"] = json!(w.repo_label());
            obj["focus"] = json!(focus_for_item(w, items, state, now_epoch));
            obj["next"] = json!(next_action_for(w, items, state));
            println!("{obj}");
        }
        if show_all || state_filter == Some("done") {
            for w in &closed {
                let mut obj = w.to_json();
                obj["repoLabel"] = json!(w.repo_label());
                obj["focus"] = json!(focus_for_item(w, items, state, now_epoch));
                obj["next"] = json!(next_action_for(w, items, state));
                println!("{obj}");
            }
        }
        println!(
            "{}",
            json!({
                "ownerSummary": {
                    "open": open.len(),
                    "act": n_act,
                    "nod": n_nod,
                    "card": n_card,
                    "direction": n_dir,
                    "dropped": dropped.len(),
                    "closed": closed.len(),
                }
            })
        );
        return Ok(0);
    }

    let dropped_part = if dropped.is_empty() {
        String::new()
    } else {
        format!(" · 已否 {}", dropped.len())
    };
    println!(
        "等你 {}（动手 {n_act} · 点头 {n_nod} · 看卡 {n_card} · 方向 {n_dir}）{dropped_part}",
        open.len()
    );

    if open.is_empty() {
        println!("（当前无待所有者处理项）");
    } else {
        for &kind in &OWNER_KINDS {
            let group: Vec<&&WorkItem> = open
                .iter()
                .filter(|w| w.owner.as_ref().is_some_and(|o| o.kind == kind))
                .collect();
            if group.is_empty() {
                continue;
            }
            println!("\n## {} ({})", OwnerInfo::kind_label(kind), group.len());
            for w in group {
                let ow = w.owner.as_ref().unwrap();
                let ask = if ow.ask.is_empty() {
                    w.title.as_str()
                } else {
                    ow.ask.as_str()
                };
                let missing_tag = if w.is_missing_owner_fields() {
                    " · 缺建议"
                } else {
                    ""
                };
                println!("[{}] [{}] {}{}", w.id, w.repo_label(), ask, missing_tag);
                if !ow.recommendation.is_empty() {
                    println!("  建议：{}", ow.recommendation);
                }
                if !ow.if_undecided.is_empty() {
                    println!("  不定：{}", ow.if_undecided);
                }
                if !ow.cost.is_empty() {
                    println!("  成本：{}", ow.cost);
                }
                if !w.brief.is_empty() {
                    println!("  卡片：{}", w.brief);
                }
                println!("  下一步：delegate {}", next_action_for(w, items, state));
            }
        }
    }

    if !dropped.is_empty() {
        println!("\n## 已否 ({})", dropped.len());
        for w in &dropped {
            let ow = w.owner.as_ref().unwrap();
            let ask = if ow.ask.is_empty() {
                w.title.as_str()
            } else {
                ow.ask.as_str()
            };
            let date = if w.updated_at.len() >= 10 {
                &w.updated_at[..10]
            } else {
                ""
            };
            let date_tag = if date.is_empty() {
                String::new()
            } else {
                format!("（{date}）")
            };
            println!(
                "[{}] [{}] {} — {}{}",
                w.id,
                w.repo_label(),
                ask,
                w.close_reason,
                date_tag
            );
        }
    }

    if (show_all || state_filter == Some("done")) && !closed.is_empty() {
        println!("\n## 已定 ({})", closed.len());
        for w in &closed {
            let ow = w.owner.as_ref().unwrap();
            let ask = if ow.ask.is_empty() {
                w.title.as_str()
            } else {
                ow.ask.as_str()
            };
            println!("[{}] [{}] {} — {}", w.id, w.repo_label(), ask, w.close_evidence);
        }
    }

    println!("\n卡片：delegate work show <ID> · 关闭：delegate work close <ID> --evidence \"所有者：…\" · 否决：delegate work drop <ID> --reason \"…\"");
    Ok(0)
}

fn show_cmd(args: &[String]) -> Res<i32> {
    let mut json_mode = crate::output::json_enabled();
    let mut target: Option<String> = None;
    for a in args {
        if a == "--json" {
            json_mode = true;
        } else if a.starts_with('-') {
            return Err(format!("unrecognized arguments: {a}"));
        } else if target.is_none() {
            target = Some(a.clone());
        } else {
            return Err(format!("unexpected positional argument: {a}"));
        }
    }
    let raw = target.ok_or("usage: delegate work show <Wxx|R:run> [--json]")?;
    let state = state_dir();
    let items = load_readonly_in(&state);
    let now_epoch = epoch() as i64;

    if let Some(norm) = normalize_work_id(&raw) {
        let Some(item) = items.iter().find(|w| w.id == norm) else {
            return Err(format!(
                "work item '{norm}' not found (list with: delegate work --all)"
            ));
        };
        if json_mode {
            let mut obj = item.to_json();
            obj["focus"] = json!(focus_for_item(item, &items, &state, now_epoch));
            obj["next"] = json!(next_action_for(item, &items, &state));
            obj["waitOnStatus"] = json!(item
                .wait_on
                .iter()
                .map(|c| {
                    let ev = evaluate_condition(c, item, &items, &state, now_epoch);
                    json!({
                        "condition": ev.raw,
                        "satisfied": ev.satisfied,
                        "detail": ev.detail,
                        "pointer": ev.pointer,
                    })
                })
                .collect::<Vec<_>>());
            if !item.verify_on.is_empty() {
                obj["verifyOnStatus"] = json!(item
                    .verify_on
                    .iter()
                    .map(|c| {
                        let ev = evaluate_condition(c, item, &items, &state, now_epoch);
                        json!({
                            "condition": ev.raw,
                            "satisfied": ev.satisfied,
                            "detail": ev.detail,
                            "pointer": ev.pointer,
                        })
                    })
                    .collect::<Vec<_>>());
            }
            println!("{obj}");
        } else {
            println!("{}", render_card(item, &items, &state, now_epoch));
        }
        return Ok(0);
    }

    // Support `R:<short>` or bare run id
    let run_ref = raw.strip_prefix("R:").unwrap_or(&raw);
    let empty_dirs = BTreeMap::new();
    let Some(rdir) = find_run_dir(run_ref, &empty_dirs, &state) else {
        return Err(format!(
            "work item or run '{raw}' not found (list with: delegate work --all)"
        ));
    };
    let st = runs::status(&rdir);
    if json_mode {
        println!("{st}");
    } else {
        println!("{}", crate::output::human_detail(&rdir, &st));
    }
    Ok(0)
}

struct WaitFlags {
    wait_on: Vec<String>,
    verify_on: Vec<String>,
    needs_verify: Option<bool>,
    owner_ref: Option<String>,
    brief: Option<String>,
    auto: bool,
    start_args: Vec<String>,
    clear_wait: bool,
}

fn normalize_verify_condition(raw: &str) -> Res<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("--verify-on requires a non-empty condition".into());
    }
    if trimmed.starts_with("cmd:") {
        return Err(
            "--verify-on does not support cmd: (supported: run_applied:<run>, work_applied:<Wxx>, work_closed:<Wxx>, owner:<Qxx>, commit_exists:<sha>)"
                .into(),
        );
    }
    if let Some(run_ref) = trimmed.strip_prefix("run_applied:") {
        let r = run_ref.trim();
        if r.is_empty() {
            return Err("invalid --verify-on 'run_applied:' (missing run id)".into());
        }
        return Ok(format!("run_applied:{r}"));
    }
    if let Some(wid) = trimmed.strip_prefix("work_applied:") {
        let norm = normalize_work_id(wid)
            .ok_or_else(|| format!("invalid work id in --verify-on: '{wid}'"))?;
        return Ok(format!("work_applied:{norm}"));
    }
    if let Some(wid) = trimmed.strip_prefix("work_closed:") {
        let norm = normalize_work_id(wid)
            .ok_or_else(|| format!("invalid work id in --verify-on: '{wid}'"))?;
        return Ok(format!("work_closed:{norm}"));
    }
    if let Some(oref) = trimmed.strip_prefix("owner:") {
        let o = oref.trim();
        if o.is_empty() {
            return Err("invalid --verify-on 'owner:' (missing owner ref)".into());
        }
        let norm = normalize_work_id(o).unwrap_or_else(|| o.to_string());
        return Ok(format!("owner:{norm}"));
    }
    if let Some(sha) = trimmed.strip_prefix("commit_exists:") {
        let s_clean = sha.trim();
        if s_clean.len() < 4
            || s_clean.len() > 40
            || !s_clean.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(format!(
                "invalid commit sha in --verify-on '{trimmed}' (expected 4..=40 hex chars)"
            ));
        }
        return Ok(format!("commit_exists:{}", s_clean.to_ascii_lowercase()));
    }
    Err(format!(
        "invalid --verify-on '{trimmed}' (supported: run_applied:<run>, work_applied:<Wxx>, work_closed:<Wxx>, owner:<Qxx>, commit_exists:<sha>)"
    ))
}

fn matches_owner_wait_keyword(title: &str, done_when: &str) -> bool {
    OWNER_WAIT_KEYWORDS
        .iter()
        .any(|kw| title.contains(kw) || done_when.contains(kw))
}

struct ParsedWaitArgs {
    positional: Vec<String>,
    wf: WaitFlags,
    json_mode: bool,
    extra: HashMap<String, Vec<String>>,
}

fn parse_wait_conditions(args: &[String], allow_clear: bool) -> Res<ParsedWaitArgs> {
    let mut positional = Vec::new();
    let mut json_mode = crate::output::json_enabled();
    let mut wf = WaitFlags {
        wait_on: Vec::new(),
        verify_on: Vec::new(),
        needs_verify: None,
        owner_ref: None,
        brief: None,
        auto: false,
        start_args: Vec::new(),
        clear_wait: false,
    };
    let mut extra: HashMap<String, Vec<String>> = HashMap::new();
    let now_epoch = epoch() as i64;

    let mut i = 0;
    let mut after_dd = false;
    while i < args.len() {
        let a = &args[i];
        if after_dd {
            wf.start_args.push(a.clone());
            i += 1;
            continue;
        }
        if a == "--" {
            after_dd = true;
            i += 1;
            continue;
        }
        let (key, inline) = if a.starts_with("--") {
            a.split_once('=')
                .map_or((a.as_str(), None), |(k, v)| (k, Some(v)))
        } else {
            (a.as_str(), None)
        };
        let take_val = |k: &str, idx: &mut usize| -> Res<String> {
            match inline {
                Some(v) => Ok(v.to_string()),
                None => {
                    *idx += 1;
                    args.get(*idx)
                        .cloned()
                        .ok_or_else(|| format!("argument {k}: expected one argument"))
                }
            }
        };
        match key {
            "--json" => json_mode = true,
            "--auto" => wf.auto = true,
            "--needs-verify" => wf.needs_verify = Some(true),
            "--no-needs-verify" if allow_clear => wf.needs_verify = Some(false),
            "--clear" if allow_clear => wf.clear_wait = true,
            "--brief" | "--prompt-file" => {
                wf.brief = Some(take_val(key, &mut i)?);
            }
            "--after-applied" => {
                let raw = take_val(key, &mut i)?;
                for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if let Some(wid) = part.strip_prefix("work_applied:") {
                        let norm = normalize_work_id(wid)
                            .ok_or_else(|| format!("invalid work id in --after-applied: '{part}'"))?;
                        wf.wait_on.push(format!("work_applied:{norm}"));
                    } else if parse_prefixed_num(part, 'W', 'w').is_some() {
                        let norm = normalize_work_id(part).unwrap();
                        wf.wait_on.push(format!("work_applied:{norm}"));
                    } else {
                        wf.wait_on.push(format!("run_applied:{part}"));
                    }
                }
            }
            "--after-delivered" => {
                let raw = take_val(key, &mut i)?;
                for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    wf.wait_on.push(format!("run_delivered:{part}"));
                }
            }
            "--after-work" => {
                let raw = take_val(key, &mut i)?;
                for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    let norm = normalize_work_id(part)
                        .ok_or_else(|| format!("invalid work id in --after-work: '{part}'"))?;
                    wf.wait_on.push(format!("work_closed:{norm}"));
                }
            }
            "--wait-owner" => {
                let raw = take_val(key, &mut i)?;
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    return Err("--wait-owner requires a non-empty ref (e.g. Q18, D002)".into());
                }
                let norm = normalize_work_id(trimmed).unwrap_or_else(|| trimmed.to_string());
                wf.owner_ref = Some(norm.clone());
                wf.wait_on.push(format!("owner:{norm}"));
            }
            "--after-time" => {
                let raw = take_val(key, &mut i)?;
                let iso_ts = parse_time_spec(&raw, now_epoch)?;
                wf.wait_on.push(format!("after_time:{iso_ts}"));
            }
            "--verify-on" => {
                let raw = take_val(key, &mut i)?;
                let cond = normalize_verify_condition(&raw)?;
                if !wf.verify_on.contains(&cond) {
                    wf.verify_on.push(cond);
                }
            }
            "--needs-owner"
            | "--class"
            | "--files"
            | "--done-when"
            | "--repo"
            | "--artifact"
            | "--followup"
            | "--run"
            | "--owner-kind"
            | "--qid"
            | "--id"
            | "--ask"
            | "--recommendation"
            | "--rec"
            | "--if-undecided"
            | "--cost" => {
                let raw = take_val(key, &mut i)?;
                extra.entry(key.to_string()).or_default().push(raw);
            }
            other if other.starts_with('-') => {
                return Err(format!("unrecognized arguments: {other}"));
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }

    Ok(ParsedWaitArgs {
        positional,
        wf,
        json_mode,
        extra,
    })
}

fn build_auto_dispatch(brief: &str, start_args: &[String], repo: &str) -> Res<AutoDispatch> {
    if brief.trim().is_empty() {
        return Err("--auto requires --brief <FILE> (or --prompt-file <FILE>)".into());
    }
    let p = PathBuf::from(brief);
    if !p.is_file() {
        return Err(format!("brief/prompt file does not exist: {brief}"));
    }
    let resolved = fs::canonicalize(&p).unwrap_or(p);
    let cwd = std::env::current_dir()
        .map(|d| fs::canonicalize(&d).unwrap_or(d))
        .unwrap_or_else(|_| PathBuf::from(repo));
    let runs_dir = std::env::var_os("DELEGATE_RUNS")
        .or_else(|| std::env::var_os("PI_DELEGATE_RUNS"))
        .filter(|v| !v.is_empty())
        .map(|v| {
            let p = PathBuf::from(v);
            fs::canonicalize(&p).unwrap_or(p).to_string_lossy().into_owned()
        })
        .unwrap_or_default();
    Ok(AutoDispatch {
        prompt_file: resolved.to_string_lossy().into_owned(),
        start_args: start_args.to_vec(),
        cwd: cwd.to_string_lossy().into_owned(),
        repo: repo.to_string(),
        runs_dir,
        caller: caller().unwrap_or_default(),
        dispatched_run: String::new(),
    })
}

struct ValidatedOwnerFields {
    ask: String,
    recommendation: String,
    if_undecided: String,
    cost: String,
}

fn validate_owner_fields(
    ask_raw: Option<&str>,
    rec_raw: Option<&str>,
    if_undecided_raw: Option<&str>,
    cost_raw: Option<&str>,
    title_to_differ: Option<&str>,
) -> Res<ValidatedOwnerFields> {
    let ask = ask_raw.map(str::trim).unwrap_or("");
    let rec = rec_raw.map(str::trim).unwrap_or("");
    let if_undecided = if_undecided_raw.map(str::trim).unwrap_or("");
    let cost = cost_raw.map(str::trim).unwrap_or("");

    let mut missing = Vec::new();
    if ask.is_empty() {
        missing.push("--ask");
    }
    if rec.is_empty() {
        missing.push("--recommendation");
    }
    if if_undecided.is_empty() {
        missing.push("--if-undecided");
    }
    if cost.is_empty() {
        missing.push("--cost");
    }
    if !missing.is_empty() {
        return Err(format!(
            "所有者事项缺少必填字段：{}",
            missing.join("、")
        ));
    }
    if let Some(t) = title_to_differ.map(str::trim).filter(|t| !t.is_empty()) {
        if ask == t {
            return Err("--ask 须写具体问句，不能直接等于标题".into());
        }
    }
    Ok(ValidatedOwnerFields {
        ask: ask.to_string(),
        recommendation: rec.to_string(),
        if_undecided: if_undecided.to_string(),
        cost: cost.to_string(),
    })
}

fn add_cmd(args: &[String]) -> Res<i32> {
    let ParsedWaitArgs {
        positional,
        wf,
        json_mode,
        extra,
    } = parse_wait_conditions(args, false)?;

    let owner_kind_raw = extra.get("--owner-kind").and_then(|v| v.last()).cloned();
    let explicit_id_raw = extra
        .get("--qid")
        .or_else(|| extra.get("--id"))
        .and_then(|v| v.last())
        .cloned();
    let ask_raw = extra.get("--ask").and_then(|v| v.last()).cloned();
    let rec_raw = extra
        .get("--recommendation")
        .or_else(|| extra.get("--rec"))
        .and_then(|v| v.last())
        .cloned();
    let if_undecided_raw = extra
        .get("--if-undecided")
        .and_then(|v| v.last())
        .cloned();
    let cost_raw = extra.get("--cost").and_then(|v| v.last()).cloned();

    let needs_owner_kind = match extra.get("--needs-owner").and_then(|v| v.last()) {
        Some(raw) => {
            let k = raw.trim();
            if !NEEDS_OWNER_KINDS.contains(&k) {
                return Err(format!(
                    "invalid --needs-owner '{k}' (expected: {})",
                    NEEDS_OWNER_KINDS.join("|")
                ));
            }
            if owner_kind_raw.is_some()
                || explicit_id_raw.as_deref().is_some_and(is_owner_id)
            {
                return Err("--needs-owner cannot be combined with --owner-kind".into());
            }
            Some(k.to_string())
        }
        None => None,
    };

    let is_owner_item = needs_owner_kind.is_none()
        && (owner_kind_raw.is_some()
            || ask_raw.is_some()
            || rec_raw.is_some()
            || if_undecided_raw.is_some()
            || cost_raw.is_some()
            || explicit_id_raw.as_deref().is_some_and(is_owner_id));

    let pos_title = positional.join(" ").trim().to_string();
    let (title, owner_info, needs_owner_fields) = if is_owner_item {
        let kind = owner_kind_raw
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("owner work item requires --owner-kind <act|nod|card|direction>")?;
        if !OWNER_KINDS.contains(&kind) {
            return Err(format!(
                "invalid --owner-kind '{kind}' (expected: {})",
                OWNER_KINDS.join("|")
            ));
        }
        if kind == "direction" {
            let ask = ask_raw
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| pos_title.clone());
            let final_title = if pos_title.is_empty() {
                ask.clone()
            } else {
                pos_title
            };
            if final_title.is_empty() {
                return Err("usage: delegate work add --owner-kind direction --ask \"<方向问题>\" [--qid D003] [--brief <path>]".into());
            }
            (
                final_title,
                Some(OwnerInfo {
                    kind: kind.to_string(),
                    ask,
                    recommendation: rec_raw.as_deref().map(str::trim).unwrap_or("").to_string(),
                    if_undecided: if_undecided_raw
                        .as_deref()
                        .map(str::trim)
                        .unwrap_or("")
                        .to_string(),
                    cost: cost_raw.as_deref().map(str::trim).unwrap_or("").to_string(),
                }),
                None,
            )
        } else {
            let validated = validate_owner_fields(
                ask_raw.as_deref(),
                rec_raw.as_deref(),
                if_undecided_raw.as_deref(),
                cost_raw.as_deref(),
                (!pos_title.is_empty()).then_some(pos_title.as_str()),
            )?;
            let final_title = if pos_title.is_empty() {
                validated.ask.clone()
            } else {
                pos_title
            };
            (
                final_title,
                Some(OwnerInfo {
                    kind: kind.to_string(),
                    ask: validated.ask,
                    recommendation: validated.recommendation,
                    if_undecided: validated.if_undecided,
                    cost: validated.cost,
                }),
                None,
            )
        }
    } else {
        if pos_title.is_empty() {
            return Err("usage: delegate work add \"<title>\" [--done-when \"…\"] [--brief <path>] [--after-applied <run>] [--wait-owner <Qxx>] [--needs-owner nod|act|card --ask \"…\" --recommendation \"…\" --if-undecided \"…\" --cost \"…\"] [--auto -- <start-args>]".into());
        }
        let no_fields = if needs_owner_kind.is_some() {
            Some(validate_owner_fields(
                ask_raw.as_deref(),
                rec_raw.as_deref(),
                if_undecided_raw.as_deref(),
                cost_raw.as_deref(),
                Some(pos_title.as_str()),
            )?)
        } else {
            None
        };
        (pos_title, None, no_fields)
    };

    let explicit_id = if let Some(raw_id) = explicit_id_raw {
        let norm = normalize_work_id(&raw_id).ok_or_else(|| {
            format!("invalid id '{raw_id}' (expected Qxx, Dxxx, or Wxxx)")
        })?;
        if owner_info.is_some() && !is_owner_id(&norm) {
            return Err(format!(
                "owner work item id must start with Q or D (got '{norm}')"
            ));
        }
        Some(norm)
    } else {
        None
    };

    let repo = extra
        .get("--repo")
        .and_then(|v| v.last())
        .map(|p| {
            let pb = PathBuf::from(p);
            fs::canonicalize(&pb).unwrap_or(pb).to_string_lossy().into_owned()
        })
        .unwrap_or_else(current_repo_str);
    let done_when = extra
        .get("--done-when")
        .and_then(|v| v.last())
        .cloned()
        .unwrap_or_default();
    let work_class = match extra.get("--class").and_then(|v| v.last()) {
        Some(raw) => validate_work_class(raw)?,
        None => String::new(),
    };
    let mut files: Vec<String> = Vec::new();
    if let Some(raw_files) = extra.get("--files") {
        for raw in raw_files {
            for part in raw.split(',') {
                let norm = crate::fill::normalize_rel_path(part);
                if !norm.is_empty() && !norm.contains('\0') && !files.contains(&norm) {
                    files.push(norm);
                }
            }
        }
    }
    let brief = wf.brief.unwrap_or_default();
    let auto_dispatch = if wf.auto {
        Some(build_auto_dispatch(&brief, &wf.start_args, &repo)?)
    } else {
        None
    };

    let state = state_dir();
    fs::create_dir_all(&state).map_err(|e| format!("{}: {e}", state.display()))?;
    touch_caller_session();
    let cur_caller = caller().unwrap_or_default();
    let now_iso = iso();
    let should_warn_owner_kw = owner_info.is_none()
        && wf.wait_on.is_empty()
        && needs_owner_kind.is_none()
        && matches_owner_wait_keyword(&title, &done_when);

    let extra_conds: Vec<(&str, &str)> = wf
        .wait_on
        .iter()
        .chain(wf.verify_on.iter())
        .map(|c| (repo.as_str(), c.as_str()))
        .collect();
    let prefetch = prefetch_reconcile_for_state(&state, &extra_conds);
    let (created_id, created_owner_id, outcome) = {
        let _guard = crate::locks::acquire_state(&state).map_err(|e| e.to_string())?;
        let mut items = load_raw(&state);
        let _ = migrate_legacy_locked(&state, &mut items);

        let mut wait_on = wf.wait_on;
        let mut owner_ref = wf.owner_ref.unwrap_or_default();
        let created_q = if let (Some(ref kind), Some(ref nf)) =
            (&needs_owner_kind, &needs_owner_fields)
        {
            let qid = next_owner_id(&items, kind);
            let q_item = WorkItem {
                id: qid.clone(),
                title: title.clone(),
                repo: repo.clone(),
                state: "attention".to_string(),
                owner: Some(OwnerInfo {
                    kind: kind.clone(),
                    ask: nf.ask.clone(),
                    recommendation: nf.recommendation.clone(),
                    if_undecided: nf.if_undecided.clone(),
                    cost: nf.cost.clone(),
                }),
                created_at: now_iso.clone(),
                updated_at: now_iso.clone(),
                state_since: now_iso.clone(),
                origin: "manual".into(),
                caller: cur_caller.clone(),
                ..Default::default()
            };
            items.push(q_item);
            let cond = format!("owner:{qid}");
            if !wait_on.contains(&cond) {
                wait_on.push(cond);
            }
            if owner_ref.is_empty() {
                owner_ref = qid.clone();
            }
            Some(qid)
        } else {
            None
        };

        let id = if let Some(ref eid) = explicit_id {
            if items.iter().any(|w| w.id == *eid) {
                return Err(format!("work item '{eid}' already exists"));
            }
            eid.clone()
        } else if let Some(ref ow) = owner_info {
            next_owner_id(&items, &ow.kind)
        } else {
            next_id_locked(&state, &items)
        };
        if has_work_cycle(&items, &id, &wait_on) {
            return Err("dependency cycle detected in --after-work / --after-applied".into());
        }

        let init_state = if !wait_on.is_empty() {
            "blocked"
        } else if let Some(ref ow) = owner_info {
            if ow.kind == "direction" {
                "active"
            } else {
                "attention"
            }
        } else {
            "ready"
        };
        let mut item = WorkItem {
            id: id.clone(),
            title,
            repo: repo.clone(),
            state: init_state.to_string(),
            class: work_class,
            files,
            done_when,
            needs_verify: wf.needs_verify.unwrap_or(false),
            wait_on,
            verify_on: wf.verify_on,
            auto_dispatch,
            brief,
            owner: owner_info,
            owner_ref,
            created_at: now_iso.clone(),
            updated_at: now_iso.clone(),
            state_since: now_iso.clone(),
            origin: "manual".into(),
            caller: cur_caller,
            ..Default::default()
        };

        if let Some(arts) = extra.get("--artifact") {
            for p in arts {
                if !p.trim().is_empty() {
                    item.artifacts.push(ArtifactItem {
                        path: p.trim().to_string(),
                        source_run: String::new(),
                        added_at: now_iso.clone(),
                        note: String::new(),
                    });
                }
            }
        }
        if let Some(fups) = extra.get("--followup") {
            for text in fups {
                let t = text.trim();
                if !t.is_empty() {
                    let fid = item.followups.len() + 1;
                    item.followups.push(FollowupItem {
                        id: fid,
                        kind: "note".into(),
                        text: t.to_string(),
                        anchor: String::new(),
                        source_run: String::new(),
                        done: false,
                        created_at: now_iso.clone(),
                        done_at: None,
                    });
                }
            }
        }
        if let Some(run_refs) = extra.get("--run") {
            let empty_dirs = BTreeMap::new();
            for r in run_refs {
                let rdir = find_run_dir(r, &empty_dirs, &state);
                let rid = rdir
                    .as_ref()
                    .map(|p| events::run_id_for(p, &json(p.join("meta.json"))))
                    .unwrap_or_else(|| r.clone());
                item.add_run(&rid, rdir.as_deref(), Some(&repo));
            }
        }

        items.push(item);
        let rec = reconcile_all_locked_with_prefetch(&state, &mut items, None, Some(&prefetch));
        save_raw(&state, &items)?;
        (id, created_q, rec)
    };

    let _ = emit_reconcile_events_and_dispatch(&state, &outcome);
    if should_warn_owner_kw {
        eprintln!("{OWNER_WAIT_WARNING}");
    }
    let items = load_raw(&state);
    let final_item = items.iter().find(|w| w.id == created_id).unwrap();
    let now_epoch = epoch() as i64;
    if json_mode {
        println!("{}", final_item.to_json());
    } else {
        println!(
            "已建工作项：[{}] {} · {} · 卡片：delegate work show {}",
            final_item.id,
            final_item.title,
            final_item.state,
            final_item.id
        );
        if !final_item.wait_on.is_empty() {
            let owner_note = created_owner_id
                .as_ref()
                .map(|qid| format!("（已建所有者项 [{qid}]）"))
                .unwrap_or_default();
            println!(
                "前置：{}{owner_note}",
                focus_for_item(final_item, &items, &state, now_epoch)
            );
        }
    }
    Ok(0)
}

fn wait_cmd(args: &[String]) -> Res<i32> {
    let ParsedWaitArgs {
        positional,
        wf,
        json_mode,
        extra,
    } = parse_wait_conditions(args, true)?;
    let raw_id = positional
        .first()
        .ok_or("usage: delegate work wait <Wxx|Qxx> --after-applied <run>|--after-time <TIME>|--needs-owner nod|act|card [--brief <FILE>] [--auto -- <start-args>]")?;
    let norm = normalize_work_id(raw_id)
        .ok_or_else(|| format!("invalid work id '{raw_id}' (expected W001... / Q01...)"))?;

    let needs_owner_kind = match extra.get("--needs-owner").and_then(|v| v.last()) {
        Some(raw) => {
            let k = raw.trim();
            if !NEEDS_OWNER_KINDS.contains(&k) {
                return Err(format!(
                    "invalid --needs-owner '{k}' (expected: {})",
                    NEEDS_OWNER_KINDS.join("|")
                ));
            }
            Some(k.to_string())
        }
        None => None,
    };
    let ask_raw = extra.get("--ask").and_then(|v| v.last()).cloned();
    let rec_raw = extra
        .get("--recommendation")
        .or_else(|| extra.get("--rec"))
        .and_then(|v| v.last())
        .cloned();
    let if_undecided_raw = extra
        .get("--if-undecided")
        .and_then(|v| v.last())
        .cloned();
    let cost_raw = extra.get("--cost").and_then(|v| v.last()).cloned();
    let class_update = match extra.get("--class").and_then(|v| v.last()) {
        Some(raw) => Some(validate_work_class(raw)?),
        None => None,
    };
    let files_update: Option<Vec<String>> = extra.get("--files").map(|raw_files| {
        let mut out = Vec::new();
        for raw in raw_files {
            for part in raw.split(',') {
                let norm = crate::fill::normalize_rel_path(part);
                if !norm.is_empty() && !norm.contains('\0') && !out.contains(&norm) {
                    out.push(norm);
                }
            }
        }
        out
    });
    let has_owner_field_updates = ask_raw.is_some()
        || rec_raw.is_some()
        || if_undecided_raw.is_some()
        || cost_raw.is_some();

    if !wf.clear_wait
        && wf.wait_on.is_empty()
        && wf.verify_on.is_empty()
        && wf.needs_verify.is_none()
        && wf.brief.is_none()
        && !wf.auto
        && !extra.contains_key("--artifact")
        && class_update.is_none()
        && files_update.is_none()
        && needs_owner_kind.is_none()
        && !has_owner_field_updates
    {
        return Err("work wait requires at least one condition (--class / --files / --after-applied / --after-delivered / --after-work / --wait-owner / --needs-owner / --after-time / --needs-verify / --verify-on / --brief / --ask / --recommendation / --if-undecided / --cost / --clear)".into());
    }

    let recorded_wait_on = !wf.wait_on.is_empty() || needs_owner_kind.is_some();
    let state = state_dir();
    touch_caller_session();
    let cur_caller = caller();
    let now_iso = iso();
    let wait_prefetch = {
        let existing = load_raw(&state);
        let target_repo = existing
            .iter()
            .find(|w| w.id == norm)
            .map(|w| w.repo.as_str())
            .unwrap_or("");
        let extra_conds: Vec<(&str, &str)> = wf
            .wait_on
            .iter()
            .chain(wf.verify_on.iter())
            .map(|c| (target_repo, c.as_str()))
            .collect();
        prefetch_reconcile_conditions(&state, &existing, &extra_conds)
    };
    let (created_owner_id, outcome) = {
        let _guard = crate::locks::acquire_state(&state).map_err(|e| e.to_string())?;
        let mut items = load_raw(&state);
        let _ = migrate_legacy_locked(&state, &mut items);
        let Some(pos) = items.iter().position(|w| w.id == norm) else {
            return Err(format!(
                "work item '{norm}' not found (list with: delegate work --all)"
            ));
        };
        if items[pos].state == "done" {
            return Err(format!("work item '{norm}' is already done"));
        }
        if !wf.wait_on.is_empty() && has_work_cycle(&items, &norm, &wf.wait_on) {
            return Err(format!("dependency cycle detected when updating '{norm}'"));
        }
        let repo = items[pos].repo.clone();
        let target_title = items[pos].title.clone();
        let brief_eff = wf
            .brief
            .clone()
            .unwrap_or_else(|| items[pos].brief.clone());
        let ad = if wf.auto {
            Some(build_auto_dispatch(&brief_eff, &wf.start_args, &repo)?)
        } else {
            None
        };

        let mut extra_wait_on: Vec<String> = Vec::new();
        let mut extra_owner_ref: Option<String> = None;
        let created_q = if let Some(ref kind) = needs_owner_kind {
            if items[pos].owner.is_some() {
                return Err("--needs-owner cannot be used on an owner item".into());
            }
            let validated = validate_owner_fields(
                ask_raw.as_deref(),
                rec_raw.as_deref(),
                if_undecided_raw.as_deref(),
                cost_raw.as_deref(),
                Some(target_title.as_str()),
            )?;
            let qid = next_owner_id(&items, kind);
            let q_item = WorkItem {
                id: qid.clone(),
                title: target_title,
                repo: repo.clone(),
                state: "attention".to_string(),
                owner: Some(OwnerInfo {
                    kind: kind.clone(),
                    ask: validated.ask,
                    recommendation: validated.recommendation,
                    if_undecided: validated.if_undecided,
                    cost: validated.cost,
                }),
                created_at: now_iso.clone(),
                updated_at: now_iso.clone(),
                state_since: now_iso.clone(),
                origin: "manual".into(),
                ..Default::default()
            };
            items.push(q_item);
            extra_wait_on.push(format!("owner:{qid}"));
            extra_owner_ref = Some(qid.clone());
            Some(qid)
        } else {
            if has_owner_field_updates {
                let Some(ow) = items[pos].owner.as_mut() else {
                    return Err("--ask / --recommendation / --if-undecided / --cost 仅用于所有者事项（Q/D）或配合 --needs-owner 使用".into());
                };
                if let Some(ref raw) = ask_raw {
                    let v = raw.trim();
                    if v.is_empty() {
                        return Err("--ask requires a non-empty question".into());
                    }
                    if ow.kind != "direction" && v == target_title.trim() {
                        return Err("--ask 须写具体问句，不能直接等于标题".into());
                    }
                    ow.ask = v.to_string();
                }
                if let Some(ref raw) = rec_raw {
                    let v = raw.trim();
                    if v.is_empty() {
                        return Err("--recommendation requires a non-empty value".into());
                    }
                    ow.recommendation = v.to_string();
                }
                if let Some(ref raw) = if_undecided_raw {
                    let v = raw.trim();
                    if v.is_empty() {
                        return Err("--if-undecided requires a non-empty value".into());
                    }
                    ow.if_undecided = v.to_string();
                }
                if let Some(ref raw) = cost_raw {
                    let v = raw.trim();
                    if v.is_empty() {
                        return Err("--cost requires a non-empty value".into());
                    }
                    ow.cost = v.to_string();
                }
            }
            None
        };

        let item = &mut items[pos];
        if wf.clear_wait {
            item.wait_on.clear();
            item.auto_dispatch = None;
        }
        for c in wf.wait_on.iter().chain(extra_wait_on.iter()) {
            if !item.wait_on.contains(c) {
                item.wait_on.push(c.clone());
            }
        }
        for c in &wf.verify_on {
            if !item.verify_on.contains(c) {
                item.verify_on.push(c.clone());
            }
        }
        if let Some(nv) = wf.needs_verify {
            item.needs_verify = nv;
        }
        if let Some(oref) = wf.owner_ref.or(extra_owner_ref) {
            item.owner_ref = oref;
        }
        if let Some(b) = wf.brief {
            item.brief = b;
        }
        if let Some(cls) = class_update {
            item.class = cls;
        }
        if let Some(fs_list) = files_update {
            item.files = fs_list;
        }
        if let Some(a) = ad {
            item.auto_dispatch = Some(a);
        }
        if let Some(arts) = extra.get("--artifact") {
            for p in arts {
                if !p.trim().is_empty() && !item.artifacts.iter().any(|x| x.path == p.trim()) {
                    item.artifacts.push(ArtifactItem {
                        path: p.trim().to_string(),
                        source_run: String::new(),
                        added_at: now_iso.clone(),
                        note: String::new(),
                    });
                }
            }
        }
        if !item.wait_on.is_empty() {
            item.defer_streak = 0;
            item.set_state("blocked", &now_iso);
        } else if wf.clear_wait && item.state == "blocked" {
            item.set_state("ready", &now_iso);
        }
        if let Some(c) = cur_caller {
            item.caller = c;
        }
        item.updated_at = now_iso;

        let rec = reconcile_all_locked_with_prefetch(&state, &mut items, None, Some(&wait_prefetch));
        save_raw(&state, &items)?;
        (created_q, rec)
    };

    let _ = emit_reconcile_events_and_dispatch(&state, &outcome);
    let items = load_raw(&state);
    let updated = items.iter().find(|w| w.id == norm).unwrap();
    if recorded_wait_on {
        crate::hook::record_action("wait_on", &updated.runs, Some(&updated.id), false);
    }
    let now_epoch = epoch() as i64;
    if json_mode {
        println!("{}", updated.to_json());
    } else {
        let owner_note = created_owner_id
            .as_ref()
            .map(|qid| format!("（已建所有者项 [{qid}]）"))
            .unwrap_or_default();
        println!(
            "已更新 [{}] {} · 状态：{} · {}{}（卡片：delegate work show {}）",
            updated.id,
            updated.title,
            updated.state,
            focus_for_item(updated, &items, &state, now_epoch),
            owner_note,
            updated.id
        );
    }
    Ok(0)
}

fn close_cmd(args: &[String]) -> Res<i32> {
    let mut id_raw: Option<String> = None;
    let mut evidence: Option<String> = None;
    let mut json_mode = crate::output::json_enabled();

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (key, inline) = a
            .split_once('=')
            .map_or((a.as_str(), None), |(k, v)| (k, Some(v)));
        match key {
            "--evidence" => {
                evidence = Some(match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --evidence: expected one argument")?
                    }
                });
            }
            "--json" => json_mode = true,
            other if other.starts_with('-') => {
                return Err(format!("unrecognized arguments: {other}"));
            }
            other => {
                if id_raw.is_none() {
                    id_raw = Some(other.to_string());
                } else {
                    return Err(format!("unexpected positional argument: {other}"));
                }
            }
        }
        i += 1;
    }

    let raw = id_raw.ok_or("usage: delegate work close <Wxx> --evidence \"<commit/测试/截图记录>\"")?;
    let norm = normalize_work_id(&raw)
        .ok_or_else(|| format!("invalid work id '{raw}' (expected W001...)"))?;
    let ev = evidence
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!("work close requires --evidence \"<依据>\" (e.g. delegate work close {norm} --evidence \"<sha>, 验证已通过\")")
        })?;

    let state = state_dir();
    let now_iso = iso();
    let prefetch = prefetch_reconcile_for_state(&state, &[]);
    let (closed_item, outcome) = {
        let _guard = crate::locks::acquire_state(&state).map_err(|e| e.to_string())?;
        let mut items = load_raw(&state);
        let _ = migrate_legacy_locked(&state, &mut items);
        let Some(pos) = items.iter().position(|w| w.id == norm) else {
            return Err(format!(
                "work item '{norm}' not found (list with: delegate work --all)"
            ));
        };
        items[pos].closed_kind = "closed".into();
        items[pos].close_evidence = ev;
        items[pos].defer_streak = 0;
        items[pos].set_state("done", &now_iso);
        let snap = items[pos].clone();
        let rec = reconcile_all_locked_with_prefetch(&state, &mut items, None, Some(&prefetch));
        save_raw(&state, &items)?;
        (snap, rec)
    };

    let dispatched = emit_reconcile_events_and_dispatch(&state, &outcome);
    if !closed_item.runs.is_empty() {
        events::mark_runs_read(&closed_item.runs);
    }
    crate::hook::record_action("close", &closed_item.runs, Some(&closed_item.id), false);
    if json_mode {
        println!("{}", closed_item.to_json());
    } else {
        println!(
            "已关闭 [{}] {} · done（证据：{} · 卡片：delegate work show {}）",
            closed_item.id, closed_item.title, closed_item.close_evidence, closed_item.id
        );
        for w in outcome.unlocked_ready.iter().take(3) {
            println!(
                "已解锁 [{}] {} → {}（卡片：delegate work show {}）",
                w.id,
                clip_chars(&w.title, 28),
                ready_command_for(w),
                w.id
            );
        }
        for (w, res) in dispatched.iter().take(3) {
            if let Ok(new_run) = res {
                println!(
                    "已自动派发 [{}] {} -> {}（卡片：delegate work show {}）",
                    w.id,
                    clip_chars(&w.title, 28),
                    new_run,
                    w.id
                );
            }
        }
    }
    Ok(0)
}

fn drop_cmd(args: &[String]) -> Res<i32> {
    let mut id_raw: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut json_mode = crate::output::json_enabled();

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (key, inline) = a
            .split_once('=')
            .map_or((a.as_str(), None), |(k, v)| (k, Some(v)));
        match key {
            "--reason" => {
                reason = Some(match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --reason: expected one argument")?
                    }
                });
            }
            "--json" => json_mode = true,
            other if other.starts_with('-') => {
                return Err(format!("unrecognized arguments: {other}"));
            }
            other => {
                if id_raw.is_none() {
                    id_raw = Some(other.to_string());
                } else {
                    return Err(format!("unexpected positional argument: {other}"));
                }
            }
        }
        i += 1;
    }

    let raw = id_raw.ok_or("usage: delegate work drop <Wxx> --reason \"<放弃理由>\"")?;
    let norm = normalize_work_id(&raw)
        .ok_or_else(|| format!("invalid work id '{raw}' (expected W001...)"))?;
    let r = reason
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!("work drop requires --reason \"<放弃理由>\" (e.g. delegate work drop {norm} --reason \"方向已取消\")")
        })?;

    let state = state_dir();
    let now_iso = iso();
    let (dropped_item, dependents) = {
        let _guard = crate::locks::acquire_state(&state).map_err(|e| e.to_string())?;
        let mut items = load_raw(&state);
        let _ = migrate_legacy_locked(&state, &mut items);
        let Some(pos) = items.iter().position(|w| w.id == norm) else {
            return Err(format!(
                "work item '{norm}' not found (list with: delegate work --all)"
            ));
        };
        items[pos].closed_kind = "dropped".into();
        items[pos].close_reason = r;
        items[pos].defer_streak = 0;
        items[pos].set_state("done", &now_iso);
        let snap = items[pos].clone();
        let deps: Vec<String> = items
            .iter()
            .filter(|w| {
                w.state != "done"
                    && w.wait_on.iter().any(|c| {
                        c == &format!("work_closed:{norm}")
                            || c == &format!("work_applied:{norm}")
                            || c.strip_prefix("owner:")
                                .and_then(normalize_work_id)
                                .as_deref()
                                == Some(norm.as_str())
                    })
            })
            .map(|w| w.id.clone())
            .collect();
        save_raw(&state, &items)?;
        (snap, deps)
    };

    if !dropped_item.runs.is_empty() {
        events::mark_runs_read(&dropped_item.runs);
    }
    if json_mode {
        println!("{}", dropped_item.to_json());
    } else {
        println!(
            "已放弃 [{}] {} · done/dropped（理由：{} · 卡片：delegate work show {}）",
            dropped_item.id, dropped_item.title, dropped_item.close_reason, dropped_item.id
        );
        if !dependents.is_empty() {
            println!(
                "提示：以下工作项依赖 {norm}，不会自动解锁，请人工确认：{}",
                dependents.join("、")
            );
        }
    }
    Ok(0)
}

fn defer_cmd(args: &[String]) -> Res<i32> {
    let mut target_raw: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut until_raw: Option<String> = None;
    let mut json_mode = crate::output::json_enabled();

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (key, inline) = a
            .split_once('=')
            .map_or((a.as_str(), None), |(k, v)| (k, Some(v)));
        match key {
            "--reason" => {
                reason = Some(match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --reason: expected one argument")?
                    }
                });
            }
            "--until" => {
                until_raw = Some(match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --until: expected one argument")?
                    }
                });
            }
            "--json" => json_mode = true,
            other if other.starts_with('-') => {
                return Err(format!("unrecognized arguments: {other}"));
            }
            other => {
                if target_raw.is_none() {
                    target_raw = Some(other.to_string());
                } else {
                    return Err(format!("unexpected positional argument: {other}"));
                }
            }
        }
        i += 1;
    }

    let raw = target_raw
        .ok_or("usage: delegate work defer <Wxx|run> --reason \"<暂缓原因>\" --until <4h|24h|YYYY-MM-DD|ISO>")?;
    let r = reason
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!("work defer requires --reason \"<原因>\" (e.g. delegate work defer {raw} --reason \"等上游窗口\" --until 4h)")
        })?;
    let u = until_raw
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!("work defer requires --until <4h|24h|YYYY-MM-DD|ISO> (e.g. delegate work defer {raw} --reason \"等上游窗口\" --until 4h)")
        })?;
    let now_epoch = epoch() as i64;
    let until_iso = parse_time_spec(&u, now_epoch)?;

    let state = state_dir();
    fs::create_dir_all(&state).map_err(|e| e.to_string())?;
    let now_iso = iso();

    let deferred_item = {
        let _guard = crate::locks::acquire_state(&state).map_err(|e| e.to_string())?;
        let mut items = load_raw(&state);
        let _ = migrate_legacy_locked(&state, &mut items);

        let pos = if let Some(norm) = normalize_work_id(&raw) {
            items.iter().position(|w| w.id == norm).ok_or_else(|| {
                format!("work item '{norm}' not found (list with: delegate work --all)")
            })?
        } else {
            // Resolve run and materialize into work.jsonl if not yet attached
            let run_ref = raw.strip_prefix("R:").unwrap_or(&raw);
            let empty_dirs = BTreeMap::new();
            let rdir = find_run_dir(run_ref, &empty_dirs, &state).ok_or_else(|| {
                format!("work item or run '{raw}' not found (list with: delegate work --all)")
            })?;
            let meta = json(rdir.join("meta.json"));
            let rid = events::run_id_for(&rdir, &meta);
            let repo = events::repo_for_meta(&meta);
            if let Some(p) = items.iter().position(|w| w.matches_run(&rid)) {
                p
            } else {
                let id = next_id_locked(&state, &items);
                let (chain_name, _) = notes::chain_display_info(&rdir);
                let mut w = WorkItem {
                    id,
                    title: chain_name,
                    repo: repo.clone(),
                    state: "deferred".into(),
                    created_at: now_iso.clone(),
                    updated_at: now_iso.clone(),
                    state_since: now_iso.clone(),
                    origin: "materialize".into(),
                    ..Default::default()
                };
                w.add_run(&rid, Some(&rdir), Some(&repo));
                items.push(w);
                items.len() - 1
            }
        };

        items[pos].defer_reason = r;
        items[pos].defer_until = until_iso;
        items[pos].defer_streak = items[pos].defer_streak.saturating_add(1);
        items[pos].set_state("deferred", &now_iso);
        let snap = items[pos].clone();
        save_raw(&state, &items)?;
        snap
    };

    if !deferred_item.runs.is_empty() {
        events::mark_runs_read(&deferred_item.runs);
    }
    crate::hook::record_action("defer", &deferred_item.runs, Some(&deferred_item.id), false);
    if json_mode {
        println!("{}", deferred_item.to_json());
    } else {
        println!(
            "已暂缓 [{}] {} · deferred 至 {}（原因：{} · 卡片：delegate work show {}）",
            deferred_item.id,
            deferred_item.title,
            deferred_item.defer_until,
            deferred_item.defer_reason,
            deferred_item.id
        );
    }
    Ok(0)
}

fn import_md_cmd(args: &[String]) -> Res<i32> {
    let mut file_raw: Option<String> = None;
    let mut dry_run = false;
    let mut include_stale = false;
    let mut repo_override: Option<String> = None;
    let mut json_mode = crate::output::json_enabled();

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (key, inline) = a
            .split_once('=')
            .map_or((a.as_str(), None), |(k, v)| (k, Some(v)));
        match key {
            "--dry-run" => dry_run = true,
            "--include-stale" => include_stale = true,
            "--json" => json_mode = true,
            "--repo" => {
                repo_override = Some(match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --repo: expected one argument")?
                    }
                });
            }
            other if other.starts_with('-') => {
                return Err(format!("unrecognized arguments: {other}"));
            }
            other => {
                if file_raw.is_none() {
                    file_raw = Some(other.to_string());
                } else {
                    return Err(format!("unexpected positional argument: {other}"));
                }
            }
        }
        i += 1;
    }

    let raw = file_raw.ok_or("usage: delegate work import-md <FILE> [--dry-run] [--repo <DIR>] [--json]")?;
    let file_path = PathBuf::from(&raw);
    if !file_path.is_file() {
        return Err(format!("markdown file does not exist: {raw}"));
    }
    let canon_file = fs::canonicalize(&file_path).unwrap_or(file_path);
    let inferred_repo = repo_override
        .map(|r| {
            let p = PathBuf::from(r);
            fs::canonicalize(&p).unwrap_or(p).to_string_lossy().into_owned()
        })
        .or_else(|| {
            canon_file
                .parent()
                .and_then(git_top)
                .map(|p| fs::canonicalize(&p).unwrap_or(p).to_string_lossy().into_owned())
        })
        .unwrap_or_else(current_repo_str);

    let state = state_dir();
    let existing = load_readonly_in(&state);
    let drafts = parse_markdown_backlog(&canon_file, &state, Some(&inferred_repo), &existing)?;
    let file_str = canon_file.to_string_lossy();

    if dry_run {
        let mut ready_cnt = 0usize;
        let mut blocked_cnt = 0usize;
        let mut owner_cnt = 0usize;
        let mut time_cnt = 0usize;
        let mut stale_cnt = 0usize;
        let mut dup_cnt = 0usize;
        for d in &drafts {
            if d.duplicate_of.is_some() {
                dup_cnt += 1;
            }
            if d.suspected_stale.is_some() {
                stale_cnt += 1;
            }
            if d.state == "ready" {
                ready_cnt += 1;
            } else if d.state == "blocked" {
                blocked_cnt += 1;
                if !d.owner_ref.is_empty() {
                    owner_cnt += 1;
                }
                if d.wait_on.iter().any(|w| w.starts_with("after_time:")) {
                    time_cnt += 1;
                }
            }
        }

        if json_mode {
            for d in &drafts {
                println!("{}", d.to_json(&file_str));
            }
            println!(
                "{}",
                json!({
                    "dryRun": true,
                    "file": file_str,
                    "repo": inferred_repo,
                    "total": drafts.len(),
                    "ready": ready_cnt,
                    "blocked": blocked_cnt,
                    "ownerBlocked": owner_cnt,
                    "timeBlocked": time_cnt,
                    "suspectedStale": stale_cnt,
                    "duplicates": dup_cnt,
                })
            );
            return Ok(0);
        }

        println!(
            "导入预览（dry-run，未写入）：共 {} 条 · 就绪 {} · 等待 {}（含所有者 {} / 定时 {}）· 疑似已过期/已合入 {} · 已存在 {}",
            drafts.len(),
            ready_cnt,
            blocked_cnt,
            owner_cnt,
            time_cnt,
            stale_cnt,
            dup_cnt
        );
        println!("源文件：{}（归属仓库：{}）", file_str, short_repo_name(&inferred_repo));
        for (i, d) in drafts.iter().enumerate() {
            let mut tags = Vec::new();
            if let Some(dup) = &d.duplicate_of {
                tags.push(format!("已存在 {dup}"));
            }
            if let Some(stale) = &d.suspected_stale {
                tags.push(format!("疑似过期：{stale}"));
            }
            if !d.wait_on.is_empty() {
                tags.push(format!("前置={}", d.wait_on.join(",")));
            }
            if !d.brief.is_empty() {
                tags.push(format!("说明={}", d.brief));
            }
            let tag_s = if tags.is_empty() {
                String::new()
            } else {
                format!(" · {}", tags.join(" · "))
            };
            println!(
                "{:2}. [{}] {} · {}（{}:{}）{}",
                i + 1,
                d.state,
                d.title,
                if d.section.is_empty() { "-" } else { d.section.as_str() },
                file_str,
                d.line_no,
                tag_s
            );
        }
        println!(
            "确认写入：delegate work import-md {}（写入后请人工核对再删除原文件）",
            shell_quote(&file_str)
        );
        return Ok(0);
    }

    // Actual import
    fs::create_dir_all(&state).map_err(|e| e.to_string())?;
    let now_iso = iso();
    let mut created_ids = Vec::new();
    let mut skipped = 0usize;

    let import_extra: Vec<(&str, &str)> = drafts
        .iter()
        .flat_map(|d| d.wait_on.iter().map(|c| (inferred_repo.as_str(), c.as_str())))
        .collect();
    let prefetch = prefetch_reconcile_for_state(&state, &import_extra);
    {
        let _guard = crate::locks::acquire_state(&state).map_err(|e| e.to_string())?;
        let mut items = load_raw(&state);
        let _ = migrate_legacy_locked(&state, &mut items);

        for d in &drafts {
            if d.duplicate_of.is_some() || (d.suspected_stale.is_some() && !include_stale) {
                skipped += 1;
                continue;
            }
            let id = next_id_locked(&state, &items);
            let item = WorkItem {
                id: id.clone(),
                title: d.title.clone(),
                repo: inferred_repo.clone(),
                state: d.state.clone(),
                done_when: d.done_when.clone(),
                wait_on: d.wait_on.clone(),
                brief: d.brief.clone(),
                owner_ref: d.owner_ref.clone(),
                created_at: now_iso.clone(),
                updated_at: now_iso.clone(),
                state_since: now_iso.clone(),
                origin: format!("import-md:{}:{}", file_str, d.line_no),
                ..Default::default()
            };
            items.push(item);
            created_ids.push(id);
        }
        let _ = reconcile_all_locked_with_prefetch(&state, &mut items, None, Some(&prefetch));
        save_raw(&state, &items)?;
    }

    if json_mode {
        println!(
            "{}",
            json!({
                "imported": created_ids,
                "skipped": skipped,
                "file": file_str,
            })
        );
    } else {
        println!(
            "已导入 {} 条工作项（跳过 {} 条重复/疑似过期）：{} · 查看：delegate work --all",
            created_ids.len(),
            skipped,
            if created_ids.is_empty() {
                "-".to_string()
            } else {
                created_ids.join(", ")
            }
        );
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// `delegate work import-dag <FILE> [--dry-run] [--repo <DIR>] [--json]`
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct DagImportItem {
    key: String,
    title: String,
    class: String,
    brief_raw: String,
    brief_resolved: String,
    done_when: String,
    after: Vec<String>,
    files: Vec<String>,
    tier: String,
    model: String,
    thinking: String,
}

impl DagImportItem {
    fn start_args(&self) -> Vec<String> {
        let mut args = vec!["--worktree".to_string()];
        if !self.tier.is_empty() {
            args.push("--tier".to_string());
            args.push(self.tier.clone());
        }
        if !self.model.is_empty() {
            args.push("--model".to_string());
            args.push(self.model.clone());
        }
        if !self.thinking.is_empty() {
            args.push("--thinking".to_string());
            args.push(self.thinking.clone());
        }
        args
    }
}

fn resolve_dag_brief_path(raw: &str, repo: &str, base_dir: Option<&Path>) -> Option<PathBuf> {
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        return p.is_file().then(|| fs::canonicalize(&p).unwrap_or(p));
    }
    if p.is_file() {
        return Some(fs::canonicalize(&p).unwrap_or(p));
    }
    if !repo.is_empty() {
        let r = Path::new(repo).join(raw);
        if r.is_file() {
            return Some(fs::canonicalize(&r).unwrap_or(r));
        }
    }
    if let Some(bd) = base_dir {
        let b = bd.join(raw);
        if b.is_file() {
            return Some(fs::canonicalize(&b).unwrap_or(b));
        }
    }
    None
}

fn parse_dag_import_json(
    doc: &Value,
    repo: &str,
    base_dir: Option<&Path>,
    existing_ids: &HashSet<String>,
) -> Res<Vec<DagImportItem>> {
    let Some(arr) = doc.as_object().and_then(|o| o.get("items")).and_then(Value::as_array) else {
        return Err("import-dag: JSON must be an object with an \"items\" array".into());
    };
    if arr.is_empty() {
        return Err("import-dag: \"items\" array is empty".into());
    }

    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut parsed: Vec<DagImportItem> = Vec::with_capacity(arr.len());

    for (idx, raw_item) in arr.iter().enumerate() {
        let Some(obj) = raw_item.as_object() else {
            return Err(format!("import-dag: items[{idx}] must be an object"));
        };
        let key = obj
            .get("key")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if key.is_empty() {
            return Err(format!("import-dag: items[{idx}] missing non-empty \"key\""));
        }
        if !seen_keys.insert(key.to_string()) {
            return Err(format!("import-dag: duplicate key '{key}'"));
        }
        let title = obj
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if title.is_empty() {
            return Err(format!("import-dag: item '{key}' missing non-empty \"title\""));
        }
        let class = match obj.get("class") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(raw)) => {
                if raw.trim().is_empty() {
                    String::new()
                } else {
                    validate_work_class(raw)
                        .map_err(|e| format!("import-dag: item '{key}': {e}"))?
                }
            }
            Some(_) => {
                return Err(format!(
                    "import-dag: item '{key}': \"class\" must be a string ({})",
                    WORK_CLASSES.join("|")
                ))
            }
        };
        let brief_raw = obj
            .get("brief")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if brief_raw.is_empty() {
            return Err(format!("import-dag: item '{key}' missing non-empty \"brief\" file path"));
        }
        let Some(brief_path) = resolve_dag_brief_path(brief_raw, repo, base_dir) else {
            return Err(format!(
                "import-dag: item '{key}': brief file does not exist: {brief_raw}"
            ));
        };
        let done_when = match obj.get("doneWhen") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.trim().to_string(),
            Some(_) => {
                return Err(format!(
                    "import-dag: item '{key}': \"doneWhen\" must be a string"
                ))
            }
        };
        let mut after: Vec<String> = Vec::new();
        if let Some(av) = obj.get("after").filter(|v| !v.is_null()) {
            let Some(a_arr) = av.as_array() else {
                return Err(format!(
                    "import-dag: item '{key}': \"after\" must be an array of keys"
                ));
            };
            for dep in a_arr {
                let Some(ds) = dep.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
                    return Err(format!(
                        "import-dag: item '{key}': \"after\" must contain non-empty key strings"
                    ));
                };
                if !after.iter().any(|x| x == ds) {
                    after.push(ds.to_string());
                }
            }
        }
        let mut files: Vec<String> = Vec::new();
        if let Some(fv) = obj.get("files").filter(|v| !v.is_null()) {
            let Some(f_arr) = fv.as_array() else {
                return Err(format!(
                    "import-dag: item '{key}': \"files\" must be an array of path prefixes"
                ));
            };
            for f in f_arr {
                let Some(fs_raw) = f.as_str() else {
                    return Err(format!(
                        "import-dag: item '{key}': \"files\" must contain non-empty path strings"
                    ));
                };
                let norm = crate::fill::normalize_rel_path(fs_raw);
                if norm.is_empty() || fs_raw.contains('\0') {
                    return Err(format!(
                        "import-dag: item '{key}': \"files\" contains an empty or invalid path"
                    ));
                }
                if !files.contains(&norm) {
                    files.push(norm);
                }
            }
        }
        let tier = match obj.get("tier") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(t)) => {
                let tt = t.trim();
                if !tt.is_empty() && !crate::agents::ALL_TIERS.contains(&tt) {
                    return Err(format!(
                        "import-dag: item '{key}': invalid tier '{tt}' (expected: scout|review|write)"
                    ));
                }
                tt.to_string()
            }
            Some(_) => return Err(format!("import-dag: item '{key}': \"tier\" must be a string")),
        };
        let model = match obj.get("model") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(m)) => {
                let mm = m.trim();
                if !mm.is_empty() {
                    crate::models::validate_model(mm)
                        .map_err(|e| format!("import-dag: item '{key}': {e}"))?;
                }
                mm.to_string()
            }
            Some(_) => return Err(format!("import-dag: item '{key}': \"model\" must be a string")),
        };
        let thinking = match obj.get("thinking") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(th)) => {
                let tt = th.trim();
                if !tt.is_empty() {
                    crate::models::validate_thinking(tt)
                        .map_err(|e| format!("import-dag: item '{key}': {e}"))?;
                }
                tt.to_string()
            }
            Some(_) => {
                return Err(format!(
                    "import-dag: item '{key}': \"thinking\" must be a string"
                ))
            }
        };

        parsed.push(DagImportItem {
            key: key.to_string(),
            title: title.to_string(),
            class,
            brief_raw: brief_raw.to_string(),
            brief_resolved: brief_path.to_string_lossy().into_owned(),
            done_when,
            after,
            files,
            tier,
            model,
            thinking,
        });
    }

    // Validate every `after` reference exists either in the batch or in existing work items.
    let key_to_idx: HashMap<&str, usize> = parsed
        .iter()
        .enumerate()
        .map(|(i, it)| (it.key.as_str(), i))
        .collect();
    for item in &parsed {
        for dep in &item.after {
            if key_to_idx.contains_key(dep.as_str()) {
                continue;
            }
            if normalize_work_id(dep).is_some_and(|id| existing_ids.contains(&id)) {
                continue;
            }
            return Err(format!(
                "import-dag: item '{}' references unknown dependency '{}'",
                item.key, dep
            ));
        }
    }

    // 3-color DFS cycle detection across batch keys (rejects self-loops and any cycle).
    let mut color = vec![0u8; parsed.len()]; // 0=white, 1=gray, 2=black
    fn dfs_cycle(
        u: usize,
        parsed: &[DagImportItem],
        key_to_idx: &HashMap<&str, usize>,
        color: &mut [u8],
    ) -> Option<String> {
        color[u] = 1;
        for dep in &parsed[u].after {
            if let Some(&v) = key_to_idx.get(dep.as_str()) {
                if color[v] == 1 {
                    return Some(format!("{} -> {}", parsed[u].key, parsed[v].key));
                }
                if color[v] == 0 {
                    if let Some(cyc) = dfs_cycle(v, parsed, key_to_idx, color) {
                        return Some(cyc);
                    }
                }
            }
        }
        color[u] = 2;
        None
    }
    for i in 0..parsed.len() {
        if color[i] == 0 {
            if let Some(cyc) = dfs_cycle(i, &parsed, &key_to_idx, &mut color) {
                return Err(format!("import-dag: dependency cycle detected ({cyc})"));
            }
        }
    }

    Ok(parsed)
}

fn import_dag_cmd(args: &[String]) -> Res<i32> {
    let mut file_path: Option<String> = None;
    let mut dry_run = false;
    let mut repo_arg: Option<String> = None;
    let mut json_mode = crate::output::json_enabled();

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (key, inline) = a
            .split_once('=')
            .map_or((a.as_str(), None), |(k, v)| (k, Some(v)));
        match key {
            "--dry-run" => dry_run = true,
            "--json" => json_mode = true,
            "--repo" => {
                let v = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --repo: expected one argument")?
                    }
                };
                repo_arg = Some(v);
            }
            other if other.starts_with('-') => {
                return Err(format!("unrecognized arguments: {other}"));
            }
            other => {
                if file_path.is_none() {
                    file_path = Some(other.to_string());
                } else {
                    return Err(format!("unexpected positional argument: {other}"));
                }
            }
        }
        i += 1;
    }

    let file_str =
        file_path.ok_or("usage: delegate work import-dag <FILE> [--dry-run] [--repo <DIR>] [--json]")?;
    let p = PathBuf::from(&file_str);
    if !p.is_file() {
        return Err(format!("file not found: {file_str}"));
    }
    let content = fs::read_to_string(&p).map_err(|e| format!("{file_str}: {e}"))?;
    let doc: Value =
        serde_json::from_str(&content).map_err(|e| format!("{file_str}: invalid JSON: {e}"))?;

    let repo = repo_arg
        .map(|r| {
            let pb = PathBuf::from(r);
            fs::canonicalize(&pb)
                .unwrap_or(pb)
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(current_repo_str);

    let state = state_dir();
    let existing_ids: HashSet<String> = load_raw(&state).into_iter().map(|w| w.id).collect();
    let base_dir = p.parent().filter(|d| !d.as_os_str().is_empty());
    let dag_items = parse_dag_import_json(&doc, &repo, base_dir, &existing_ids)?;

    let edges: Vec<(String, String)> = dag_items
        .iter()
        .flat_map(|it| it.after.iter().map(|dep| (dep.clone(), it.key.clone())))
        .collect();

    if dry_run {
        if json_mode {
            println!(
                "{}",
                json!({
                    "dryRun": true,
                    "file": file_str,
                    "repo": repo,
                    "items": dag_items.iter().map(|it| json!({
                        "key": it.key,
                        "title": it.title,
                        "class": it.class,
                        "brief": it.brief_resolved,
                        "doneWhen": it.done_when,
                        "after": it.after,
                        "files": it.files,
                        "startArgs": it.start_args(),
                    })).collect::<Vec<_>>(),
                    "edges": edges.iter().map(|(from, to)| json!({
                        "from": from,
                        "to": to,
                    })).collect::<Vec<_>>(),
                })
            );
        } else {
            println!(
                "预览导入（--dry-run，未写入）：{} 项 · {} 条依赖边（{}）",
                dag_items.len(),
                edges.len(),
                file_str
            );
            for it in &dag_items {
                let cls = if it.class.is_empty() {
                    "缺类别"
                } else {
                    it.class.as_str()
                };
                let files_tag = if it.files.is_empty() {
                    String::new()
                } else {
                    format!(" · files={}", it.files.join(","))
                };
                let after_tag = if it.after.is_empty() {
                    "ready".to_string()
                } else {
                    format!("after={}", it.after.join(","))
                };
                println!(
                    "  [{}] {} · {} · {}{} · brief={} · auto={}",
                    it.key,
                    it.title,
                    cls,
                    after_tag,
                    files_tag,
                    it.brief_raw,
                    it.start_args().join(" ")
                );
            }
            if !edges.is_empty() {
                println!("依赖边：");
                for (from, to) in &edges {
                    println!("  {from} -> {to}");
                }
            }
        }
        return Ok(0);
    }

    fs::create_dir_all(&state).map_err(|e| format!("{}: {e}", state.display()))?;
    touch_caller_session();
    let cur_caller = caller().unwrap_or_default();
    let now_iso = iso();
    let prefetch = prefetch_reconcile_for_state(&state, &[]);

    let (created_records, outcome) = {
        let _guard = crate::locks::acquire_state(&state).map_err(|e| e.to_string())?;
        let mut items = load_raw(&state);
        let _ = migrate_legacy_locked(&state, &mut items);

        // Pre-allocate sequential W-IDs for every item in the batch so intra-batch `after`
        // references resolve cleanly regardless of declaration order.
        let mut key_to_id: HashMap<String, String> = HashMap::with_capacity(dag_items.len());
        let mut allocated: Vec<(String, String)> = Vec::with_capacity(dag_items.len());
        let mut dummy_items = items.clone();
        for it in &dag_items {
            let id = next_id_locked(&state, &dummy_items);
            dummy_items.push(WorkItem {
                id: id.clone(),
                ..Default::default()
            });
            key_to_id.insert(it.key.clone(), id.clone());
            allocated.push((it.key.clone(), id));
        }

        let mut records: Vec<Value> = Vec::with_capacity(dag_items.len());
        for (it, (_, id)) in dag_items.iter().zip(allocated.iter()) {
            let mut wait_on: Vec<String> = Vec::new();
            for dep in &it.after {
                let target_id = key_to_id
                    .get(dep)
                    .cloned()
                    .or_else(|| normalize_work_id(dep))
                    .ok_or_else(|| format!("import-dag: unresolved dependency '{dep}'"))?;
                let cond = format!("work_closed:{target_id}");
                if !wait_on.contains(&cond) {
                    wait_on.push(cond);
                }
            }
            let init_state = if wait_on.is_empty() {
                "ready"
            } else {
                "blocked"
            };
            let ad = build_auto_dispatch(&it.brief_resolved, &it.start_args(), &repo)?;
            let wi = WorkItem {
                id: id.clone(),
                title: it.title.clone(),
                repo: repo.clone(),
                state: init_state.to_string(),
                class: it.class.clone(),
                files: it.files.clone(),
                done_when: it.done_when.clone(),
                wait_on: wait_on.clone(),
                auto_dispatch: Some(ad),
                brief: it.brief_resolved.clone(),
                created_at: now_iso.clone(),
                updated_at: now_iso.clone(),
                state_since: now_iso.clone(),
                origin: format!("import-dag:{}:{}", file_str, it.key),
                caller: cur_caller.clone(),
                ..Default::default()
            };
            records.push(json!({
                "key": it.key,
                "id": id,
                "title": it.title,
                "class": it.class,
                "state": init_state,
                "waitOn": wait_on,
                "files": it.files,
                "brief": it.brief_resolved,
            }));
            items.push(wi);
        }

        let rec = reconcile_all_locked_with_prefetch(&state, &mut items, None, Some(&prefetch));
        save_raw(&state, &items)?;
        (records, rec)
    };

    let _ = emit_reconcile_events_and_dispatch(&state, &outcome);

    if json_mode {
        let mapped_edges: Vec<Value> = {
            let key_map: HashMap<&str, &str> = created_records
                .iter()
                .map(|r| (s(r, "key"), s(r, "id")))
                .collect();
            edges
                .iter()
                .map(|(from, to)| {
                    json!({
                        "fromKey": from,
                        "toKey": to,
                        "fromId": key_map.get(from.as_str()).copied().unwrap_or(from.as_str()),
                        "toId": key_map.get(to.as_str()).copied().unwrap_or(to.as_str()),
                    })
                })
                .collect()
        };
        println!(
            "{}",
            json!({
                "file": file_str,
                "repo": repo,
                "imported": created_records,
                "edges": mapped_edges,
            })
        );
    } else {
        let ids: Vec<&str> = created_records.iter().map(|r| s(r, "id")).collect();
        println!(
            "已导入 DAG {} 条工作项（{} 条依赖边）：{} · 查看：delegate work --all",
            created_records.len(),
            edges.len(),
            ids.join(", ")
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "delegate-work-test-{tag}-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn owner_readme_scan_is_read_only_and_detects_open_resolved_rejected() {
        let repo = temp_dir("owner");
        let owner_dir = repo.join("docs/owner");
        fs::create_dir_all(&owner_dir).unwrap();
        let readme = owner_dir.join("README.md");
        let content = "# 所有者案头\n\
            \n\
            ## 要你动手\n\
            | 编号 | 做什么 |\n\
            | --- | --- |\n\
            | Q18 | 删掉两条记忆 |\n\
            \n\
            ## 方向\n\
            | 编号 | 说明 |\n\
            | --- | --- |\n\
            | [D002](directions/streaming-motion.md) | 流式驱动 |\n\
            \n\
            ## 已否\n\
            | 日期 | 编号 | 事项 |\n\
            | --- | --- | --- |\n\
            | 2026-11-23 | Q19 | 买直连 |\n\
            \n\
            ---\n\
            主控维护规则：编号（Q、D）只增不复用。\n";
        fs::write(&readme, content).unwrap();

        assert_eq!(check_owner_readme(&repo, "Q18"), OwnerStatus::Waiting);
        assert_eq!(check_owner_readme(&repo, "D002"), OwnerStatus::Waiting);
        assert_eq!(check_owner_readme(&repo, "Q19"), OwnerStatus::Rejected);
        assert_eq!(check_owner_readme(&repo, "Q17"), OwnerStatus::Resolved);
        // Verify file was never modified
        assert_eq!(fs::read_to_string(&readme).unwrap(), content);

        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn work_closed_cycle_detection() {
        let items = vec![
            WorkItem {
                id: "W001".into(),
                wait_on: vec!["work_closed:W002".into()],
                ..Default::default()
            },
            WorkItem {
                id: "W002".into(),
                wait_on: vec!["work_closed:W003".into()],
                ..Default::default()
            },
        ];
        assert!(has_work_cycle(&items, "W003", &["work_closed:W001".into()]));
        assert!(has_work_cycle(&items, "W001", &["work_closed:W001".into()]));
        assert!(!has_work_cycle(&items, "W004", &["work_closed:W001".into()]));
    }

    #[test]
    fn table_row_respects_100_char_budget_and_contains_6_columns() {
        let row = TableRow {
            id: "W019".into(),
            repo: "demo-app".into(),
            title: "3D 默认待机动作与默认书房场景上线以及很长很长的标题测试截断".into(),
            state: "ready".into(),
            focus: "上游 007d 已合入".into(),
            next: "start --work W019 --prompt-file .local/plan/brief-body-idle-scene.md".into(),
            is_ephemeral: false,
            run_id: String::new(),
            missing_class: false,
        };
        let rendered = format_table_row(&row);
        assert!(rendered.chars().count() <= 100, "row too long: {rendered}");
        assert!(rendered.contains("[W019]"));
        assert!(rendered.contains("[demo-app]"));
        assert!(rendered.contains("ready"));
        assert!(rendered.contains("下一步："));
    }

    #[test]
    fn time_spec_parses_durations_dates_and_iso() {
        let base = iso_epoch("2026-11-23T00:00:00Z").unwrap();
        assert_eq!(
            parse_time_spec("4h", base).unwrap(),
            "2026-11-23T04:00:00Z"
        );
        assert_eq!(
            parse_time_spec("2026-11-30", base).unwrap(),
            "2026-11-30T00:00:00Z"
        );
        assert_eq!(
            parse_time_spec("2026-11-30T12:30:00Z", base).unwrap(),
            "2026-11-30T12:30:00Z"
        );
        assert!(parse_time_spec("not-a-time", base).is_err());
    }

    #[test]
    fn import_md_parses_sections_refs_dates_and_briefs() {
        let dir = temp_dir("import");
        let state = dir.join("state");
        fs::create_dir_all(&state).unwrap();
        let md = dir.join("backlog.md");
        fs::write(
            &md,
            "# 待办\n\
             ## 等前置条件\n\
             - 3D 顶层空间（007d 链）合入后：派 .local/plan/brief-body-idle-scene.md\n\
             ## 主控自己做\n\
             - 观察影子：2099-11-30 或 12-31 跑报告\n\
             ## 可派（排队）\n\
             - 清理旧兼容列\n\
             ## 要所有者定\n\
             - D002：身体第二步是否改流式驱动\n",
        )
        .unwrap();

        let drafts = parse_markdown_backlog(&md, &state, None, &[]).unwrap();
        assert_eq!(drafts.len(), 4);
        assert_eq!(drafts[0].state, "blocked");
        assert!(drafts[0].wait_on.contains(&"run_applied:007d".to_string()));
        assert_eq!(drafts[0].brief, ".local/plan/brief-body-idle-scene.md");
        assert_eq!(drafts[2].state, "ready");
        assert_eq!(drafts[3].owner_ref, "D002");
        assert!(drafts[3].wait_on.contains(&"owner:D002".to_string()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_legacy_notes_and_deferred_is_idempotent() {
        let dir = temp_dir("migrate");
        let state = dir.join("state");
        let repo = dir.join("repo");
        let runs_dir = repo.join(".local/run/delegate");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&runs_dir).unwrap();
        fs::create_dir_all(repo.join(".local/delegate")).unwrap();

        // Register repo
        notes::register_repo_paths(&state, &repo, &runs_dir);

        // Write legacy notes.json
        write_json(
            runs_dir.join("notes.json"),
            &json!({
                "chains": [{
                    "root": "20261122-100000-r1",
                    "runs": ["20261122-100000-r1"],
                    "name": "旧待办任务",
                    "latestRun": "20261122-100000-r1",
                    "cleaned": true,
                    "applied": true,
                    "items": [{
                        "text": "核对线上日志",
                        "createdAt": "2026-11-22T02:00:00Z",
                        "done": false
                    }]
                }]
            }),
        )
        .unwrap();

        // Write legacy deferred.tsv (fresh)
        let now_s = epoch() as i64;
        fs::write(
            repo.join(".local/delegate/deferred.tsv"),
            format!("20261123-100000-r2\t等发布窗口\t{now_s}\n"),
        )
        .unwrap();

        let mut items = Vec::new();
        assert!(migrate_legacy_locked(&state, &mut items));
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|w| w.state == "verify" && w.title == "旧待办任务" && w.pending_followups() == 1));
        assert!(items.iter().any(|w| w.state == "deferred" && w.defer_reason == "等发布窗口"));

        // Second pass is idempotent (no duplicates!)
        assert!(!migrate_legacy_locked(&state, &mut items));
        assert_eq!(items.len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn owner_work_item_roundtrip_id_normalization_and_unlock_on_close() {
        assert_eq!(normalize_work_id("w5").as_deref(), Some("W005"));
        assert_eq!(normalize_work_id("q7").as_deref(), Some("Q07"));
        assert_eq!(normalize_work_id("Q18").as_deref(), Some("Q18"));
        assert_eq!(normalize_work_id("d2").as_deref(), Some("D002"));
        assert_eq!(normalize_work_id("D002").as_deref(), Some("D002"));
        assert_eq!(normalize_work_id("X1"), None);

        let dir = temp_dir("owner-work");
        let state = dir.join("state");
        fs::create_dir_all(&state).unwrap();

        let owner_item = WorkItem {
            id: "Q20".into(),
            title: "是否切到流式动作".into(),
            repo: dir.to_string_lossy().into_owned(),
            state: "attention".into(),
            brief: "docs/owner/directions/streaming.md".into(),
            owner: Some(OwnerInfo {
                kind: "card".into(),
                ask: "身体第二步是否改成流式驱动？".into(),
                recommendation: "建议先跑 2 天验证再决定".into(),
                if_undecided: "沿用固定片段".into(),
                cost: "2 天开发".into(),
            }),
            created_at: "2026-11-23T08:00:00Z".into(),
            updated_at: "2026-11-23T08:00:00Z".into(),
            state_since: "2026-11-23T08:00:00Z".into(),
            ..Default::default()
        };
        let waiting_item = WorkItem {
            id: "W001".into(),
            title: "实施流式动作".into(),
            repo: dir.to_string_lossy().into_owned(),
            state: "blocked".into(),
            wait_on: vec!["owner:Q20".into()],
            owner_ref: "Q20".into(),
            created_at: "2026-11-23T08:01:00Z".into(),
            updated_at: "2026-11-23T08:01:00Z".into(),
            state_since: "2026-11-23T08:01:00Z".into(),
            ..Default::default()
        };
        save_raw(&state, &[owner_item, waiting_item]).unwrap();

        // Roundtrip JSON + waitOnStatus in load_readonly_json
        let snap = load_readonly_json(&state);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0]["owner"]["kind"], "card");
        assert_eq!(snap[0]["owner"]["ifUndecided"], "沿用固定片段");
        assert_eq!(snap[1]["state"], "blocked");
        assert_eq!(snap[1]["waitOnStatus"][0]["satisfied"], false);
        assert!(snap[1]["waitOnStatus"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("等所有者 Q20"));

        // Dropping Q20 must NOT auto-unlock W001
        let mut items = load_raw(&state);
        items[0].state = "done".into();
        items[0].closed_kind = "dropped".into();
        items[0].close_reason = "先不做".into();
        reconcile_all_locked(&state, &mut items);
        assert_eq!(items[1].state, "blocked");

        // Closing Q20 with evidence unlocks W001 to ready automatically
        items[0].closed_kind = "closed".into();
        items[0].close_evidence = "所有者：同意".into();
        let rec = reconcile_all_locked(&state, &mut items);
        assert_eq!(items[1].state, "ready");
        assert_eq!(rec.unlocked_ready.len(), 1);
        assert_eq!(rec.unlocked_ready[0].id, "W001");

        // Next owner ID allocation respects existing Q/D items
        assert_eq!(next_owner_id(&items, "nod"), "Q21");
        assert_eq!(next_owner_id(&items, "direction"), "D001");

        // Render card includes owner details
        let card = render_card(&items[0], &items, &state, epoch() as i64);
        assert!(card.contains("所有者事项：看卡（card）"), "{card}");
        assert!(card.contains("主控建议：建议先跑 2 天验证再决定"), "{card}");
        assert!(card.contains("不定会怎样：沿用固定片段"), "{card}");
        assert!(card.contains("成本：2 天开发"), "{card}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn w044_cleaned_and_zero_change_runs_do_not_force_attention() {
        let dir = temp_dir("w044");
        let state = dir.join("state");
        let repo = dir.join("repo");
        let runs_dir = repo.join(".local/run/delegate");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&runs_dir).unwrap();
        notes::register_repo_paths(&state, &repo, &runs_dir);

        // 1) W038-like item: structured wait_on + a live zero-change delivered write run
        let r0 = runs_dir.join("20261123-100000-zero");
        fs::create_dir_all(&r0).unwrap();
        write_json(
            r0.join("meta.json"),
            &json!({"run": "20261123-100000-zero", "name": "zero", "mode": "write", "workId": "W038"}),
        )
        .unwrap();
        write_json(
            r0.join("summary.json"),
            &json!({
                "state": "delivered",
                "changes": {"files": 0, "added": 0, "deleted": 0},
                "pendingChanges": {"files": 0, "added": 0, "deleted": 0}
            }),
        )
        .unwrap();
        fs::write(r0.join("exit_code"), "0\n").unwrap();

        let mut w038 = WorkItem {
            id: "W038".into(),
            title: "Phase 0 收尾".into(),
            repo: repo.to_string_lossy().into_owned(),
            state: "attention".into(), // simulate stale attention from pre-6.36
            wait_on: vec!["after_time:2099-01-01T00:00:00Z".into()],
            created_at: "2026-11-23T08:00:00Z".into(),
            updated_at: "2026-11-23T08:00:00Z".into(),
            state_since: "2026-11-23T08:00:00Z".into(),
            ..Default::default()
        };
        w038.add_run("20261123-100000-zero", Some(&r0), Some(&repo.to_string_lossy()));

        // 2) Live zero-change delivered write run WITHOUT wait_on -> verify + focus "已收取"
        let r1 = runs_dir.join("20261123-100500-zero2");
        fs::create_dir_all(&r1).unwrap();
        write_json(
            r1.join("meta.json"),
            &json!({"run": "20261123-100500-zero2", "name": "zero2", "mode": "write", "workId": "W039"}),
        )
        .unwrap();
        write_json(
            r1.join("summary.json"),
            &json!({
                "state": "delivered",
                "changes": {"files": 0, "added": 0, "deleted": 0},
                "pendingChanges": {"files": 0, "added": 0, "deleted": 0}
            }),
        )
        .unwrap();
        fs::write(r1.join("exit_code"), "0\n").unwrap();

        let mut w039 = WorkItem {
            id: "W039".into(),
            title: "零改动确认".into(),
            repo: repo.to_string_lossy().into_owned(),
            state: "active".into(),
            created_at: "2026-11-23T08:05:00Z".into(),
            updated_at: "2026-11-23T08:05:00Z".into(),
            state_since: "2026-11-23T08:05:00Z".into(),
            ..Default::default()
        };
        w039.add_run("20261123-100500-zero2", Some(&r1), Some(&repo.to_string_lossy()));

        // 3) Archived/cleaned run directory (under archive/<run>/record) must be ignored completely!
        let arch_rec = dir.join("archive/20261123-090000-old/record");
        fs::create_dir_all(&arch_rec).unwrap();
        write_json(
            arch_rec.join("meta.json"),
            &json!({"run": "20261123-090000-old", "name": "old", "mode": "write", "workId": "W040"}),
        )
        .unwrap();
        write_json(
            arch_rec.join("summary.json"),
            &json!({
                "state": "delivered",
                "changes": {"files": 2, "added": 10, "deleted": 1},
                "pendingChanges": {"files": 2, "added": 10, "deleted": 1}
            }),
        )
        .unwrap();
        fs::write(arch_rec.join("exit_code"), "0\n").unwrap();

        let mut w040 = WorkItem {
            id: "W040".into(),
            title: "已归档项".into(),
            repo: repo.to_string_lossy().into_owned(),
            state: "attention".into(),
            wait_on: vec!["after_time:2099-01-01T00:00:00Z".into()],
            cleaned: true,
            created_at: "2026-11-23T07:00:00Z".into(),
            updated_at: "2026-11-23T07:00:00Z".into(),
            state_since: "2026-11-23T07:00:00Z".into(),
            ..Default::default()
        };
        w040.add_run("20261123-090000-old", Some(&arch_rec), Some(&repo.to_string_lossy()));

        let mut items = vec![w038, w039, w040];
        let now_epoch = epoch() as i64;
        reconcile_all_locked(&state, &mut items);

        assert_eq!(items[0].state, "blocked", "W038 must honor wait_on, not attention");
        assert!(
            focus_for_item(&items[0], &items, &state, now_epoch).contains("2099-01-01")
        );

        assert_eq!(items[1].state, "verify", "live zero-change delivery should move to verify");
        assert_eq!(focus_for_item(&items[1], &items, &state, now_epoch), "已收取");

        assert_eq!(items[2].state, "blocked", "cleaned/archived run must not affect work state");

        // Also check ephemeral zero-change run shows verify + 已收取
        let r_eph = runs_dir.join("20261123-101000-ephzero");
        fs::create_dir_all(&r_eph).unwrap();
        write_json(
            r_eph.join("meta.json"),
            &json!({"run": "20261123-101000-ephzero", "name": "ephzero", "mode": "write"}),
        )
        .unwrap();
        write_json(
            r_eph.join("summary.json"),
            &json!({
                "state": "delivered",
                "changes": {"files": 0, "added": 0, "deleted": 0},
                "pendingChanges": {"files": 0, "added": 0, "deleted": 0}
            }),
        )
        .unwrap();
        fs::write(r_eph.join("exit_code"), "0\n").unwrap();

        let eph_rows = ephemeral_run_rows(&items, &state, now_epoch);
        let zrow = eph_rows
            .iter()
            .find(|r| r.run_id == "20261123-101000-ephzero")
            .expect("ephemeral zero-change run row");
        assert_eq!(zrow.state, "verify");
        assert_eq!(zrow.focus, "已收取");
        assert!(!zrow.next.starts_with("apply"), "unexpected next: {}", zrow.next);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_thresholds_4h_verify_8h_ready_and_6h_cooldown() {
        let dir = temp_dir("stale-thresholds");
        let state = dir.join("state");
        fs::create_dir_all(&state).unwrap();
        let now_ep = epoch() as i64;
        let ago = |h: i64| format_epoch(now_ep - h * 3600 - 60, false, "%Y-%m-%dT%H:%M:%SZ");

        let mut items = vec![
            WorkItem {
                id: "W001".into(),
                title: "fresh verify 3h".into(),
                state: "verify".into(),
                state_since: format_epoch(now_ep - 3 * 3600, false, "%Y-%m-%dT%H:%M:%SZ"),
                ..Default::default()
            },
            WorkItem {
                id: "W002".into(),
                title: "stale verify 4h".into(),
                state: "verify".into(),
                state_since: ago(4),
                ..Default::default()
            },
            WorkItem {
                id: "W003".into(),
                title: "fresh ready 7h".into(),
                state: "ready".into(),
                brief: "docs/b.md".into(),
                state_since: format_epoch(now_ep - 7 * 3600, false, "%Y-%m-%dT%H:%M:%SZ"),
                ..Default::default()
            },
            WorkItem {
                id: "W004".into(),
                title: "stale ready 8h".into(),
                state: "ready".into(),
                brief: "docs/b.md".into(),
                state_since: ago(8),
                ..Default::default()
            },
            WorkItem {
                id: "W005".into(),
                title: "stale ready cooled 5h".into(),
                state: "ready".into(),
                brief: "docs/b.md".into(),
                state_since: ago(12),
                last_reminded_at: format_epoch(now_ep - 5 * 3600, false, "%Y-%m-%dT%H:%M:%SZ"),
                ..Default::default()
            },
            WorkItem {
                id: "W006".into(),
                title: "stale ready cooldown expired 6h".into(),
                state: "ready".into(),
                brief: "docs/b.md".into(),
                state_since: ago(12),
                last_reminded_at: ago(6),
                ..Default::default()
            },
        ];

        let rec = reconcile_all_locked(&state, &mut items);
        let stale_ids: Vec<&str> = rec.stale_items.iter().map(|(w, _)| w.id.as_str()).collect();
        assert_eq!(stale_ids, vec!["W002", "W004", "W006"]);

        // Immediate second reconcile must respect 6h cooldown and emit nothing.
        let rec2 = reconcile_all_locked(&state, &mut items);
        assert!(rec2.stale_items.is_empty(), "{:?}", rec2.stale_items);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_on_auto_closes_with_evidence_supports_commit_exists_and_rejects_cmd() {
        assert!(normalize_verify_condition("cmd:true").is_err());
        assert!(normalize_verify_condition("manual:wait").is_err());
        assert_eq!(
            normalize_verify_condition("commit_exists:ABCDEF1").unwrap(),
            "commit_exists:abcdef1"
        );
        assert_eq!(
            normalize_verify_condition("work_closed:w1").unwrap(),
            "work_closed:W001"
        );
        assert_eq!(
            normalize_verify_condition("owner:q2").unwrap(),
            "owner:Q02"
        );

        let dir = temp_dir("verify-on");
        let state = dir.join("state");
        let repo = dir.join("repo");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&repo).unwrap();

        // Create a real git repo with one commit to test commit_exists:<sha>
        let run_git = |args: &[&str]| {
            let st = Command::new("git")
                .current_dir(&repo)
                .args(args)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);
        fs::write(repo.join("a.txt"), "hello\n").unwrap();
        run_git(&["add", "a.txt"]);
        run_git(&["commit", "-q", "-m", "init"]);
        let sha_out = Command::new("git")
            .current_dir(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let full_sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();
        let short_sha = &full_sha[..7];

        assert!(commit_exists_in_repo(&repo, &full_sha));
        assert!(commit_exists_in_repo(&repo, short_sha));
        assert!(!commit_exists_in_repo(&repo, "deadbeef"));

        let now_iso = iso();
        let mut items = vec![
            WorkItem {
                id: "Q01".into(),
                title: "点头发布".into(),
                repo: repo.to_string_lossy().into_owned(),
                state: "attention".into(),
                owner: Some(OwnerInfo {
                    kind: "nod".into(),
                    ask: "点头发布".into(),
                    ..Default::default()
                }),
                created_at: now_iso.clone(),
                updated_at: now_iso.clone(),
                state_since: now_iso.clone(),
                ..Default::default()
            },
            WorkItem {
                id: "W001".into(),
                title: "带 verify_on 闭环".into(),
                repo: repo.to_string_lossy().into_owned(),
                state: "verify".into(),
                verify_on: vec![
                    format!("commit_exists:{short_sha}"),
                    "owner:Q01".into(),
                ],
                created_at: now_iso.clone(),
                updated_at: now_iso.clone(),
                state_since: now_iso.clone(),
                ..Default::default()
            },
            WorkItem {
                id: "W002".into(),
                title: "无 verify_on 的 verify 项".into(),
                repo: repo.to_string_lossy().into_owned(),
                state: "verify".into(),
                created_at: now_iso.clone(),
                updated_at: now_iso.clone(),
                state_since: now_iso.clone(),
                ..Default::default()
            },
        ];

        // Q01 still open -> W001 stays verify; W002 has empty verify_on -> stays verify.
        reconcile_all_locked(&state, &mut items);
        assert_eq!(items[1].state, "verify");
        assert_eq!(items[2].state, "verify");

        // Close Q01 -> W001 auto-closes with evidence containing both condition results; W002 stays verify!
        items[0].state = "done".into();
        items[0].closed_kind = "closed".into();
        items[0].close_evidence = "所有者已点头".into();
        reconcile_all_locked(&state, &mut items);
        assert_eq!(items[1].state, "done");
        assert_eq!(items[1].closed_kind, "auto");
        assert!(
            items[1].close_evidence.contains(&format!("commit_exists:{short_sha}"))
                && items[1].close_evidence.contains("owner:Q01"),
            "evidence: {}",
            items[1].close_evidence
        );
        assert_eq!(items[2].state, "verify");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn owner_wait_keywords_and_repeated_defer_display() {
        assert!(matches_owner_wait_keyword("需要真机验证", ""));
        assert!(matches_owner_wait_keyword("普通改动", "对外发布前确认"));
        assert!(matches_owner_wait_keyword("删除旧表", ""));
        assert!(!matches_owner_wait_keyword("修单测", "cargo test 通过"));

        let dir = temp_dir("repeat-defer");
        let state = dir.join("state");
        fs::create_dir_all(&state).unwrap();
        let now_ep = epoch() as i64;

        let mut w = WorkItem {
            id: "W009".into(),
            title: "老被推迟的项".into(),
            state: "deferred".into(),
            defer_reason: "等窗口".into(),
            defer_until: "2026-11-26T00:00:00Z".into(),
            defer_streak: 2,
            ..Default::default()
        };
        assert!(!focus_for_item(&w, std::slice::from_ref(&w), &state, now_ep).contains("反复暂缓"));
        w.defer_streak = 3;
        assert!(focus_for_item(&w, std::slice::from_ref(&w), &state, now_ep).contains("反复暂缓"));
        let card = render_card(&w, std::slice::from_ref(&w), &state, now_ep);
        assert!(card.contains("反复暂缓（连续 3 次）"), "{card}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconcile_under_state_lock_never_runs_git_even_without_final_entries() {
        let dir = temp_dir("reconcile-no-git-under-lock");
        let state = dir.join("state");
        let repo = dir.join("repo");
        let runs_dir = repo.join(".local/run/delegate");
        let run_dir = runs_dir.join("20261124-120000-r1");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&run_dir).unwrap();

        let run_git = |args: &[&str]| {
            let st = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);
        fs::write(repo.join("a.txt"), "base\n").unwrap();
        run_git(&["add", "a.txt"]);
        run_git(&["commit", "-q", "-m", "base"]);
        let base_tree = git_text(&repo, &["rev-parse", "HEAD^{tree}"]).unwrap();

        fs::write(repo.join("a.txt"), "changed\n").unwrap();
        run_git(&["add", "a.txt"]);
        run_git(&["commit", "-q", "-m", "after"]);
        let after_tree = git_text(&repo, &["rev-parse", "HEAD^{tree}"]).unwrap();

        // Register repo in state/repos.json so `find_live_run_dir` resolves `20261124-120000-r1`.
        let runs_key = fs::canonicalize(&runs_dir).unwrap().to_string_lossy().into_owned();
        let repo_key = fs::canonicalize(&repo).unwrap().to_string_lossy().into_owned();
        write_json(
            state.join("repos.json"),
            &json!({"repos": {runs_key.clone(): {"repo": repo_key.clone(), "runs": runs_key}}}),
        )
        .unwrap();

        // Legacy run record WITHOUT `finalEntries` in `changes.json`: calling `recorded_entries`
        // inside `.state.lock` previously invoked `git` and panicked in debug builds / printed
        // `forbidden slow operation (git/subprocess)` in release builds.
        write_json(
            run_dir.join("meta.json"),
            &json!({
                "run": "20261124-120000-r1",
                "mode": "write",
                "worktree": {"source": repo_key, "path": repo_key},
                "base": {"tree": base_tree}
            }),
        )
        .unwrap();
        write_json(
            run_dir.join("changes.json"),
            &json!({
                "top": repo_key,
                "base": base_tree,
                "after": after_tree,
                "changes": [{"path": "a.txt", "status": "M"}]
            }),
        )
        .unwrap();
        fs::write(run_dir.join("exit_code"), "0\n").unwrap();

        let now_iso = iso();
        let mut items = vec![WorkItem {
            id: "W001".into(),
            title: "downstream".into(),
            repo: repo_key,
            state: "blocked".into(),
            wait_on: vec!["run_applied:20261124-120000-r1".into()],
            created_at: now_iso.clone(),
            updated_at: now_iso.clone(),
            state_since: now_iso,
            ..Default::default()
        }];

        // 1. Prefetch outside state lock, then reconcile inside state lock: must NOT panic and MUST unlock W001.
        let prefetch = prefetch_reconcile_conditions(&state, &items, &[]);
        {
            let _guard = crate::locks::acquire_state(&state).unwrap();
            let rec = reconcile_all_locked_with_prefetch(&state, &mut items, None, Some(&prefetch));
            assert!(rec.changed);
            assert_eq!(items[0].state, "ready");
        }

        // 2. Defense-in-depth: even without prefetch, calling reconcile under `.state.lock` must NOT panic.
        items[0].state = "blocked".into();
        {
            let _guard = crate::locks::acquire_state(&state).unwrap();
            let _ = reconcile_all_locked(&state, &mut items);
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_legacy_manual_wait_strips_appends_followup_and_emits_event() {
        let dir = temp_dir("migrate-manual-wait");
        let state = dir.join("state");
        fs::create_dir_all(&state).unwrap();

        let legacy_items = vec![
            WorkItem {
                id: "W001".into(),
                title: "仅手动前置项".into(),
                repo: "/tmp/demo".into(),
                state: "blocked".into(),
                wait_on: vec!["manual:等发版窗口\n第二行".into()],
                created_at: "2026-11-20T08:00:00Z".into(),
                updated_at: "2026-11-20T08:00:00Z".into(),
                state_since: "2026-11-20T08:00:00Z".into(),
                ..Default::default()
            },
            WorkItem {
                id: "W002".into(),
                title: "混合前置项".into(),
                repo: "/tmp/demo".into(),
                state: "blocked".into(),
                wait_on: vec![
                    "manual:等接口上线".into(),
                    "after_time:2099-01-01T00:00:00Z".into(),
                ],
                created_at: "2026-11-20T08:00:00Z".into(),
                updated_at: "2026-11-20T08:00:00Z".into(),
                state_since: "2026-11-20T08:00:00Z".into(),
                ..Default::default()
            },
        ];
        save_raw(&state, &legacy_items).unwrap();

        // First read rewrites work.jsonl once and emits one `work_migrated` event.
        let migrated = load_raw(&state);
        assert_eq!(migrated.len(), 2);
        assert!(migrated[0].wait_on.is_empty());
        assert_eq!(migrated[0].state, "ready");
        assert_eq!(migrated[0].followups.len(), 1);
        assert_eq!(migrated[0].followups[0].text, "原手动前置：等发版窗口 第二行");
        assert!(!migrated[0].followups[0].text.contains('\n'));

        assert_eq!(migrated[1].wait_on, vec!["after_time:2099-01-01T00:00:00Z"]);
        assert_eq!(migrated[1].state, "blocked");
        assert_eq!(migrated[1].followups.len(), 1);
        assert_eq!(migrated[1].followups[0].text, "原手动前置：等接口上线");

        // On-disk work.jsonl has no manual: left.
        let disk_text = fs::read_to_string(work_path(&state)).unwrap();
        assert!(!disk_text.contains("manual:"), "{disk_text}");
        assert!(disk_text.contains("原手动前置：等发版窗口 第二行"), "{disk_text}");

        let ev_lines: Vec<Value> = fs::read_to_string(state.join("events.jsonl"))
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(ev_lines.len(), 1, "{ev_lines:?}");
        assert_eq!(ev_lines[0]["kind"], "work_migrated");
        assert_eq!(ev_lines[0]["payload"]["reason"], "manual_wait_removed");
        assert_eq!(ev_lines[0]["payload"]["workIds"], json!(["W001", "W002"]));

        // Second read is idempotent: no duplicate followup and no second event.
        let again = load_raw(&state);
        assert_eq!(again[0].followups.len(), 1);
        let ev_lines2: Vec<String> = fs::read_to_string(state.join("events.jsonl"))
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect();
        assert_eq!(ev_lines2.len(), 1);

        // After migration, code no longer recognizes `manual:` in evaluate_condition.
        let ev = evaluate_condition("manual:legacy", &again[0], &again, &state, epoch() as i64);
        assert!(ev.detail.starts_with("未知前置："), "{}", ev.detail);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_owner_fields_rejects_missing_and_title_copy() {
        let err = validate_owner_fields(None, Some("建议 A"), None, Some("1 天"), Some("发布审批"))
            .err()
            .expect("must reject missing fields");
        assert!(err.contains("--ask"), "{err}");
        assert!(err.contains("--if-undecided"), "{err}");
        assert!(!err.contains("--recommendation"), "{err}");
        assert!(!err.contains("--cost"), "{err}");

        let same_err = validate_owner_fields(
            Some("发布审批"),
            Some("建议 A"),
            Some("延期"),
            Some("1 天"),
            Some("发布审批"),
        )
        .err()
        .expect("must reject ask == title");
        assert!(same_err.contains("不能直接等于标题"), "{same_err}");

        let ok = validate_owner_fields(
            Some("是否批准对外发布？"),
            Some("建议批准"),
            Some("顺延至下周"),
            Some("零成本"),
            Some("发布审批"),
        )
        .expect("valid fields");
        assert_eq!(ok.ask, "是否批准对外发布？");
        assert_eq!(ok.recommendation, "建议批准");
        assert_eq!(ok.if_undecided, "顺延至下周");
        assert_eq!(ok.cost, "零成本");
    }

    #[test]
    fn render_card_owner_snapshot_shows_waiting_work_items() {
        let dir = temp_dir("owner-card-snapshot");
        let state = dir.join("state");
        fs::create_dir_all(&state).unwrap();

        let q10 = WorkItem {
            id: "Q10".into(),
            title: "路由方案选型".into(),
            repo: "/tmp/demo-app".into(),
            state: "attention".into(),
            owner: Some(OwnerInfo {
                kind: "nod".into(),
                ask: "是否采用方案 A 切换路由？".into(),
                recommendation: "建议选方案 A，回滚成本低".into(),
                if_undecided: "继续使用旧路由".into(),
                cost: "半天开发".into(),
            }),
            created_at: "2026-11-24T10:00:00Z".into(),
            updated_at: "2026-11-24T10:00:00Z".into(),
            state_since: "2026-11-24T10:00:00Z".into(),
            ..Default::default()
        };
        let w1 = WorkItem {
            id: "W001".into(),
            title: "接入新路由".into(),
            repo: "/tmp/demo-app".into(),
            state: "blocked".into(),
            done_when: "全量单测与灰度通过".into(),
            brief: "docs/plan/routing-a.md".into(),
            wait_on: vec!["owner:Q10".into()],
            ..Default::default()
        };
        let w2 = WorkItem {
            id: "W002".into(),
            title: "清理旧路由配置".into(),
            repo: "/tmp/demo-app".into(),
            state: "blocked".into(),
            done_when: "删除旧配置并跑通冒烟".into(),
            wait_on: vec!["owner:q10".into()],
            ..Default::default()
        };
        let w3_done = WorkItem {
            id: "W003".into(),
            title: "已关闭旧项".into(),
            repo: "/tmp/demo-app".into(),
            state: "done".into(),
            wait_on: vec!["owner:Q10".into()],
            ..Default::default()
        };
        let all = vec![q10.clone(), w1, w2, w3_done];
        let now_ep = parse_iso_or_date_epoch("2026-11-24T12:00:00Z").unwrap();
        let card = render_card(&q10, &all, &state, now_ep);
        let expected = "\
[Q10] 路由方案选型 · [demo-app] · 状态：attention（已停留 2h）
所有者事项：点头（nod）
要你回答：是否采用方案 A 切换路由？
主控建议：建议选方案 A，回滚成本低
不定会怎样：继续使用旧路由
成本：半天开发
在等它的工作：
  - [W001] 接入新路由 · 完成条件：全量单测与灰度通过 · 说明：docs/plan/routing-a.md
  - [W002] 清理旧路由配置 · 完成条件：删除旧配置并跑通冒烟";
        assert_eq!(card, expected);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn has_open_closure_ignores_text_done_when_and_honors_structural_blockers() {
        let mut w = WorkItem {
            id: "W054".into(),
            title: "修复合入关项".into(),
            done_when: "单测全绿并通过人工检查".into(),
            ..Default::default()
        };
        assert!(!w.has_open_closure());

        w.needs_verify = true;
        assert!(w.has_open_closure());
        w.needs_verify = false;

        w.verify_on.push("owner:Q01".into());
        assert!(w.has_open_closure());
        w.verify_on.clear();

        w.artifacts.push(ArtifactItem {
            path: ".local/out.png".into(),
            source_run: String::new(),
            added_at: String::new(),
            note: String::new(),
        });
        assert!(w.has_open_closure());
        w.artifacts.clear();

        w.followups.push(FollowupItem {
            id: 1,
            kind: "note".into(),
            text: "补单测".into(),
            anchor: String::new(),
            source_run: String::new(),
            done: false,
            created_at: String::new(),
            done_at: None,
        });
        assert!(w.has_open_closure());
        w.followups[0].done = true;
        assert!(!w.has_open_closure());
    }

    #[test]
    fn work_applied_eval_and_cycle_detection() {
        let dir = temp_dir("work-applied-eval");
        let state = dir.join("state");
        fs::create_dir_all(&state).unwrap();
        let now_ep = epoch() as i64;

        let mut items = vec![
            WorkItem {
                id: "W071".into(),
                title: "核心代码".into(),
                state: "verify".into(),
                needs_verify: true,
                applied: false,
                ..Default::default()
            },
            WorkItem {
                id: "W064".into(),
                title: "真机实测".into(),
                state: "blocked".into(),
                wait_on: vec!["work_applied:W071".into()],
                ..Default::default()
            },
            WorkItem {
                id: "W072".into(),
                title: "已放弃上游".into(),
                state: "done".into(),
                closed_kind: "dropped".into(),
                applied: false,
                ..Default::default()
            },
            WorkItem {
                id: "W073".into(),
                title: "等已放弃上游".into(),
                state: "blocked".into(),
                wait_on: vec!["work_applied:W072".into()],
                ..Default::default()
            },
        ];

        assert!(!evaluate_condition("work_applied:W071", &items[1], &items, &state, now_ep).satisfied);
        // Applied while still in `verify` immediately satisfies `work_applied:W071`
        items[0].applied = true;
        assert!(evaluate_condition("work_applied:W071", &items[1], &items, &state, now_ep).satisfied);
        // Dropped upstream never auto-unlocks `work_applied:`
        assert!(!evaluate_condition("work_applied:W072", &items[3], &items, &state, now_ep).satisfied);

        // Cycle detection covers work_applied: and mixed work_applied:/work_closed: cycles
        assert!(has_work_cycle(&items, "W071", &["work_applied:W064".into()]));
        assert!(has_work_cycle(&items, "W071", &["work_closed:W064".into()]));
        assert!(!has_work_cycle(&items, "W099", &["work_applied:W064".into()]));

        // `--after-applied <Wxx>` parses to `work_applied:<Wxxx>` while run refs stay `run_applied:`
        let parsed = parse_wait_conditions(
            &[
                "--after-applied".into(),
                "w71,20261124-140427-delegate-fixes".into(),
                "--needs-verify".into(),
            ],
            false,
        )
        .unwrap();
        assert_eq!(
            parsed.wf.wait_on,
            vec![
                "work_applied:W071".to_string(),
                "run_applied:20261124-140427-delegate-fixes".to_string()
            ]
        );
        assert_eq!(parsed.wf.needs_verify, Some(true));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn blocked_downstream_walks_parent_chain_and_transitive_work_graph() {
        let items = vec![
            WorkItem {
                id: "W057".into(),
                title: "等根 run".into(),
                state: "blocked".into(),
                wait_on: vec!["run_applied:20261124-140427-delegate-fixes".into()],
                ..Default::default()
            },
            WorkItem {
                id: "W058".into(),
                title: "等 W057 合入".into(),
                state: "blocked".into(),
                wait_on: vec!["work_applied:W057".into()],
                ..Default::default()
            },
            WorkItem {
                id: "W069".into(),
                title: "等 W058 关闭".into(),
                state: "blocked".into(),
                wait_on: vec!["work_closed:W058".into()],
                ..Default::default()
            },
            WorkItem {
                id: "W070".into(),
                title: "也等 W058".into(),
                state: "blocked".into(),
                wait_on: vec!["work_closed:W058".into()],
                ..Default::default()
            },
            WorkItem {
                id: "W071".into(),
                title: "已完成不计入".into(),
                state: "done".into(),
                wait_on: vec!["work_closed:W058".into()],
                ..Default::default()
            },
        ];

        // Leaf `bb52` carries its parent chain back to root `140427`
        let chain_ids = vec![
            "20261124-145101-bb52".to_string(),
            "20261124-140427-delegate-fixes".to_string(),
        ];
        let blocked = blocked_downstream_in(&items, &chain_ids, &[]);
        assert_eq!(blocked, vec!["W057", "W058", "W069", "W070"]);
    }

    #[test]
    fn import_dag_rejects_dup_cycle_missing_brief_and_accepts_valid() {
        let dir = temp_dir("import-dag");
        let repo = dir.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let brief_a = repo.join("brief-a.md");
        let brief_b = repo.join("brief-b.md");
        fs::write(&brief_a, "implement A").unwrap();
        fs::write(&brief_b, "implement B").unwrap();
        let repo_s = repo.to_string_lossy().into_owned();
        let empty = HashSet::new();

        // 1. Duplicate key rejected
        let dup_doc = json!({
            "items": [
                {"key": "a", "title": "A1", "class": "feat", "brief": "brief-a.md"},
                {"key": "a", "title": "A2", "class": "feat", "brief": "brief-b.md"}
            ]
        });
        let err = parse_dag_import_json(&dup_doc, &repo_s, Some(&repo), &empty).unwrap_err();
        assert!(err.contains("duplicate key"), "{err}");

        // 2. Cycle rejected (2-node cycle)
        let cyc_doc = json!({
            "items": [
                {"key": "a", "title": "A", "class": "feat", "brief": "brief-a.md", "after": ["b"]},
                {"key": "b", "title": "B", "class": "unblock", "brief": "brief-b.md", "after": ["a"]}
            ]
        });
        let err = parse_dag_import_json(&cyc_doc, &repo_s, Some(&repo), &empty).unwrap_err();
        assert!(err.contains("cycle"), "{err}");

        // 3. Missing brief rejected
        let miss_doc = json!({
            "items": [
                {"key": "a", "title": "A", "class": "feat", "brief": "brief-a.md"},
                {"key": "b", "title": "B", "class": "docs", "brief": "no-such-brief.md"}
            ]
        });
        let err = parse_dag_import_json(&miss_doc, &repo_s, Some(&repo), &empty).unwrap_err();
        assert!(err.contains("brief file does not exist"), "{err}");

        // 4. Valid DAG accepted
        let ok_doc = json!({
            "items": [
                {
                    "key": "a",
                    "title": "Base task",
                    "class": "unblock",
                    "brief": "brief-a.md",
                    "doneWhen": "tests pass",
                    "files": ["./crates/foo/src/lib.rs"],
                    "tier": "write",
                    "model": "sol",
                    "thinking": "medium"
                },
                {
                    "key": "b",
                    "title": "Dependent task",
                    "class": "feat",
                    "brief": "brief-b.md",
                    "after": ["a"],
                    "files": ["README.md"]
                }
            ]
        });
        let parsed = parse_dag_import_json(&ok_doc, &repo_s, Some(&repo), &empty).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].files, vec!["crates/foo/src/lib.rs".to_string()]);
        assert_eq!(
            parsed[0].start_args(),
            vec!["--worktree", "--tier", "write", "--model", "sol", "--thinking", "medium"]
        );
        assert_eq!(parsed[1].after, vec!["a".to_string()]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[allow(clippy::too_many_arguments)]
    fn make_fill_item(
        id: &str,
        repo: &str,
        state: &str,
        class: &str,
        brief: &str,
        state_since: &str,
        files: &[&str],
        wait_on: &[&str],
    ) -> WorkItem {
        WorkItem {
            id: id.into(),
            title: format!("task {id}"),
            repo: repo.into(),
            state: state.into(),
            class: class.into(),
            files: files.iter().map(|s| s.to_string()).collect(),
            brief: brief.into(),
            wait_on: wait_on.iter().map(|s| s.to_string()).collect(),
            auto_dispatch: Some(AutoDispatch {
                prompt_file: brief.into(),
                start_args: vec!["--worktree".into()],
                cwd: repo.into(),
                repo: repo.into(),
                runs_dir: String::new(),
                caller: String::new(),
                dispatched_run: String::new(),
            }),
            created_at: state_since.into(),
            updated_at: state_since.into(),
            state_since: state_since.into(),
            ..Default::default()
        }
    }

    #[test]
    fn fill_selection_class_order_out_degree_quota_hotspots_and_target_cap() {
        let dir = temp_dir("fill-select");
        let state = dir.join("state");
        let repo = dir.join("repo");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let brief = repo.join("brief.md");
        fs::write(&brief, "do work").unwrap();
        let brief_s = brief.to_string_lossy().into_owned();
        let repo_s = fs::canonicalize(&repo).unwrap().to_string_lossy().into_owned();

        // Write .delegate.json with target=6, maxInfraDocsShare=0.3, hotspots=["README.md"]
        fs::write(
            repo.join(".delegate.json"),
            r#"{"fill":{"target":6,"maxInfraDocsShare":0.3},"hotspots":["README.md"]}"#,
        )
        .unwrap();

        // 1) Class priority: feat > unblock > infra > docs; classless ignored
        let items = vec![
            make_fill_item("W001", &repo_s, "ready", "", &brief_s, "2026-11-24T01:00:00Z", &[], &[]),
            make_fill_item("W002", &repo_s, "ready", "docs", &brief_s, "2026-11-24T01:00:00Z", &[], &[]),
            make_fill_item("W003", &repo_s, "ready", "infra", &brief_s, "2026-11-24T02:00:00Z", &[], &[]),
            make_fill_item("W004", &repo_s, "ready", "unblock", &brief_s, "2026-11-24T03:00:00Z", &[], &[]),
            make_fill_item("W005", &repo_s, "ready", "feat", &brief_s, "2026-11-24T04:00:00Z", &[], &[]),
        ];
        let picked = select_fill_candidates(&state, &items, None, &HashSet::new());
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].item.id, "W005");
        assert_eq!(picked[0].active_writes, 0);

        // Missing class row renders 缺类别 at line end without truncation
        let row = TableRow {
            id: "W001".into(),
            repo: "repo".into(),
            title: "一个非常长非常长非常长非常长非常长的标题".into(),
            state: "ready".into(),
            focus: "可派发 1h".into(),
            next: "start --work W001 --prompt-file very/long/path/to/brief.md".into(),
            is_ephemeral: false,
            run_id: String::new(),
            missing_class: items[0].is_missing_class(),
        };
        let rendered = format_table_row(&row);
        assert!(rendered.ends_with(" · 缺类别"), "{rendered}");
        assert!(rendered.chars().count() <= 100, "{rendered}");

        // 2) Same class: higher out-degree wins over earlier ready time; tie falls back to ready time
        let items_deg = vec![
            make_fill_item("W010", &repo_s, "ready", "feat", &brief_s, "2026-11-24T01:00:00Z", &[], &[]),
            make_fill_item("W011", &repo_s, "ready", "feat", &brief_s, "2026-11-24T05:00:00Z", &[], &[]),
            make_fill_item("W012", &repo_s, "blocked", "feat", &brief_s, "2026-11-24T06:00:00Z", &[], &["work_closed:W011"]),
            make_fill_item("W013", &repo_s, "blocked", "feat", &brief_s, "2026-11-24T06:00:00Z", &[], &["work_closed:W011"]),
        ];
        let picked_deg = select_fill_candidates(&state, &items_deg, None, &HashSet::new());
        assert_eq!(picked_deg[0].item.id, "W011");

        // 3) Infra/docs quota: target=6, share=0.3 -> max 2. With 2 active infra/docs, 3rd infra/docs skipped!
        let items_quota = vec![
            make_fill_item("W020", &repo_s, "active", "infra", &brief_s, "2026-11-24T01:00:00Z", &[], &[]),
            make_fill_item("W021", &repo_s, "active", "docs", &brief_s, "2026-11-24T01:00:00Z", &[], &[]),
            make_fill_item("W022", &repo_s, "ready", "infra", &brief_s, "2026-11-24T01:00:00Z", &[], &[]),
            make_fill_item("W023", &repo_s, "ready", "docs", &brief_s, "2026-11-24T01:00:00Z", &[], &[]),
        ];
        let picked_quota = select_fill_candidates(&state, &items_quota, None, &HashSet::new());
        assert!(picked_quota.is_empty(), "expected infra/docs quota cap at 2");

        // With 1 active infra, 2nd docs IS allowed
        let items_quota_ok = vec![
            make_fill_item("W020", &repo_s, "active", "infra", &brief_s, "2026-11-24T01:00:00Z", &[], &[]),
            make_fill_item("W023", &repo_s, "ready", "docs", &brief_s, "2026-11-24T01:00:00Z", &[], &[]),
        ];
        let picked_quota_ok = select_fill_candidates(&state, &items_quota_ok, None, &HashSet::new());
        assert_eq!(picked_quota_ok.len(), 1);
        assert_eq!(picked_quota_ok[0].item.id, "W023");

        // 4) Hotspot skipping: active touches README.md -> skip W031 (touches README.md), pick W032 (overlaps non-hotspot src/a.rs!)
        let items_hot = vec![
            make_fill_item("W030", &repo_s, "active", "feat", &brief_s, "2026-11-24T01:00:00Z", &["README.md", "src/a.rs"], &[]),
            make_fill_item("W031", &repo_s, "ready", "feat", &brief_s, "2026-11-24T01:00:00Z", &["README.md"], &[]),
            make_fill_item("W032", &repo_s, "ready", "unblock", &brief_s, "2026-11-24T02:00:00Z", &["src/a.rs"], &[]),
        ];
        let picked_hot = select_fill_candidates(&state, &items_hot, None, &HashSet::new());
        assert_eq!(picked_hot.len(), 1);
        assert_eq!(picked_hot[0].item.id, "W032");

        // 5) Target full (6 active writes) -> does not dispatch
        let mut items_full: Vec<WorkItem> = (1..=6)
            .map(|i| {
                make_fill_item(
                    &format!("W04{i}"),
                    &repo_s,
                    "active",
                    "feat",
                    &brief_s,
                    "2026-11-24T01:00:00Z",
                    &[],
                    &[],
                )
            })
            .collect();
        items_full.push(make_fill_item(
            "W049",
            &repo_s,
            "ready",
            "feat",
            &brief_s,
            "2026-11-24T01:00:00Z",
            &[],
            &[],
        ));
        let picked_full = select_fill_candidates(&state, &items_full, None, &HashSet::new());
        assert!(picked_full.is_empty(), "target=6 full must not dispatch");

        // 6) Read-only active item does NOT count toward target=6 (5 write + 2 read-only = 5 < 6 -> dispatches!)
        let mut items_ro: Vec<WorkItem> = (1..=5)
            .map(|i| {
                make_fill_item(
                    &format!("W05{i}"),
                    &repo_s,
                    "active",
                    "feat",
                    &brief_s,
                    "2026-11-24T01:00:00Z",
                    &[],
                    &[],
                )
            })
            .collect();
        let mut ro_active = make_fill_item(
            "W056",
            &repo_s,
            "active",
            "feat",
            &brief_s,
            "2026-11-24T01:00:00Z",
            &[],
            &[],
        );
        ro_active.auto_dispatch.as_mut().unwrap().start_args = vec!["--read-only".into()];
        items_ro.push(ro_active);
        items_ro.push(make_fill_item(
            "W059",
            &repo_s,
            "ready",
            "feat",
            &brief_s,
            "2026-11-24T01:00:00Z",
            &[],
            &[],
        ));
        let picked_ro = select_fill_candidates(&state, &items_ro, None, &HashSet::new());
        assert_eq!(picked_ro.len(), 1);
        assert_eq!(picked_ro[0].item.id, "W059");
        assert_eq!(picked_ro[0].active_writes, 5);

        let _ = fs::remove_dir_all(&dir);
    }
}
