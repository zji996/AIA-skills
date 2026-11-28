//! Metadata-only owner drop directory perception (`ownerDrop` + optional Q item `dropPath`).
//!
//! Strictly uses `fs::read_dir` and `fs::symlink_metadata` (`lstat`) — NEVER opens or reads
//! file contents, never follows symlinks, imposes no depth or file-count cap (warns when
//! scanned files >= `LARGE_SCAN_WARN_THRESHOLD`), and uses a dual-layer checkpoint:
//! - Read-only paths (`hook check`, `hook pending`, `work [--owner]`, `snapshot --json`, `status`):
//!   lock-free, git-free, zero-write in-memory comparison against `<state>/owner-drop-seen.json`.
//! - Write paths (`work` mutations, `start`, `apply`/`land`, `_gc-worker`):
//!   persists baseline initialization, `owner_drop` events, `owner-drop-seen.json`, and
//!   high-confidence waking of `deferred` owner items.

use crate::common::*;
use crate::config;
use crate::work::{self, OwnerInfo, WorkItem};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const LARGE_SCAN_WARN_THRESHOLD: usize = 1000;

const GENERIC_DROP_WORDS: &[&str] = &[
    "secret",
    "secrets",
    "upload",
    "uploads",
    "drop",
    "drops",
    "file",
    "files",
    "tmp",
    "temp",
    "json",
    "yaml",
    "yml",
    "toml",
    "txt",
    "md",
    "env",
    "png",
    "jpg",
    "jpeg",
    "webp",
    "pdf",
    "zip",
    "tar",
    "gz",
    "log",
];

const STRIP_EXTENSIONS: &[&str] = &[
    ".json", ".yaml", ".yml", ".toml", ".txt", ".md", ".env", ".png", ".jpg", ".jpeg", ".webp",
    ".pdf", ".zip", ".tar", ".gz", ".log", ".pem", ".key", ".crt", ".csv", ".tsv", ".ini", ".cfg",
    ".conf", ".bak",
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeenFile {
    pub mtime_ns: i64,
    pub size: u64,
    pub seen_seq: u64,
    pub acked: bool,
}

impl SeenFile {
    fn from_value(v: &Value) -> Option<Self> {
        if !v.is_object() {
            return None;
        }
        Some(Self {
            mtime_ns: v["mtimeNs"].as_i64().unwrap_or(0),
            size: v["size"].as_u64().unwrap_or(0),
            seen_seq: v["seenSeq"].as_u64().unwrap_or(0),
            acked: b(v, "acked"),
        })
    }

    fn to_json(&self) -> Value {
        json!({
            "mtimeNs": self.mtime_ns,
            "size": self.size,
            "seenSeq": self.seen_seq,
            "acked": self.acked,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct RepoSeen {
    pub initialized_at: String,
    pub files: BTreeMap<String, SeenFile>,
}

impl RepoSeen {
    fn from_value(v: &Value) -> Option<Self> {
        if !v.is_object() {
            return None;
        }
        let mut files = BTreeMap::new();
        if let Some(map) = v["files"].as_object() {
            for (k, fv) in map {
                if !k.is_empty() {
                    if let Some(sf) = SeenFile::from_value(fv) {
                        files.insert(k.clone(), sf);
                    }
                }
            }
        }
        Some(Self {
            initialized_at: s(v, "initializedAt").to_string(),
            files,
        })
    }

    fn to_json(&self) -> Value {
        let mut fm = serde_json::Map::new();
        for (k, sf) in &self.files {
            fm.insert(k.clone(), sf.to_json());
        }
        json!({
            "initializedAt": self.initialized_at,
            "files": Value::Object(fm),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct SeenState {
    pub repos: BTreeMap<String, RepoSeen>,
}

impl SeenState {
    pub fn load(state: &Path) -> Self {
        let path = seen_path(state);
        let doc = json(&path);
        let mut repos = BTreeMap::new();
        if let Some(map) = doc["repos"].as_object() {
            for (k, rv) in map {
                if !k.is_empty() {
                    if let Some(rs) = RepoSeen::from_value(rv) {
                        repos.insert(k.clone(), rs);
                    }
                }
            }
        }
        Self { repos }
    }

    pub fn save(&self, state: &Path) -> Res<()> {
        let mut rm = serde_json::Map::new();
        for (k, rs) in &self.repos {
            rm.insert(k.clone(), rs.to_json());
        }
        let doc = json!({
            "repos": Value::Object(rm),
        });
        write_json(seen_path(state), &doc)
    }
}

pub fn seen_path(state: &Path) -> PathBuf {
    state.join("owner-drop-seen.json")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedFile {
    pub mtime_ns: i64,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDrop {
    pub repo: String,
    pub repo_label: String,
    pub rel_path: String,
    pub mtime_ns: i64,
    pub size: u64,
    pub seen_seq: u64,
    pub is_unsynced: bool,
    pub matched_ids: Vec<String>,
    pub sole_open_act_id: Option<String>,
}

impl PendingDrop {
    pub fn mtime_sec(&self) -> i64 {
        self.mtime_ns.div_euclid(1_000_000_000)
    }

    pub fn local_time_short(&self) -> String {
        format_epoch(self.mtime_sec(), true, "%m-%d %H:%M")
    }

    pub fn to_json(&self) -> Value {
        let mut obj = json!({
            "repo": self.repo,
            "repoLabel": self.repo_label,
            "path": self.rel_path,
            "mtimeNs": self.mtime_ns,
            "size": self.size,
            "seenSeq": self.seen_seq,
            "matchedIds": self.matched_ids,
        });
        if let Some(ref sid) = self.sole_open_act_id {
            obj["soleOpenActId"] = json!(sid);
        }
        obj
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanDiagnostic {
    pub repo: String,
    pub repo_label: String,
    pub file_count: usize,
    pub prefixes: Vec<String>,
}

impl ScanDiagnostic {
    pub fn format_warning_line(&self) -> String {
        let dirs = if self.prefixes.is_empty() {
            "ownerDrop".to_string()
        } else {
            self.prefixes.join("、")
        };
        format!(
            "⚠ 交接目录文件较多：[{}] {} 共 {} 个文件（建议清理旧文件以保持扫描迅捷）",
            self.repo_label, dirs, self.file_count
        )
    }

    pub fn to_json(&self) -> Value {
        json!({
            "repo": self.repo,
            "repoLabel": self.repo_label,
            "fileCount": self.file_count,
            "prefixes": self.prefixes,
            "warning": self.format_warning_line(),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct OwnerDropSnapshot {
    pub pending: Vec<PendingDrop>,
    pub diagnostics: Vec<ScanDiagnostic>,
}

#[derive(Clone, Debug, Default)]
pub struct OwnerDropSyncOutcome {
    pub seen_changed: bool,
    pub work_changed: bool,
    pub new_events: usize,
    pub woken_ids: Vec<String>,
    pub diagnostics: Vec<ScanDiagnostic>,
}

pub fn format_size_human(size: u64) -> String {
    if size == 0 {
        "0 B 空文件".to_string()
    } else if size < 1024 {
        format!("{size} B")
    } else if size < 1024 * 1024 {
        format!("{:.1} KB", size as f64 / 1024.0)
    } else {
        format!("{:.1} MB", size as f64 / (1024.0 * 1024.0))
    }
}

pub fn canonical_repo_key(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let p = Path::new(trimmed);
    fs::canonicalize(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string()
}

pub fn current_repo_fast() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let top = git_top_fast(&cwd)?;
    Some(fs::canonicalize(&top).unwrap_or(top))
}

fn is_ignored_name(name: &str) -> bool {
    name == ".git"
        || name == ".DS_Store"
        || name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swo")
        || name.ends_with(".tmp")
        || name.starts_with(".#")
        || (name.len() >= 2 && name.starts_with('#') && name.ends_with('#'))
}

fn mtime_ns_of(md: &fs::Metadata) -> i64 {
    md.mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(md.mtime_nsec())
}

/// Check that no component from `repo_root` down to `rel_path` is a symlink.
/// Returns `Some(metadata)` of the final target when safe, `None` if missing or symlinked.
fn lstat_no_symlink_ancestors(repo_root: &Path, rel_path: &str) -> Option<(PathBuf, fs::Metadata)> {
    let mut cur = repo_root.to_path_buf();
    let mut last_md = None;
    for seg in rel_path.split('/').filter(|s| !s.is_empty()) {
        cur.push(seg);
        let md = fs::symlink_metadata(&cur).ok()?;
        if md.file_type().is_symlink() {
            return None;
        }
        last_md = Some(md);
    }
    last_md.map(|md| (cur, md))
}

/// Scan configured `ownerDrop` prefixes and explicit `drop_paths` inside `repo_root`.
/// Pure `read_dir` + `symlink_metadata`, zero file opens, no symlink traversal, no depth/file cap.
pub fn scan_repo_drops(
    repo_root: &Path,
    prefixes: &[String],
    extra_drop_paths: &[String],
) -> BTreeMap<String, ScannedFile> {
    let mut files: BTreeMap<String, ScannedFile> = BTreeMap::new();
    let mut stack: Vec<(PathBuf, String)> = Vec::new();
    let mut queued_dirs: BTreeSet<String> = BTreeSet::new();

    for rel in prefixes.iter().chain(extra_drop_paths.iter()) {
        let Ok(norm) = config::validate_relative_drop_path(rel, "ownerDrop") else {
            continue;
        };
        let Some((target, md)) = lstat_no_symlink_ancestors(repo_root, &norm) else {
            continue;
        };
        let ft = md.file_type();
        if ft.is_file() {
            files.insert(
                norm,
                ScannedFile {
                    mtime_ns: mtime_ns_of(&md),
                    size: md.len(),
                },
            );
        } else if ft.is_dir() && queued_dirs.insert(norm.clone()) {
            stack.push((target, norm));
        }
    }

    while let Some((dir_path, rel_dir)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir_path) else {
            continue;
        };
        for entry in rd.flatten() {
            let os_name = entry.file_name();
            let Some(name) = os_name.to_str() else {
                continue;
            };
            if is_ignored_name(name) {
                continue;
            }
            let entry_path = entry.path();
            let Ok(md) = fs::symlink_metadata(&entry_path) else {
                continue;
            };
            let ft = md.file_type();
            if ft.is_symlink() {
                continue;
            }
            let rel = if rel_dir.is_empty() {
                name.to_string()
            } else {
                format!("{rel_dir}/{name}")
            };
            if ft.is_dir() {
                if queued_dirs.insert(rel.clone()) {
                    stack.push((entry_path, rel));
                }
            } else if ft.is_file() {
                files.insert(
                    rel,
                    ScannedFile {
                        mtime_ns: mtime_ns_of(&md),
                        size: md.len(),
                    },
                );
            }
        }
    }

    files
}

fn collect_target_repos(items: &[WorkItem], extra_cwd_repo: Option<&Path>) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    if let Some(p) = extra_cwd_repo {
        let k = canonical_repo_key(&p.to_string_lossy());
        if !k.is_empty() {
            set.insert(k);
        }
    } else if let Some(p) = current_repo_fast() {
        let k = canonical_repo_key(&p.to_string_lossy());
        if !k.is_empty() {
            set.insert(k);
        }
    }
    for w in items {
        if w.state == "done" {
            continue;
        }
        let k = canonical_repo_key(&w.repo);
        if !k.is_empty() {
            set.insert(k);
        }
        for r in &w.repos {
            let rk = canonical_repo_key(r);
            if !rk.is_empty() {
                set.insert(rk);
            }
        }
    }
    set.into_iter().collect()
}

fn extra_drop_paths_for_repo(items: &[WorkItem], repo_key: &str) -> Vec<String> {
    let mut out = Vec::new();
    for w in items {
        if w.state == "done" {
            continue;
        }
        let Some(ow) = &w.owner else {
            continue;
        };
        if ow.drop_path.trim().is_empty() {
            continue;
        }
        if canonical_repo_key(&w.repo) != repo_key {
            continue;
        }
        if let Ok(norm) = config::validate_relative_drop_path(&ow.drop_path, "dropPath") {
            if !out.contains(&norm) {
                out.push(norm);
            }
        }
    }
    out
}

fn strip_drop_prefix<'a>(rel_path: &'a str, prefixes: &[String]) -> &'a str {
    let mut best: Option<&str> = None;
    for p in prefixes {
        if let Some(rest) = rel_path.strip_prefix(p.as_str()) {
            if let Some(after_slash) = rest.strip_prefix('/') {
                if best.is_none_or(|b| after_slash.len() < b.len()) {
                    best = Some(after_slash);
                }
            }
        }
    }
    best.unwrap_or(rel_path)
}

fn strip_common_extensions(rel: &str) -> String {
    let mut cur = rel.to_string();
    loop {
        let lower = cur.to_ascii_lowercase();
        let mut stripped = false;
        for ext in STRIP_EXTENSIONS {
            if lower.ends_with(ext) && cur.len() > ext.len() {
                cur.truncate(cur.len() - ext.len());
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    cur
}

fn filter_generic_tokens(tokens: BTreeSet<String>) -> BTreeSet<String> {
    tokens
        .into_iter()
        .filter(|t| !GENERIC_DROP_WORDS.contains(&t.as_str()))
        .collect()
}

fn path_contains_item_id_boundary(rel_path: &str, item_id: &str) -> bool {
    let mut start = None;
    let bytes = rel_path.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b.is_ascii_alphanumeric() {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s_idx) = start.take() {
            let seg = &rel_path[s_idx..i];
            if work::normalize_work_id(seg).as_deref() == Some(item_id) {
                return true;
            }
        }
    }
    if let Some(s_idx) = start {
        let seg = &rel_path[s_idx..];
        if work::normalize_work_id(seg).as_deref() == Some(item_id) {
            return true;
        }
    }
    false
}

/// Score a high-confidence match between `rel_path` and an owner `WorkItem`.
/// Returns `None` if the match does not meet the high-confidence threshold.
pub fn high_confidence_match_score(
    rel_path: &str,
    prefixes: &[String],
    item: &WorkItem,
) -> Option<usize> {
    let ow = item.owner.as_ref()?;
    if !ow.drop_path.trim().is_empty() {
        if let Ok(dp) = config::validate_relative_drop_path(&ow.drop_path, "dropPath") {
            if rel_path == dp || rel_path.starts_with(&format!("{dp}/")) {
                return Some(1000);
            }
        }
    }
    if path_contains_item_id_boundary(rel_path, &item.id) {
        return Some(500);
    }

    let stripped = strip_drop_prefix(rel_path, prefixes);
    let cleaned = strip_common_extensions(stripped);
    let path_tokens = filter_generic_tokens(crate::history::tokenize(&cleaned));
    if path_tokens.is_empty() {
        return None;
    }
    let item_text = format!("{} {}", item.title, ow.ask);
    let item_tokens = filter_generic_tokens(crate::history::tokenize(&item_text));
    if item_tokens.is_empty() {
        return None;
    }

    let shared: Vec<&String> = path_tokens.intersection(&item_tokens).collect();
    if shared.is_empty() {
        return None;
    }

    let has_specific_ascii = shared.iter().any(|tok| {
        tok.len() >= 4
            && tok.bytes().all(|b| b.is_ascii_alphanumeric())
            && tok.bytes().any(|b| b.is_ascii_alphabetic())
    });
    if shared.len() >= 2 || has_specific_ascii {
        let max_len = shared.iter().map(|t| t.len()).max().unwrap_or(0);
        Some(100 + shared.len() * 10 + max_len)
    } else {
        None
    }
}

/// Match a dropped file against all open owner items in `repo_key`.
/// Returns `(high_confidence_ids, sole_open_act_id_if_unmatched)`.
pub fn match_drop_to_owner_items(
    rel_path: &str,
    prefixes: &[String],
    repo_key: &str,
    items: &[WorkItem],
) -> (Vec<String>, Option<String>) {
    let mut scored: Vec<(usize, u8, String)> = Vec::new();
    let mut open_act_ids: Vec<String> = Vec::new();

    for w in items {
        if w.state == "done" {
            continue;
        }
        let Some(ow) = &w.owner else {
            continue;
        };
        if canonical_repo_key(&w.repo) != repo_key {
            continue;
        }
        if ow.kind == "act" {
            open_act_ids.push(w.id.clone());
        }
        if let Some(score) = high_confidence_match_score(rel_path, prefixes, w) {
            scored.push((score, OwnerInfo::kind_priority(&ow.kind), w.id.clone()));
        }
    }

    if scored.is_empty() {
        let sole = if open_act_ids.len() == 1 {
            open_act_ids.into_iter().next()
        } else {
            None
        };
        return (Vec::new(), sole);
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2)));
    let top_score = scored[0].0;
    let matched: Vec<String> = if top_score >= 500 {
        scored
            .into_iter()
            .filter(|(sc, _, _)| *sc >= 500)
            .map(|(_, _, id)| id)
            .collect()
    } else {
        scored.into_iter().map(|(_, _, id)| id).collect()
    };
    (matched, None)
}

fn drop_hint_text(rel_path: &str, mtime_ns: i64) -> String {
    let ts = mtime_ns.div_euclid(1_000_000_000);
    let local_t = format_epoch(ts, true, "%m-%d %H:%M");
    format!("所有者放了材料：{rel_path}（{local_t}）")
}

/// Strictly read-only, zero-lock, zero-git inspection of owner drop directories.
/// Repositories not yet initialized in `<state>/owner-drop-seen.json` produce no pending drops
/// (preventing historical file storms until the first write-layer baseline is recorded).
pub fn inspect_readonly(
    state: &Path,
    items: &[WorkItem],
    extra_cwd_repo: Option<&Path>,
) -> OwnerDropSnapshot {
    let seen = SeenState::load(state);
    let repos = collect_target_repos(items, extra_cwd_repo);
    let mut snapshot = OwnerDropSnapshot::default();

    for repo_key in repos {
        let repo_path = Path::new(&repo_key);
        let prefixes = config::load_repo_owner_drop(repo_path);
        let extra_paths = extra_drop_paths_for_repo(items, &repo_key);
        if prefixes.is_empty() && extra_paths.is_empty() {
            continue;
        }
        let scanned = scan_repo_drops(repo_path, &prefixes, &extra_paths);
        let repo_label = work::short_repo_name(&repo_key).to_string();
        if scanned.len() >= LARGE_SCAN_WARN_THRESHOLD {
            snapshot.diagnostics.push(ScanDiagnostic {
                repo: repo_key.clone(),
                repo_label: repo_label.clone(),
                file_count: scanned.len(),
                prefixes: prefixes.clone(),
            });
        }
        let Some(repo_seen) = seen.repos.get(&repo_key) else {
            // Uninitialized repo: stay silent in read-only mode until baseline is persisted.
            continue;
        };
        for (rel_path, sf) in &scanned {
            let prev = repo_seen.files.get(rel_path);
            let is_unsynced = prev.is_none_or(|p| p.mtime_ns != sf.mtime_ns || p.size != sf.size);
            let is_pending = is_unsynced || prev.is_some_and(|p| !p.acked);
            if !is_pending {
                continue;
            }
            let seen_seq = if is_unsynced {
                0
            } else {
                prev.map_or(0, |p| p.seen_seq)
            };
            let (matched_ids, sole_open_act_id) =
                match_drop_to_owner_items(rel_path, &prefixes, &repo_key, items);
            snapshot.pending.push(PendingDrop {
                repo: repo_key.clone(),
                repo_label: repo_label.clone(),
                rel_path: rel_path.clone(),
                mtime_ns: sf.mtime_ns,
                size: sf.size,
                seen_seq,
                is_unsynced,
                matched_ids,
                sole_open_act_id,
            });
        }
    }

    snapshot
}

/// Apply in-memory read-only overlay of pending owner drops onto `items`:
/// high-confidence matches wake `deferred` Q/D items in memory without acquiring locks or writing disk.
/// Only unsynced `(mtimeNs, size)` drops wake in memory; once the write layer has synced a given
/// `(mtimeNs, size)` into `owner-drop-seen.json`, subsequent explicit `work defer` is respected
/// until `(mtimeNs, size)` changes again.
pub fn apply_readonly_overlay(
    state: &Path,
    items: &mut [WorkItem],
    extra_cwd_repo: Option<&Path>,
) -> OwnerDropSnapshot {
    let snap = inspect_readonly(state, items, extra_cwd_repo);
    let now_iso = iso();
    for d in &snap.pending {
        if !d.is_unsynced || d.matched_ids.is_empty() {
            continue;
        }
        let hint = drop_hint_text(&d.rel_path, d.mtime_ns);
        for w in items.iter_mut() {
            if w.state == "done" || !d.matched_ids.contains(&w.id) {
                continue;
            }
            if canonical_repo_key(&w.repo) != d.repo {
                continue;
            }
            let Some(kind) = w.owner.as_ref().map(|ow| ow.kind.clone()) else {
                continue;
            };
            if w.state == "deferred" {
                let target = if kind == "direction" {
                    "active"
                } else {
                    "attention"
                };
                w.defer_until.clear();
                w.set_state(target, &now_iso);
            }
            if let Some(ow) = w.owner.as_mut() {
                if ow.last_drop_hint.is_empty() {
                    ow.last_drop_hint = hint.clone();
                }
            }
        }
    }
    snap
}

/// Fast lock-free check used by `check_stale_and_expired` to avoid taking `.state.lock` when
/// no repo needs baseline initialization or drop synchronization.
pub fn needs_sync_readonly(
    state: &Path,
    items: &[WorkItem],
    extra_cwd_repo: Option<&Path>,
) -> bool {
    let seen = SeenState::load(state);
    let repos = collect_target_repos(items, extra_cwd_repo);
    for repo_key in repos {
        let repo_path = Path::new(&repo_key);
        let prefixes = config::load_repo_owner_drop(repo_path);
        let extra_paths = extra_drop_paths_for_repo(items, &repo_key);
        if prefixes.is_empty() && extra_paths.is_empty() {
            continue;
        }
        let Some(repo_seen) = seen.repos.get(&repo_key) else {
            return true;
        };
        let scanned = scan_repo_drops(repo_path, &prefixes, &extra_paths);
        if scanned.len() != repo_seen.files.len() {
            return true;
        }
        for (rel, sf) in &scanned {
            match repo_seen.files.get(rel) {
                Some(prev) if prev.mtime_ns == sf.mtime_ns && prev.size == sf.size => {}
                _ => return true,
            }
        }
    }
    false
}

/// Write-layer sync (caller MUST hold `<state>/.state.lock`).
/// - First time a repo is seen: records all existing files as `acked: true` (silent baseline).
/// - Deleted files: pruned from baseline without alerting.
/// - New or modified `(mtimeNs, size)` files: emits `owner_drop` event to `events.jsonl`,
///   persists `owner-drop-seen.json`, and wakes high-confidence matched `deferred` owner items.
pub fn sync_locked(
    state: &Path,
    items: &mut [WorkItem],
    extra_cwd_repo: Option<&Path>,
) -> OwnerDropSyncOutcome {
    let mut seen = SeenState::load(state);
    let repos = collect_target_repos(items, extra_cwd_repo);
    let now_iso = iso();
    let mut out = OwnerDropSyncOutcome::default();

    for repo_key in repos {
        let repo_path = Path::new(&repo_key);
        let prefixes = config::load_repo_owner_drop(repo_path);
        let extra_paths = extra_drop_paths_for_repo(items, &repo_key);
        if prefixes.is_empty() && extra_paths.is_empty() {
            continue;
        }
        let scanned = scan_repo_drops(repo_path, &prefixes, &extra_paths);
        let repo_label = work::short_repo_name(&repo_key).to_string();
        if scanned.len() >= LARGE_SCAN_WARN_THRESHOLD {
            out.diagnostics.push(ScanDiagnostic {
                repo: repo_key.clone(),
                repo_label,
                file_count: scanned.len(),
                prefixes: prefixes.clone(),
            });
        }

        let repo_seen = match seen.repos.entry(repo_key.clone()) {
            std::collections::btree_map::Entry::Vacant(e) => {
                let mut files = BTreeMap::new();
                for (rel, sf) in scanned {
                    files.insert(
                        rel,
                        SeenFile {
                            mtime_ns: sf.mtime_ns,
                            size: sf.size,
                            seen_seq: 0,
                            acked: true,
                        },
                    );
                }
                e.insert(RepoSeen {
                    initialized_at: now_iso.clone(),
                    files,
                });
                out.seen_changed = true;
                continue;
            }
            std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
        };
        let before_len = repo_seen.files.len();
        repo_seen.files.retain(|k, _| scanned.contains_key(k));
        if repo_seen.files.len() != before_len {
            out.seen_changed = true;
        }

        for (rel_path, sf) in &scanned {
            let is_new_or_changed = match repo_seen.files.get(rel_path) {
                None => true,
                Some(prev) => prev.mtime_ns != sf.mtime_ns || prev.size != sf.size,
            };
            if !is_new_or_changed {
                continue;
            }
            let (matched_ids, sole_open_act_id) =
                match_drop_to_owner_items(rel_path, &prefixes, &repo_key, items);
            let mtime_sec = sf.mtime_ns.div_euclid(1_000_000_000);
            let mut payload = json!({
                "repo": repo_key,
                "path": rel_path,
                "name": rel_path,
                "size": sf.size,
                "mtimeNs": sf.mtime_ns,
                "mtime": format_epoch(mtime_sec, false, "%Y-%m-%dT%H:%M:%SZ"),
                "matchedIds": matched_ids,
            });
            if let Some(ref sid) = sole_open_act_id {
                payload["soleOpenActId"] = json!(sid);
            }
            let seq = crate::events::emit_in(state, "", &repo_key, "owner_drop", payload)
                .unwrap_or(0);
            repo_seen.files.insert(
                rel_path.clone(),
                SeenFile {
                    mtime_ns: sf.mtime_ns,
                    size: sf.size,
                    seen_seq: seq,
                    acked: false,
                },
            );
            out.seen_changed = true;
            out.new_events += 1;

            if !matched_ids.is_empty() {
                let hint = drop_hint_text(rel_path, sf.mtime_ns);
                for w in items.iter_mut() {
                    if w.state == "done" || !matched_ids.contains(&w.id) {
                        continue;
                    }
                    if canonical_repo_key(&w.repo) != repo_key {
                        continue;
                    }
                    let Some(kind) = w.owner.as_ref().map(|ow| ow.kind.clone()) else {
                        continue;
                    };
                    if w.state == "deferred" {
                        let target = if kind == "direction" {
                            "active"
                        } else {
                            "attention"
                        };
                        w.defer_until.clear();
                        w.set_state(target, &now_iso);
                        out.woken_ids.push(w.id.clone());
                        out.work_changed = true;
                    }
                    let hint_changed = if let Some(ow) = w.owner.as_mut() {
                        if ow.last_drop_hint != hint {
                            ow.last_drop_hint = hint.clone();
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if hint_changed {
                        w.updated_at = now_iso.clone();
                        out.work_changed = true;
                    }
                }
            }
        }
    }

    if out.seen_changed {
        let _ = seen.save(state);
    }
    out
}

/// Automatically acknowledge pending drops matched to a closed/dropped owner item (`Qxx`/`Dxxx`).
/// Caller MUST hold `<state>/.state.lock`.
pub fn ack_matched_for_work_locked(state: &Path, closed_item: &WorkItem) -> usize {
    let Some(ow) = &closed_item.owner else {
        return 0;
    };
    let repo_key = canonical_repo_key(&closed_item.repo);
    if repo_key.is_empty() {
        return 0;
    }
    let repo_path = Path::new(&repo_key);
    let prefixes = config::load_repo_owner_drop(repo_path);
    let mut extra_paths = Vec::new();
    if !ow.drop_path.trim().is_empty() {
        if let Ok(norm) = config::validate_relative_drop_path(&ow.drop_path, "dropPath") {
            extra_paths.push(norm);
        }
    }
    if prefixes.is_empty() && extra_paths.is_empty() {
        return 0;
    }

    let scanned = scan_repo_drops(repo_path, &prefixes, &extra_paths);
    let mut seen = SeenState::load(state);
    let now_iso = iso();
    let repo_seen = match seen.repos.entry(repo_key) {
        std::collections::btree_map::Entry::Vacant(e) => {
            let mut files = BTreeMap::new();
            for (rel, sf) in scanned {
                files.insert(
                    rel,
                    SeenFile {
                        mtime_ns: sf.mtime_ns,
                        size: sf.size,
                        seen_seq: 0,
                        acked: true,
                    },
                );
            }
            e.insert(RepoSeen {
                initialized_at: now_iso,
                files,
            });
            let _ = seen.save(state);
            return 0;
        }
        std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
    };
    let mut acked_count = 0usize;
    let mut changed = false;

    for (rel_path, sf) in &scanned {
        if high_confidence_match_score(rel_path, &prefixes, closed_item).is_none() {
            continue;
        }
        let prev = repo_seen.files.get(rel_path);
        let needs_ack = match prev {
            None => true,
            Some(p) => !p.acked || p.mtime_ns != sf.mtime_ns || p.size != sf.size,
        };
        if needs_ack {
            let prev_seq = prev
                .filter(|p| p.mtime_ns == sf.mtime_ns && p.size == sf.size)
                .map_or(0, |p| p.seen_seq);
            repo_seen.files.insert(
                rel_path.clone(),
                SeenFile {
                    mtime_ns: sf.mtime_ns,
                    size: sf.size,
                    seen_seq: prev_seq,
                    acked: true,
                },
            );
            acked_count += 1;
            changed = true;
        }
    }

    if changed {
        let _ = seen.save(state);
    }
    acked_count
}

/// Explicitly acknowledge one or more drop paths (or `--all`) in `repo_path`.
/// Caller MUST hold `<state>/.state.lock`.
/// Returns `(acked_paths, work_changed)`.
pub fn ack_drop_locked(
    state: &Path,
    repo_path: &Path,
    paths: &[String],
    all_flag: bool,
    items: &mut [WorkItem],
) -> Res<(Vec<String>, bool)> {
    let repo_key = canonical_repo_key(&repo_path.to_string_lossy());
    if repo_key.is_empty() {
        return Err("cannot determine target repository for ack-drop".into());
    }
    let canon_root = Path::new(&repo_key);
    let prefixes = config::load_repo_owner_drop(canon_root);
    let extra_paths = extra_drop_paths_for_repo(items, &repo_key);
    let scanned = scan_repo_drops(canon_root, &prefixes, &extra_paths);

    let mut seen = SeenState::load(state);
    let now_iso = iso();
    let repo_seen = seen.repos.entry(repo_key.clone()).or_insert_with(|| {
        let mut files = BTreeMap::new();
        for (rel, sf) in &scanned {
            files.insert(
                rel.clone(),
                SeenFile {
                    mtime_ns: sf.mtime_ns,
                    size: sf.size,
                    seen_seq: 0,
                    acked: true,
                },
            );
        }
        RepoSeen {
            initialized_at: now_iso.clone(),
            files,
        }
    });

    let mut acked_paths = Vec::new();
    if all_flag {
        repo_seen.files.retain(|k, _| scanned.contains_key(k));
        for (rel, sf) in &scanned {
            let prev = repo_seen.files.get(rel);
            let was_pending = match prev {
                None => true,
                Some(p) => !p.acked || p.mtime_ns != sf.mtime_ns || p.size != sf.size,
            };
            let prev_seq = prev
                .filter(|p| p.mtime_ns == sf.mtime_ns && p.size == sf.size)
                .map_or(0, |p| p.seen_seq);
            repo_seen.files.insert(
                rel.clone(),
                SeenFile {
                    mtime_ns: sf.mtime_ns,
                    size: sf.size,
                    seen_seq: prev_seq,
                    acked: true,
                },
            );
            if was_pending {
                acked_paths.push(rel.clone());
            }
        }
    } else {
        for raw_p in paths {
            let norm = config::validate_relative_drop_path(raw_p, "path")?;
            if let Some(sf) = scanned.get(&norm) {
                let prev_seq = repo_seen
                    .files
                    .get(&norm)
                    .filter(|p| p.mtime_ns == sf.mtime_ns && p.size == sf.size)
                    .map_or(0, |p| p.seen_seq);
                repo_seen.files.insert(
                    norm.clone(),
                    SeenFile {
                        mtime_ns: sf.mtime_ns,
                        size: sf.size,
                        seen_seq: prev_seq,
                        acked: true,
                    },
                );
                acked_paths.push(norm);
            } else if let Some((_, md)) = lstat_no_symlink_ancestors(canon_root, &norm) {
                if md.file_type().is_file() {
                    let mtime_ns = mtime_ns_of(&md);
                    let size = md.len();
                    let prev_seq = repo_seen
                        .files
                        .get(&norm)
                        .filter(|p| p.mtime_ns == mtime_ns && p.size == size)
                        .map_or(0, |p| p.seen_seq);
                    repo_seen.files.insert(
                        norm.clone(),
                        SeenFile {
                            mtime_ns,
                            size,
                            seen_seq: prev_seq,
                            acked: true,
                        },
                    );
                    acked_paths.push(norm);
                } else {
                    return Err(format!("drop path is not a regular file: {norm}"));
                }
            } else if repo_seen.files.remove(&norm).is_some() {
                acked_paths.push(norm);
            } else {
                return Err(format!(
                    "drop file not found in {}: {norm}",
                    canon_root.display()
                ));
            }
        }
    }

    seen.save(state)?;

    // Clear stale `last_drop_hint` on open owner items referencing acked paths.
    let mut work_changed = false;
    for w in items.iter_mut() {
        if w.state == "done" || canonical_repo_key(&w.repo) != repo_key {
            continue;
        }
        if let Some(ow) = w.owner.as_mut() {
            if ow.last_drop_hint.is_empty() {
                continue;
            }
            let should_clear = all_flag
                || acked_paths
                    .iter()
                    .any(|p| ow.last_drop_hint.contains(p.as_str()));
            if should_clear {
                ow.last_drop_hint.clear();
                w.updated_at = now_iso.clone();
                work_changed = true;
            }
        }
    }

    Ok((acked_paths, work_changed))
}

/// Strictly read-only diagnostic check for `delegate status` (warns when scanned files >= 1000).
pub fn diagnostics_readonly(
    state: &Path,
    extra_cwd_repo: Option<&Path>,
) -> Vec<ScanDiagnostic> {
    let items = work::load_raw_readonly(state);
    inspect_readonly(state, &items, extra_cwd_repo).diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn temp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "delegate-owner-drop-{name}-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn make_owner_q(id: &str, repo: &Path, title: &str, ask: &str, state: &str, drop_path: &str) -> WorkItem {
        WorkItem {
            id: id.into(),
            title: title.into(),
            repo: repo.to_string_lossy().into_owned(),
            state: state.into(),
            defer_until: if state == "deferred" {
                "2099-01-01T00:00:00Z".into()
            } else {
                String::new()
            },
            owner: Some(OwnerInfo {
                kind: "act".into(),
                ask: ask.into(),
                recommendation: "建议尽快完成".into(),
                if_undecided: "阻塞下游".into(),
                cost: "5 分钟".into(),
                drop_path: drop_path.into(),
                last_drop_hint: String::new(),
            }),
            created_at: "2026-11-28T00:00:00Z".into(),
            updated_at: "2026-11-28T00:00:00Z".into(),
            state_since: "2026-11-28T00:00:00Z".into(),
            ..Default::default()
        }
    }

    #[test]
    fn config_validates_owner_drop_and_rejects_user_level_abs_and_parent() {
        let p = Path::new("/tmp/fake/.delegate.json");
        assert!(config::validate(&json!({"ownerDrop": ["tmp/", "inbox/files"]}), p, true).is_ok());
        assert!(config::validate(&json!({"ownerDrop": ["/abs/path"]}), p, true).is_err());
        assert!(config::validate(&json!({"ownerDrop": ["../escape"]}), p, true).is_err());
        assert!(config::validate(&json!({"ownerDrop": ["tmp/../escape"]}), p, true).is_err());
        assert!(config::validate(&json!({"ownerDrop": [""]}), p, true).is_err());
        assert!(config::validate(&json!({"ownerDrop": ["."]}), p, true).is_err());
        // User-level config must reject ownerDrop
        assert!(config::validate(&json!({"ownerDrop": ["tmp/"]}), p, false).is_err());
    }

    #[test]
    fn unconfigured_repo_has_zero_behavior() {
        let root = temp_dir("unconfigured");
        let state = root.join("state");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join("tmp")).unwrap();
        fs::write(repo.join("tmp/anything.txt"), "hello").unwrap();

        let items = vec![make_owner_q("Q01", &repo, "测试项", "请提供材料", "deferred", "")];
        assert!(!needs_sync_readonly(&state, &items, Some(&repo)));
        let snap = inspect_readonly(&state, &items, Some(&repo));
        assert!(snap.pending.is_empty());
        assert!(snap.diagnostics.is_empty());

        let mut mut_items = items.clone();
        let out = sync_locked(&state, &mut mut_items, Some(&repo));
        assert!(!out.seen_changed);
        assert!(!out.work_changed);
        assert_eq!(out.new_events, 0);
        assert!(!seen_path(&state).exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn first_baseline_is_silent_and_subsequent_drop_including_chmod_000_and_deep_dirs_triggers() {
        let root = temp_dir("baseline-chmod000");
        let state = root.join("state");
        let repo = root.join("repo");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(repo.join("tmp/a/b/c/d/e/f/g/h")).unwrap();
        fs::create_dir_all(repo.join("tmp/.git")).unwrap();
        write_json(repo.join(".delegate.json"), &json!({"ownerDrop": ["tmp/"]})).unwrap();

        // Pre-existing files (including depth > 6) and ignored noise / symlink cycle
        fs::write(repo.join("tmp/legacy.txt"), "old").unwrap();
        fs::write(repo.join("tmp/a/b/c/d/e/f/g/h/deep-old.txt"), "deep").unwrap();
        fs::write(repo.join("tmp/.DS_Store"), "noise").unwrap();
        fs::write(repo.join("tmp/edit.swp"), "noise").unwrap();
        fs::write(repo.join("tmp/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let _ = symlink(repo.join("tmp"), repo.join("tmp/loop-link"));

        let mut items = vec![make_owner_q(
            "Q01",
            &repo,
            "建 Acme OAuth App",
            "请把 Acme OAuth 凭据放进交接目录",
            "deferred",
            "",
        )];

        // Read-only before baseline is initialized stays silent (no historical storm)
        let pre_snap = inspect_readonly(&state, &items, Some(&repo));
        assert!(pre_snap.pending.is_empty());

        // First write sync silently baselines all existing files
        let out1 = sync_locked(&state, &mut items, Some(&repo));
        assert!(out1.seen_changed);
        assert!(!out1.work_changed);
        assert_eq!(out1.new_events, 0);
        assert_eq!(items[0].state, "deferred");

        let seen = SeenState::load(&state);
        let rk = canonical_repo_key(&repo.to_string_lossy());
        let rs = seen.repos.get(&rk).expect("baseline repo");
        assert!(rs.files.contains_key("tmp/legacy.txt"));
        assert!(rs.files.contains_key("tmp/a/b/c/d/e/f/g/h/deep-old.txt"));
        assert!(!rs.files.contains_key("tmp/.DS_Store"));
        assert!(!rs.files.contains_key("tmp/edit.swp"));
        assert!(!rs.files.contains_key("tmp/.git/HEAD"));
        assert!(rs.files.values().all(|f| f.acked));

        // Drop a chmod 000 unreadable file + deep new file
        let secret_dir = repo.join("tmp/secret");
        fs::create_dir_all(&secret_dir).unwrap();
        let unreadable = secret_dir.join("acme-oauth");
        fs::write(&unreadable, "never-read-this-content").unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();

        // Read-only overlay detects immediately without opening file and wakes Q01 in memory
        let mut ro_items = items.clone();
        let ro_snap = apply_readonly_overlay(&state, &mut ro_items, Some(&repo));
        assert_eq!(ro_snap.pending.len(), 1);
        assert_eq!(ro_snap.pending[0].rel_path, "tmp/secret/acme-oauth");
        assert_eq!(ro_snap.pending[0].size, 23);
        assert_eq!(ro_snap.pending[0].matched_ids, vec!["Q01"]);
        assert_eq!(ro_items[0].state, "attention");
        assert!(ro_items[0].defer_until.is_empty());
        assert!(ro_items[0]
            .owner
            .as_ref()
            .unwrap()
            .last_drop_hint
            .contains("所有者放了材料：tmp/secret/acme-oauth"));

        // Write sync emits owner_drop event and persists woken state
        let out2 = sync_locked(&state, &mut items, Some(&repo));
        assert_eq!(out2.new_events, 1);
        assert!(out2.work_changed);
        assert_eq!(out2.woken_ids, vec!["Q01"]);
        assert_eq!(items[0].state, "attention");
        let evs = crate::events::read_since_in(&state, 0);
        assert_eq!(evs.len(), 1);
        assert_eq!(s(&evs[0], "kind"), "owner_drop");
        assert_eq!(s(&evs[0]["payload"], "path"), "tmp/secret/acme-oauth");

        // Restore permissions so cleanup succeeds
        let _ = fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unrelated_file_does_not_wake_deferred_q_and_ack_prevents_rewake() {
        let root = temp_dir("unrelated-and-ack");
        let state = root.join("state");
        let repo = root.join("repo");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(repo.join("tmp")).unwrap();
        write_json(repo.join(".delegate.json"), &json!({"ownerDrop": ["tmp/"]})).unwrap();

        let mut items = vec![
            make_owner_q(
                "Q01",
                &repo,
                "建 Acme OAuth App",
                "请建 Acme OAuth 凭据",
                "deferred",
                "",
            ),
            make_owner_q(
                "Q02",
                &repo,
                "提供云盘证书",
                "请放好云盘证书",
                "deferred",
                "tmp/certs/cloud.pem",
            ),
        ];

        // Baseline
        let _ = sync_locked(&state, &mut items, Some(&repo));

        // Drop unrelated file -> reminds, does NOT wake Q01 or Q02
        fs::write(repo.join("tmp/random.log"), "noise").unwrap();
        let snap = apply_readonly_overlay(&state, &mut items, Some(&repo));
        assert_eq!(snap.pending.len(), 1);
        assert!(snap.pending[0].matched_ids.is_empty());
        assert_eq!(items[0].state, "deferred");
        assert_eq!(items[1].state, "deferred");

        // Ack unrelated file -> disappears from pending
        let (acked, _) = ack_drop_locked(&state, &repo, &["tmp/random.log".into()], false, &mut items).unwrap();
        assert_eq!(acked, vec!["tmp/random.log"]);
        assert!(inspect_readonly(&state, &items, Some(&repo)).pending.is_empty());

        // Drop exact dropPath for Q02 -> wakes Q02 only
        fs::create_dir_all(repo.join("tmp/certs")).unwrap();
        fs::write(repo.join("tmp/certs/cloud.pem"), "cert-bytes").unwrap();
        let out = sync_locked(&state, &mut items, Some(&repo));
        assert_eq!(out.woken_ids, vec!["Q02"]);
        assert_eq!(items[0].state, "deferred");
        assert_eq!(items[1].state, "attention");

        // Ack Q02's drop and re-defer Q02 -> same (mtime, size) NEVER wakes Q02 again
        let (acked2, _) = ack_drop_locked(&state, &repo, &["tmp/certs/cloud.pem".into()], false, &mut items).unwrap();
        assert_eq!(acked2, vec!["tmp/certs/cloud.pem"]);
        items[1].state = "deferred".into();
        items[1].defer_until = "2099-01-01T00:00:00Z".into();

        let snap_after = apply_readonly_overlay(&state, &mut items, Some(&repo));
        assert!(snap_after.pending.is_empty());
        assert_eq!(items[1].state, "deferred");
        let out_after = sync_locked(&state, &mut items, Some(&repo));
        assert_eq!(out_after.new_events, 0);
        assert_eq!(items[1].state, "deferred");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn large_drop_dir_scans_completely_and_emits_diagnostic() {
        let root = temp_dir("large-dir");
        let state = root.join("state");
        let repo = root.join("repo");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(repo.join("tmp")).unwrap();
        write_json(repo.join(".delegate.json"), &json!({"ownerDrop": ["tmp/"]})).unwrap();

        for i in 0..LARGE_SCAN_WARN_THRESHOLD {
            fs::write(repo.join(format!("tmp/f-{i:04}.txt")), "").unwrap();
        }
        let scanned = scan_repo_drops(&repo, &["tmp".into()], &[]);
        assert_eq!(scanned.len(), LARGE_SCAN_WARN_THRESHOLD);

        let snap = inspect_readonly(&state, &[], Some(&repo));
        assert_eq!(snap.diagnostics.len(), 1);
        assert_eq!(snap.diagnostics[0].file_count, LARGE_SCAN_WARN_THRESHOLD);
        assert!(snap.diagnostics[0].format_warning_line().contains("交接目录文件较多"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn synced_unacked_drop_respects_subsequent_defer_in_readonly_until_file_changes() {
        let root = temp_dir("synced-unacked-defer");
        let state = root.join("state");
        let repo = root.join("repo");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(repo.join("tmp/secret")).unwrap();
        write_json(repo.join(".delegate.json"), &json!({"ownerDrop": ["tmp/"]})).unwrap();

        let mut items = vec![make_owner_q(
            "Q01",
            &repo,
            "建 Acme OAuth App",
            "请把 Acme OAuth 凭据放进交接目录",
            "deferred",
            "",
        )];

        // 1. Silent baseline
        let _ = sync_locked(&state, &mut items, Some(&repo));

        // 2. Drop matching file -> write sync wakes Q01 and records (mtimeNs, size, acked: false)
        let fpath = repo.join("tmp/secret/acme-oauth");
        fs::write(&fpath, "v1").unwrap();
        let out1 = sync_locked(&state, &mut items, Some(&repo));
        assert_eq!(out1.woken_ids, vec!["Q01"]);
        assert_eq!(items[0].state, "attention");

        // 3. User explicitly re-defers Q01 WITHOUT calling `ack-drop`
        items[0].state = "deferred".into();
        items[0].defer_until = "2099-01-01T00:00:00Z".into();
        items[0].owner.as_mut().unwrap().last_drop_hint.clear();

        // Read-only overlay MUST respect `deferred` (and NOT re-populate last_drop_hint)
        // because `(mtimeNs, size)` is already synced (`is_unsynced == false`),
        // while `snap.pending` still reports the unacked drop for reminder surfaces.
        let mut ro_items = items.clone();
        let snap = apply_readonly_overlay(&state, &mut ro_items, Some(&repo));
        assert_eq!(snap.pending.len(), 1);
        assert!(!snap.pending[0].is_unsynced);
        assert_eq!(ro_items[0].state, "deferred");
        assert!(ro_items[0].owner.as_ref().unwrap().last_drop_hint.is_empty());

        // 4. Updating the file (new size/mtime) flips `is_unsynced == true` and wakes Q01 again
        fs::write(&fpath, "v2-updated-content").unwrap();
        let mut ro_items2 = items.clone();
        let snap2 = apply_readonly_overlay(&state, &mut ro_items2, Some(&repo));
        assert_eq!(snap2.pending.len(), 1);
        assert!(snap2.pending[0].is_unsynced);
        assert_eq!(ro_items2[0].state, "attention");
        assert!(ro_items2[0]
            .owner
            .as_ref()
            .unwrap()
            .last_drop_hint
            .contains("tmp/secret/acme-oauth"));

        let _ = fs::remove_dir_all(&root);
    }
}
