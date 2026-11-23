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
    pub closed_kind: String,
    pub done_when: String,
    pub wait_on: Vec<String>,
    pub auto_dispatch: Option<AutoDispatch>,
    pub runs: Vec<String>,
    pub run_dirs: BTreeMap<String, String>,
    pub artifacts: Vec<ArtifactItem>,
    pub brief: String,
    pub followups: Vec<FollowupItem>,
    pub owner_ref: String,
    pub defer_reason: String,
    pub defer_until: String,
    pub created_at: String,
    pub updated_at: String,
    pub state_since: String,
    pub last_reminded_at: String,
    pub close_reason: String,
    pub close_evidence: String,
    pub origin: String,
    pub note_root: String,
    pub applied: bool,
    pub cleaned: bool,
}

impl WorkItem {
    pub fn pending_followups(&self) -> usize {
        self.followups.iter().filter(|f| !f.done).count()
    }

    pub fn has_open_closure(&self) -> bool {
        !self.done_when.trim().is_empty()
            || !self.artifacts.is_empty()
            || self.pending_followups() > 0
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
            closed_kind: s(v, "closedKind").to_string(),
            done_when: s(v, "doneWhen").to_string(),
            wait_on: strings(&v["waitOn"]),
            auto_dispatch: AutoDispatch::from_value(&v["autoDispatch"]),
            runs: strings(&v["runs"]),
            run_dirs,
            artifacts,
            brief: s(v, "brief").to_string(),
            followups,
            owner_ref: s(v, "ownerRef").to_string(),
            defer_reason: s(v, "deferReason").to_string(),
            defer_until: s(v, "deferUntil").to_string(),
            created_at,
            updated_at: s(v, "updatedAt").to_string(),
            state_since,
            last_reminded_at: s(v, "lastRemindedAt").to_string(),
            close_reason: s(v, "closeReason").to_string(),
            close_evidence: s(v, "closeEvidence").to_string(),
            origin: s(v, "origin").to_string(),
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
        if !self.closed_kind.is_empty() {
            obj["closedKind"] = json!(self.closed_kind);
        }
        if !self.done_when.is_empty() {
            obj["doneWhen"] = json!(self.done_when);
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
        if !self.owner_ref.is_empty() {
            obj["ownerRef"] = json!(self.owner_ref);
        }
        if !self.defer_reason.is_empty() {
            obj["deferReason"] = json!(self.defer_reason);
        }
        if !self.defer_until.is_empty() {
            obj["deferUntil"] = json!(self.defer_until);
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

fn parse_work_num(id: &str) -> Option<u64> {
    let rest = id.strip_prefix('W').or_else(|| id.strip_prefix('w'))?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse::<u64>().ok()
}

pub fn normalize_work_id(raw: &str) -> Option<String> {
    let n = parse_work_num(raw.trim())?;
    Some(format!("W{n:03}"))
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

fn load_raw(state: &Path) -> Vec<WorkItem> {
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

fn find_run_dir(run_ref: &str, preferred_dirs: &BTreeMap<String, String>, state: &Path) -> Option<PathBuf> {
    if let Some(p) = preferred_dirs.get(run_ref) {
        let pb = PathBuf::from(p);
        if pb.join("meta.json").is_file() {
            return Some(pb);
        }
    }
    if let Ok(p) = runs::resolve_head(run_ref) {
        if p.join("meta.json").is_file() {
            return Some(p);
        }
    }
    // Search registered repos' runs dirs (exact match first, then unique suffix/substring)
    let mut partial_matches = Vec::new();
    for (_, runs_dir) in registered_repo_runs(state) {
        let exact = runs_dir.join(run_ref);
        if exact.join("meta.json").is_file() {
            return Some(exact);
        }
        if let Ok(rd) = fs::read_dir(&runs_dir) {
            for ent in rd.flatten() {
                let p = ent.path();
                if !p.join("meta.json").is_file() {
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

pub fn evaluate_condition(
    cond: &str,
    item: &WorkItem,
    all_items: &[WorkItem],
    state: &Path,
    now_epoch: i64,
) -> CondEval {
    if let Some(run_ref) = cond.strip_prefix("run_applied:") {
        let ok = all_items
            .iter()
            .any(|w| w.applied && w.matches_run(run_ref))
            || is_run_applied(run_ref, state, Some(&item.repo));
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
            OwnerStatus::MissingFile => (false, format!("等所有者 {oref}（未找到 docs/owner/README.md）")),
        };
        return CondEval {
            raw: cond.to_string(),
            satisfied: ok,
            detail,
            pointer: readme.to_string_lossy().into_owned(),
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
    if let Some(note) = cond.strip_prefix("manual:") {
        return CondEval {
            raw: cond.to_string(),
            satisfied: false,
            detail: format!("手动前置：{note}"),
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

fn latest_live_run(item: &WorkItem, state: &Path) -> Option<(String, PathBuf)> {
    for rid in item.runs.iter().rev() {
        if let Some(dir) = find_run_dir(rid, &item.run_dirs, state) {
            return Some((rid.clone(), dir));
        }
    }
    None
}

struct ReconcileOutcome {
    changed: bool,
    unlocked_ready: Vec<WorkItem>,
    to_auto_dispatch: Vec<WorkItem>,
    defer_expired: Vec<WorkItem>,
    stale_items: Vec<(WorkItem, String)>,
}

fn reconcile_all_locked(state: &Path, items: &mut [WorkItem]) -> ReconcileOutcome {
    let now_epoch = epoch() as i64;
    let now_iso = iso();
    let verify_stale_secs = std::env::var("DELEGATE_WORK_STALE_VERIFY_HOURS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(24.0)
        * 3600.0;
    let ready_stale_secs = std::env::var("DELEGATE_WORK_STALE_READY_HOURS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(48.0)
        * 3600.0;
    let cooldown_secs = 24.0 * 3600.0;

    let mut out = ReconcileOutcome {
        changed: false,
        unlocked_ready: Vec::new(),
        to_auto_dispatch: Vec::new(),
        defer_expired: Vec::new(),
        stale_items: Vec::new(),
    };

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

        // 2. Inspect latest live run (if any)
        let mut live_state: Option<String> = None;
        if let Some((_, rdir)) = latest_live_run(item, state) {
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
                    || review_verdict == "NEEDS_CONTROLLER"
                    || matches!(
                        st.as_str(),
                        "rejected" | "failed" | "malformed" | "timeout" | "stalled" | "killed" | "crashed"
                    )
                {
                    if !is_applied {
                        live_state = Some("attention".into());
                    }
                } else if (st == "delivered" && !is_applied && s(&meta, "mode") == "write")
                    || (st == "answered"
                        && !rdir.join(".delivered").exists()
                        && s(&meta, "resultReadAt").is_empty())
                {
                    live_state = Some("attention".into());
                } else if is_applied {
                    item.applied = true;
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
                .map(|c| evaluate_condition(c, item, &snapshot, state, now_epoch))
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
                if item
                    .auto_dispatch
                    .as_ref()
                    .is_some_and(|ad| ad.dispatched_run.is_empty())
                {
                    if item.set_state("ready", &now_iso) {
                        out.changed = true;
                    }
                    out.to_auto_dispatch.push(item.clone());
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
        if item.state == "deferred" || item.state == "active" {
            let next_st = if item.has_open_closure() && (item.applied || item.cleaned || !item.runs.is_empty()) {
                "verify"
            } else {
                "ready"
            };
            if item.set_state(next_st, &now_iso) {
                out.changed = true;
            }
        }

        // 5. Stale reminder check (24h cooldown per item)
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

    out
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
            obj
        })
        .collect()
}

fn load_readonly_in(state: &Path) -> Vec<WorkItem> {
    let mut items = load_raw(state);
    if needs_legacy_migration(state) {
        let _ = migrate_legacy_impl(state, &mut items, false);
    }
    let _ = reconcile_all_locked(state, &mut items);
    items
}

fn load_synced_in(state: &Path) -> Res<Vec<WorkItem>> {
    // Fast lock-free path: if no legacy migration is pending and no state changed,
    // return the atomically-written snapshot directly without touching `.state.lock`.
    let needs_mig = needs_legacy_migration(state);
    let mut snapshot = load_raw(state);
    let preview = reconcile_all_locked(state, &mut snapshot);
    if !needs_mig && !preview.changed {
        return Ok(snapshot);
    }

    fs::create_dir_all(state).map_err(|e| format!("{}: {e}", state.display()))?;
    let guard = crate::locks::acquire_state(state).map_err(|e| e.to_string())?;
    let (items, outcome) = {
        let _guard = guard;
        let mut items = load_raw(state);
        let mig = migrate_legacy_locked(state, &mut items);
        let outcome = reconcile_all_locked(state, &mut items);
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
    let preview = reconcile_all_locked(&state, &mut snapshot);
    if !preview.changed {
        return;
    }
    let Ok(_guard) = crate::locks::try_acquire_state(&state) else {
        return;
    };
    let outcome = {
        let mut items = load_raw(&state);
        let mig = migrate_legacy_locked(&state, &mut items);
        let outcome = reconcile_all_locked(&state, &mut items);
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
        w.cleaned = false;
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
                || matches!(
                    st,
                    "rejected" | "failed" | "malformed" | "timeout" | "stalled" | "killed" | "crashed"
                )
                || (st == "delivered" && s(meta, "mode") == "write")
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

        let rec = reconcile_all_locked(&state, &mut items);
        if changed || rec.changed {
            let _ = save_raw(&state, &items);
        }
        rec
    };
    let _ = emit_reconcile_events_and_dispatch(&state, &outcome);
}

pub fn on_run_result_read(run: &Path, meta: &Value) {
    let state = state_dir();
    if !work_path(&state).is_file() {
        return;
    }
    let rid = events::run_id_for(run, meta);
    let work_id = s(meta, "workId").to_string();
    let sum = json(run.join("summary.json"));
    if s(&sum, "state") != "answered" || b(&sum, "question") || s(meta, "mode") == "write" {
        return;
    }
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
        let rec = reconcile_all_locked(&state, &mut items);
        if changed || rec.changed {
            let _ = save_raw(&state, &items);
        }
        rec
    };
    let _ = emit_reconcile_events_and_dispatch(&state, &outcome);
}

fn verify_reasons(item: &WorkItem) -> Vec<String> {
    let mut parts = Vec::new();
    if !item.done_when.trim().is_empty() {
        parts.push(format!("完成条件：{}", clip_chars(&item.done_when, 48)));
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
    parts
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

/// Called after `delegate apply` succeeds on one or more runs.
/// Returns 0..=4 timely hint lines for stdout tail (each with a Host-written pointer).
pub fn on_runs_applied(
    applied_runs: &[(PathBuf, Value, Value)],
    closes_work_id: Option<&str>,
) -> Vec<String> {
    let state = state_dir();
    let closes_norm = closes_work_id.and_then(normalize_work_id);
    let mut hints: Vec<String> = Vec::new();

    // Expand merged source runs if any applied run is a merge run.
    let mut effective_run_ids: Vec<(String, String, Option<PathBuf>, String)> = Vec::new();
    for (run_dir, meta, applied_json) in applied_runs {
        let rid = events::run_id_for(run_dir, meta);
        let head = s(applied_json, "head").to_string();
        let wid = s(meta, "workId").to_string();
        effective_run_ids.push((rid.clone(), wid, Some(run_dir.clone()), head.clone()));

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
                effective_run_ids.push((
                    src_id.to_string(),
                    src_wid,
                    src_dir.is_dir().then_some(src_dir),
                    head.clone(),
                ));
            }
        }
    }

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
        let mut stays: Vec<WorkItem> = Vec::new();
        let mut closed: Vec<WorkItem> = Vec::new();

        for (rid, wid, rdir, head) in &effective_run_ids {
            if let Some(w) = items
                .iter_mut()
                .find(|w| (!wid.is_empty() && w.id == *wid) || w.matches_run(rid))
            {
                w.applied = true;
                if let Some(dir) = rdir {
                    w.add_run(rid, Some(dir), None);
                }
                let explicit_close = closes_norm.as_deref() == Some(w.id.as_str());
                let short_head = if head.len() >= 7 { &head[..7] } else { head.as_str() };
                if explicit_close || !w.has_open_closure() {
                    w.closed_kind = if explicit_close {
                        "closed".into()
                    } else {
                        "auto".into()
                    };
                    w.close_evidence = if short_head.is_empty() {
                        format!("applied {rid}")
                    } else {
                        format!("applied {rid} ({short_head})")
                    };
                    w.set_state("done", &now_iso);
                    closed.push(w.clone());
                    changed = true;
                } else {
                    w.set_state("verify", &now_iso);
                    stays.push(w.clone());
                    changed = true;
                }
            }
        }

        // Handle explicit `--closes <Wxx>` even if the applied run wasn't pre-linked to Wxx
        if let Some(cid) = &closes_norm {
            if !closed.iter().any(|w| w.id == *cid) {
                if let Some(w) = items.iter_mut().find(|w| w.id == *cid) {
                    let first_rid = effective_run_ids
                        .first()
                        .map(|(r, _, _, _)| r.as_str())
                        .unwrap_or("");
                    let first_head = effective_run_ids
                        .first()
                        .map(|(_, _, _, h)| h.as_str())
                        .unwrap_or("");
                    let short_h = &first_head[..first_head.len().min(7)];
                    w.applied = true;
                    w.closed_kind = "closed".into();
                    w.close_evidence = if short_h.is_empty() {
                        format!("applied {first_rid}")
                    } else {
                        format!("applied {first_rid} ({short_h})")
                    };
                    w.set_state("done", &now_iso);
                    closed.push(w.clone());
                    changed = true;
                }
            }
        }

        let rec = reconcile_all_locked(&state, &mut items);
        if changed || rec.changed {
            let _ = save_raw(&state, &items);
        }
        (rec, stays, closed)
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

    // 3. Work item stayed in `verify`
    for w in verify_stays.iter().take(2) {
        let why = verify_reasons(w).join("；");
        hints.push(format!(
            "工作项 [{}] 留在 verify（{}）· 闭环：delegate work close {} --evidence \"…\"（卡片：delegate work show {}）",
            w.id, why, w.id, w.id
        ));
    }

    // 4. Explicitly closed via --closes
    if let Some(cid) = &closes_norm {
        if let Some(w) = auto_closed.iter().find(|w| w.id == *cid) {
            hints.push(format!(
                "已关闭工作项 [{}] {}（{} · 卡片：delegate work show {}）",
                w.id,
                clip_chars(&w.title, 30),
                w.close_evidence,
                w.id
            ));
        }
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

        // 1. Owning work item (1 line)
        if let Some(w) = items
            .iter()
            .find(|w| (!wid.is_empty() && w.id == wid) || w.matches_run(&rid))
        {
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

        // 2. Downstream blocked/ready items waiting on this run (<= 3 lines)
        let mut downstream_count = 0usize;
        for w in &items {
            if w.state == "done" || downstream_count >= 3 {
                continue;
            }
            let waiting_applied = w.wait_on.iter().any(|c| {
                c.strip_prefix("run_applied:")
                    .is_some_and(|r| run_id_matches(&rid, r))
            });
            let waiting_delivered = w.wait_on.iter().any(|c| {
                c.strip_prefix("run_delivered:")
                    .is_some_and(|r| run_id_matches(&rid, r))
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
        for w in &items {
            if w.state == "done" {
                continue;
            }
            if w.wait_on.iter().any(|c| {
                c.strip_prefix("run_applied:")
                    .is_some_and(|r| run_id_matches(rid, r))
            }) && seen.insert(format!("down:{}", w.id))
            {
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
    let report_path = s(report, "reportPath");
    let mut hints = Vec::new();
    let mut seen = HashSet::new();

    for w in &items {
        if w.state == "done" {
            continue;
        }
        let unlocks = w.wait_on.iter().any(|c| {
            c.strip_prefix("run_applied:")
                .is_some_and(|r| merged_runs.iter().any(|mr| run_id_matches(mr, r)))
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
        if let Some(pos) = items
            .iter()
            .position(|w| (!wid.is_empty() && w.id == wid) || w.matches_run(&rid))
        {
            let entry = &mut items[pos];
            if is_applied {
                entry.applied = true;
            }
            let others_live = entry.runs.iter().any(|other| {
                other != &rid
                    && parent_dir.is_some_and(|p| p.join(other).join("meta.json").is_file())
            });
            if !others_live {
                entry.cleaned = true;
            }
            if entry.state != "done"
                && entry.has_open_closure()
                && entry.cleaned
                && (entry.state == "attention" || entry.state == "active")
            {
                entry.set_state("verify", &now_iso);
            }
            entry.updated_at = now_iso;
            snap = Some(entry.clone());
            changed = true;
        }
        // Reconcile downstream `wait_on: run_applied:<rid>` items if this run was applied!
        let outcome = if is_applied {
            let o = reconcile_all_locked(&state, &mut items);
            if o.changed {
                changed = true;
            }
            Some(o)
        } else {
            None
        };
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
                    let files = sum
                        .get("pendingChanges")
                        .filter(|v| v.is_object())
                        .and_then(|v| v["files"].as_u64())
                        .or_else(|| sum["changes"]["files"].as_u64())
                        .unwrap_or(0);
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
                let files = sum
                    .get("pendingChanges")
                    .filter(|v| v.is_object())
                    .and_then(|v| v["files"].as_u64())
                    .or_else(|| sum["changes"]["files"].as_u64())
                    .unwrap_or(0);
                (
                    "attention".to_string(),
                    format!("待合 +{files}f"),
                    format!("apply {}", short_run_id(rid)),
                )
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
    clip_chars(&line, 100)
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
    if !item.done_when.is_empty() {
        lines.push(format!("完成条件：{}", item.done_when));
    }
    if !item.brief.is_empty() {
        lines.push(format!("任务说明：{}", item.brief));
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
            if let Some(rdir) = find_run_dir(rid, &item.run_dirs, state) {
                let st = runs::state(&rdir);
                let applied_tag = if rdir.join(".applied").exists() || rdir.join(".applied-cleaned").exists() {
                    " · 已合入"
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
        lines.push(format!(
            "暂缓：{}（至 {}）",
            item.defer_reason,
            if item.defer_until.is_empty() {
                "已到期"
            } else {
                item.defer_until.as_str()
            }
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

        let sec_is_blocked = section.contains("前置")
            || section.contains("等待")
            || section.contains("阻塞")
            || section.contains("所有者");
        if wait_on.is_empty() && (sec_is_blocked || text.contains("合入后") || text.contains("回来后"))
        {
            let clause = text
                .split(['：', '，', '。'])
                .next()
                .unwrap_or(text)
                .trim();
            wait_on.push(format!("manual:{}", clip_chars(clause, 28)));
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
// Cycle check for `work_closed:<Wxx>`
// ---------------------------------------------------------------------------

fn has_work_cycle(items: &[WorkItem], from_id: &str, new_wait_on: &[String]) -> bool {
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for item in items {
        let deps: Vec<String> = item
            .wait_on
            .iter()
            .filter_map(|c| c.strip_prefix("work_closed:").and_then(normalize_work_id))
            .collect();
        adj.insert(item.id.clone(), deps);
    }
    let added: Vec<String> = new_wait_on
        .iter()
        .filter_map(|c| c.strip_prefix("work_closed:").and_then(normalize_work_id))
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
        other => Err(format!(
            "unknown work subcommand: '{other}' (expected: list, show, add, wait, close, drop, defer, import-md)"
        )),
    }
}

fn list_cmd(args: &[String]) -> Res<i32> {
    let mut show_all = false;
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
    owner_ref: Option<String>,
    brief: Option<String>,
    auto: bool,
    start_args: Vec<String>,
    clear_wait: bool,
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
            "--clear" if allow_clear => wf.clear_wait = true,
            "--brief" | "--prompt-file" => {
                wf.brief = Some(take_val(key, &mut i)?);
            }
            "--after-applied" => {
                let raw = take_val(key, &mut i)?;
                for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    wf.wait_on.push(format!("run_applied:{part}"));
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
                wf.owner_ref = Some(trimmed.to_string());
                wf.wait_on.push(format!("owner:{trimmed}"));
            }
            "--after-time" => {
                let raw = take_val(key, &mut i)?;
                let iso_ts = parse_time_spec(&raw, now_epoch)?;
                wf.wait_on.push(format!("after_time:{iso_ts}"));
            }
            "--wait-manual" => {
                let raw = take_val(key, &mut i)?;
                if raw.trim().is_empty() {
                    return Err("--wait-manual requires a non-empty description".into());
                }
                wf.wait_on.push(format!("manual:{}", raw.trim()));
            }
            "--done-when" | "--repo" | "--artifact" | "--followup" | "--run" => {
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
        dispatched_run: String::new(),
    })
}

fn add_cmd(args: &[String]) -> Res<i32> {
    let ParsedWaitArgs {
        positional,
        wf,
        json_mode,
        extra,
    } = parse_wait_conditions(args, false)?;
    let title = positional.join(" ").trim().to_string();
    if title.is_empty() {
        return Err("usage: delegate work add \"<title>\" [--done-when \"…\"] [--brief <path>] [--after-applied <run>] [--wait-owner <Qxx>] [--auto -- <start-args>]".into());
    }
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
    let brief = wf.brief.unwrap_or_default();
    let auto_dispatch = if wf.auto {
        Some(build_auto_dispatch(&brief, &wf.start_args, &repo)?)
    } else {
        None
    };

    let state = state_dir();
    fs::create_dir_all(&state).map_err(|e| format!("{}: {e}", state.display()))?;
    let now_iso = iso();

    let (created_id, outcome) = {
        let _guard = crate::locks::acquire_state(&state).map_err(|e| e.to_string())?;
        let mut items = load_raw(&state);
        let _ = migrate_legacy_locked(&state, &mut items);

        let id = next_id_locked(&state, &items);
        if has_work_cycle(&items, &id, &wf.wait_on) {
            return Err("dependency cycle detected in --after-work".into());
        }

        let init_state = if wf.wait_on.is_empty() {
            "ready"
        } else {
            "blocked"
        };
        let mut item = WorkItem {
            id: id.clone(),
            title,
            repo: repo.clone(),
            state: init_state.to_string(),
            done_when,
            wait_on: wf.wait_on,
            auto_dispatch,
            brief,
            owner_ref: wf.owner_ref.unwrap_or_default(),
            created_at: now_iso.clone(),
            updated_at: now_iso.clone(),
            state_since: now_iso.clone(),
            origin: "manual".into(),
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
        let rec = reconcile_all_locked(&state, &mut items);
        save_raw(&state, &items)?;
        (id, rec)
    };

    let _ = emit_reconcile_events_and_dispatch(&state, &outcome);
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
            println!("前置：{}", focus_for_item(final_item, &items, &state, now_epoch));
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
        .ok_or("usage: delegate work wait <Wxx> --after-applied <run> [--brief <FILE>] [--auto -- <start-args>]")?;
    let norm = normalize_work_id(raw_id)
        .ok_or_else(|| format!("invalid work id '{raw_id}' (expected W001...)"))?;
    if !wf.clear_wait && wf.wait_on.is_empty() && wf.brief.is_none() && !wf.auto && !extra.contains_key("--artifact") {
        return Err("work wait requires at least one condition (--after-applied / --after-delivered / --after-work / --wait-owner / --after-time / --wait-manual / --clear)".into());
    }

    let state = state_dir();
    let now_iso = iso();
    let outcome = {
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
        let brief_eff = wf
            .brief
            .clone()
            .unwrap_or_else(|| items[pos].brief.clone());
        let ad = if wf.auto {
            Some(build_auto_dispatch(&brief_eff, &wf.start_args, &repo)?)
        } else {
            None
        };

        let item = &mut items[pos];
        if wf.clear_wait {
            item.wait_on.clear();
            item.auto_dispatch = None;
        }
        for c in &wf.wait_on {
            if !item.wait_on.contains(c) {
                item.wait_on.push(c.clone());
            }
        }
        if let Some(oref) = wf.owner_ref {
            item.owner_ref = oref;
        }
        if let Some(b) = wf.brief {
            item.brief = b;
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
            item.set_state("blocked", &now_iso);
        } else if wf.clear_wait && item.state == "blocked" {
            item.set_state("ready", &now_iso);
        }
        item.updated_at = now_iso;

        let rec = reconcile_all_locked(&state, &mut items);
        save_raw(&state, &items)?;
        rec
    };

    let _ = emit_reconcile_events_and_dispatch(&state, &outcome);
    let items = load_raw(&state);
    let updated = items.iter().find(|w| w.id == norm).unwrap();
    let now_epoch = epoch() as i64;
    if json_mode {
        println!("{}", updated.to_json());
    } else {
        println!(
            "已更新 [{}] {} · 状态：{} · {}（卡片：delegate work show {}）",
            updated.id,
            updated.title,
            updated.state,
            focus_for_item(updated, &items, &state, now_epoch),
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
        items[pos].set_state("done", &now_iso);
        let snap = items[pos].clone();
        let rec = reconcile_all_locked(&state, &mut items);
        save_raw(&state, &items)?;
        (snap, rec)
    };

    let dispatched = emit_reconcile_events_and_dispatch(&state, &outcome);
    if !closed_item.runs.is_empty() {
        events::mark_runs_read(&closed_item.runs);
    }
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
        items[pos].set_state("done", &now_iso);
        let snap = items[pos].clone();
        let deps: Vec<String> = items
            .iter()
            .filter(|w| {
                w.state != "done"
                    && w.wait_on
                        .iter()
                        .any(|c| c == &format!("work_closed:{norm}"))
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
        .ok_or("usage: delegate work defer <Wxx|run> --reason \"<暂缓原因>\" [--until <4h|24h|YYYY-MM-DD|ISO>]")?;
    let r = reason
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!("work defer requires --reason \"<原因>\" (e.g. delegate work defer {raw} --reason \"等上游窗口\" --until 4h)")
        })?;
    let now_epoch = epoch() as i64;
    let until_iso = parse_time_spec(until_raw.as_deref().unwrap_or("4h"), now_epoch)?;

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
        items[pos].set_state("deferred", &now_iso);
        let snap = items[pos].clone();
        save_raw(&state, &items)?;
        snap
    };

    if !deferred_item.runs.is_empty() {
        events::mark_runs_read(&deferred_item.runs);
    }
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
        let _ = reconcile_all_locked(&state, &mut items);
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
}
