//! Unified, strictly zero-lock, zero-git, zero-subprocess Stop hook kernel
//! serving `delegate hook check --format claude|agy|json` and `delegate hook pending [--json]`.
//!
//! Shares the exact actionable-pending run classification rule with `delegate-tui`:
//! - Actionable pending runs: unapplied non-zero-change write deliveries (leaf of chain),
//!   unanswered questioning runs, unread failed/rejected/error runs (`resultReadAt` empty),
//!   unread read-only `answered` (and unread zero-change) deliveries.
//! - Non-pending runs: read `answered` / read zero-change / read failed/rejected (collected),
//!   `cleaned`, superseded ancestor rounds in a continuation chain, applied, stopped, active.

use crate::common::*;
use crate::work::{self, WorkItem};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const DEFAULT_STALE_VERIFY_HOURS: f64 = 4.0;
pub const DEFAULT_STALE_READY_HOURS: f64 = 8.0;
pub const DEFAULT_MISSING_BRIEF_GRACE_HOURS: f64 = 1.0;
pub const DEFAULT_REMIND_COOLDOWN_HOURS: f64 = 6.0;
pub const DEFAULT_ORPHAN_TAKEOVER_HOURS: f64 = 6.0;
pub const DEFAULT_STALL_PENETRATE_MINUTES: f64 = 30.0;
pub const DEFAULT_STALL_VERIFY_MINUTES: f64 = 30.0;
pub const TITLE_MAX_CHARS: usize = 40;
pub const DEFAULT_MAX_ACTIVE: usize = 32;
pub const DEFAULT_REPO_MAX_ACTIVE: usize = 0;

const UNREAD_ALERT_KINDS: &[&str] = &[
    "delivered",
    "question",
    "rejected",
    "failed",
    "stall_warning",
    "queue_paused",
    "needs_attention",
    "premerge_conflict",
    "premerge_failed",
    "work_ready",
    "work_defer_expired",
    "work_stale",
];

fn kind_label(kind: &str) -> &str {
    match kind {
        "delivered" => "交付",
        "question" => "提问",
        "rejected" => "未通过",
        "failed" => "失败",
        "stall_warning" => "卡住提醒",
        "queue_paused" => "续派暂停",
        "review_started" => "开始审查",
        "review_verdict" => "审查判定",
        "rework_sent" => "退回返工",
        "needs_attention" => "需主控关注",
        "premerge_ready" => "预合并就绪",
        "premerge_conflict" => "预合并冲突",
        "premerge_failed" => "预合并失败",
        "premerge_cache_healed" => "预合并缓存自愈",
        "accept_blocked_mainline" => "主干阻塞",
        "accept_rerun" => "自动重验",
        "adopted" => "接手会话",
        "merge_started" => "开始合入",
        "merge_item" => "合入进度",
        "merge_done" => "合入完成",
        "radar_unchanged" => "侦察未变",
        "radar_done" => "侦察完成",
        "work_ready" => "工作项就绪",
        "work_dispatched" => "工作项自动派发",
        "work_defer_expired" => "暂缓到期",
        "work_stale" => "工作项停滞",
        other => other,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookFormat {
    Claude,
    Agy,
    Json,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EvalMode {
    Check,
    Pending,
}

/// Shared actionable-pending rule (mirrored in `delegate-tui/src/data.rs`):
/// - Counted: unapplied write delivery (non-zero-change, not superseded); questioning run;
///   failed/rejected/error run whose result has not been read (`resultReadAt` empty);
///   read-only `answered` (or zero-change delivery) whose result has not been read.
/// - Excluded: read `answered` / read zero-change / read failed/rejected; `cleaned`;
///   superseded ancestor rounds in a continuation chain; applied; stopped; active runs.
#[allow(clippy::too_many_arguments)]
pub fn is_run_pending_actionable(
    state: &str,
    mode: &str,
    applied: bool,
    cleaned: bool,
    question: bool,
    result_read: bool,
    superseded: bool,
    zero_change: bool,
) -> bool {
    if superseded || cleaned || state == "cleaned" || applied {
        return false;
    }
    if matches!(
        state,
        "running" | "starting" | "waiting" | "queued" | "scouting" | "stopped" | "blocked_mainline"
    ) {
        return false;
    }
    if question {
        return true;
    }
    if matches!(state, "delivered" | "answered") && mode == "write" {
        return if zero_change { !result_read } else { true };
    }
    !result_read
}

/// Count and list actionable pending run IDs from a slice of run status JSON objects
/// using the exact shared rule (used by both `hook pending` and shared fixture tests).
pub fn classify_pending_runs(runs: &[Value]) -> Vec<String> {
    let mut by_id: HashMap<String, &Value> = HashMap::new();
    let mut parents: HashSet<String> = HashSet::new();
    for obj in runs {
        let rid = s(obj, "run");
        if !rid.is_empty() {
            by_id.insert(rid.to_string(), obj);
        }
        let parent = s(obj, "parent");
        if !parent.is_empty() {
            parents.insert(parent.to_string());
        }
    }
    let mut out = Vec::new();
    for obj in runs {
        let rid = s(obj, "run");
        if rid.is_empty() {
            continue;
        }
        let superseded = parents.contains(rid);
        let state = s(obj, "state");
        let mode = s(obj, "mode");
        let applied = b(obj, "applied") || !s(obj, "appliedAt").trim().is_empty();
        let cleaned = b(obj, "cleaned") || state == "cleaned";
        let replied = !s(obj, "repliedAt").trim().is_empty();
        let question = b(obj, "question") && !replied;
        let result_read = !s(obj, "resultReadAt").trim().is_empty();
        let zero_change = is_zero_change_delivery_ref(obj, &by_id);
        if is_run_pending_actionable(
            state,
            mode,
            applied,
            cleaned,
            question,
            result_read,
            superseded,
            zero_change,
        ) {
            out.push(rid.to_string());
        }
    }
    out.sort();
    out
}

pub(crate) fn is_zero_change_delivery_ref(obj: &Value, all_by_id: &HashMap<String, &Value>) -> bool {
    if !matches!(s(obj, "state"), "delivered" | "answered") {
        return false;
    }
    if s(obj, "mode") != "write" {
        return false;
    }
    if let Some(files) = obj
        .get("pendingChanges")
        .and_then(|p| p.get("files"))
        .and_then(Value::as_u64)
    {
        return files == 0;
    }
    let cur_changes = obj
        .get("changes")
        .and_then(|c| c.get("files"))
        .and_then(Value::as_u64);
    if cur_changes.is_some_and(|n| n > 0) {
        return false;
    }
    let wt = s(obj, "worktree");
    let mut parent = s(obj, "parent").to_string();
    let mut visited = HashSet::new();
    while !parent.is_empty() && visited.insert(parent.clone()) {
        let Some(anc) = all_by_id.get(&parent) else {
            break;
        };
        if b(anc, "applied") {
            break;
        }
        if !wt.is_empty() && s(anc, "worktree") != wt {
            break;
        }
        if anc
            .get("pendingChanges")
            .and_then(|p| p.get("files"))
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 0)
        {
            return false;
        }
        if anc
            .get("changes")
            .and_then(|c| c.get("files"))
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 0)
        {
            return false;
        }
        parent = s(anc, "parent").to_string();
    }
    if let Some(files) = cur_changes {
        return files == 0;
    }
    if let Some(files) = obj.get("files").and_then(Value::as_array) {
        return files.is_empty();
    }
    false
}

fn is_zero_change_delivery(obj: &Value, all_by_id: &HashMap<String, Value>) -> bool {
    let refs: HashMap<String, &Value> = all_by_id.iter().map(|(k, v)| (k.clone(), v)).collect();
    is_zero_change_delivery_ref(obj, &refs)
}

pub fn clip_title(text: &str, max_chars: usize) -> String {
    let clean = text.replace(['\n', '\r', '\t'], " ");
    let clean = clean.trim();
    if clean.chars().count() <= max_chars {
        return clean.to_string();
    }
    let mut out: String = clean.chars().take(max_chars).collect();
    out.push('…');
    out
}

fn env_nonempty(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn parse_pos_f64_env(name: &str, default: f64) -> f64 {
    env_nonempty(name)
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|&v| v > 0.0 && v.is_finite())
        .unwrap_or(default)
}

fn parse_nonneg_usize_env(name: &str) -> Option<usize> {
    env_nonempty(name).and_then(|raw| raw.parse::<usize>().ok())
}

fn bool_or_true_str(v: &Value) -> bool {
    v.as_bool() == Some(true)
        || v.as_str()
            .is_some_and(|st| st.trim().eq_ignore_ascii_case("true"))
}

/// Entry guards evaluated BEFORE touching disk:
/// - Colleague env (`PI_DELEGATE_ACTIVE`, `DELEGATE_AGENT`, `DELEGATE_RUN_DIR`) -> allow
/// - Loop guard (`stop_hook_active` or `continuationActive`) -> allow
/// - `agy` `terminationReason` non-empty and != `"NO_TOOL_CALL"` -> allow
pub fn check_entry_guards(payload: &Value, format: HookFormat) -> Option<&'static str> {
    check_entry_guards_with(payload, format, env_nonempty)
}

fn check_entry_guards_with(
    payload: &Value,
    format: HookFormat,
    get_env: impl Fn(&str) -> Option<String>,
) -> Option<&'static str> {
    if get_env("PI_DELEGATE_ACTIVE").is_some_and(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        || get_env("DELEGATE_AGENT").is_some()
        || get_env("DELEGATE_RUN_DIR").is_some()
    {
        return Some("colleague");
    }
    if bool_or_true_str(&payload["stop_hook_active"])
        || bool_or_true_str(&payload["continuationActive"])
    {
        return Some("loop");
    }
    let term_reason = s(payload, "terminationReason").trim();
    if !term_reason.is_empty()
        && term_reason != "NO_TOOL_CALL"
        && matches!(format, HookFormat::Agy | HookFormat::Json)
    {
        return Some("termination");
    }
    None
}

/// Caller resolution priority:
/// `DELEGATE_CALLER` > `payload.session_id` / `payload.conversationId` > `CALLER_ENV`.
pub fn resolve_caller(payload: &Value) -> (String, String) {
    resolve_caller_with(payload, env_nonempty)
}

fn resolve_caller_with(
    payload: &Value,
    get_env: impl Fn(&str) -> Option<String>,
) -> (String, String) {
    if let Some(val) = get_env("DELEGATE_CALLER") {
        return (val, "DELEGATE_CALLER".to_string());
    }
    let sid = s(payload, "session_id").trim();
    if !sid.is_empty() {
        return (sid.to_string(), "CLAUDE_CODE_SESSION_ID".to_string());
    }
    let conv = s(payload, "conversationId").trim();
    if !conv.is_empty() {
        return (conv.to_string(), "ANTIGRAVITY_CONVERSATION_ID".to_string());
    }
    let order: [&str; 4] = if is_antigravity_agent_with(&get_env) {
        [
            "ANTIGRAVITY_CONVERSATION_ID",
            "CLAUDE_CODE_SESSION_ID",
            "CODEX_THREAD_ID",
            "PI_SESSION_ID",
        ]
    } else {
        [
            "CLAUDE_CODE_SESSION_ID",
            "CODEX_THREAD_ID",
            "PI_SESSION_ID",
            "ANTIGRAVITY_CONVERSATION_ID",
        ]
    };
    for key in order {
        if let Some(val) = get_env(key) {
            return (val, key.to_string());
        }
    }
    (String::new(), String::new())
}

fn norm_path_str(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let p = Path::new(trimmed);
    fs::canonicalize(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn git_toplevel_pure(path: &str) -> Option<PathBuf> {
    if path.is_empty() {
        return None;
    }
    let p = Path::new(path);
    if !p.is_dir() {
        return None;
    }
    let mut cur = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    // Step out if inside `.git/...`
    let mut prefix = PathBuf::new();
    let mut inside_git = false;
    for comp in cur.components() {
        if comp.as_os_str() == std::ffi::OsStr::new(".git") {
            inside_git = true;
            break;
        }
        prefix.push(comp);
    }
    if inside_git && !prefix.as_os_str().is_empty() {
        cur = prefix;
    }
    let mut walk: &Path = cur.as_path();
    loop {
        if walk.join(".git").exists() {
            return Some(walk.to_path_buf());
        }
        walk = walk.parent()?;
    }
}

fn repo_from_runs_dir(runs_dir: &Path) -> Option<PathBuf> {
    let comps: Vec<_> = runs_dir.components().collect();
    if comps.len() >= 4 {
        let last3: Vec<String> = comps[comps.len() - 3..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        if (last3[0] == ".local" && last3[1] == "run")
            && (last3[2] == "delegate" || last3[2] == "pi")
        {
            return runs_dir
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .map(Path::to_path_buf);
        }
    }
    None
}

fn config_dir() -> PathBuf {
    env_nonempty("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("delegate")
}

fn load_index_runs(state: &Path) -> HashMap<String, Value> {
    let doc = json(state.join("index.json"));
    let mut out = HashMap::new();
    if let Some(map) = doc["runs"].as_object() {
        for (k, v) in map {
            if v.is_object() {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    out
}

#[derive(Clone, Debug)]
struct DiscoveredRepo {
    repo_root: Option<PathBuf>,
    runs_dir: PathBuf,
    label: String,
    is_current: bool,
}

fn discover_repos(
    state: &Path,
    cache: &Path,
    cwd: &str,
    index_runs: &HashMap<String, Value>,
    current_only: bool,
) -> Vec<DiscoveredRepo> {
    let mut candidates: Vec<(Option<PathBuf>, PathBuf, bool)> = Vec::new();
    let current_repo = git_toplevel_pure(cwd);
    let env_runs = env_nonempty("DELEGATE_RUNS").or_else(|| env_nonempty("PI_DELEGATE_RUNS"));
    if let Some(runs_str) = env_runs {
        let runs_p = PathBuf::from(&runs_str);
        let repo_p = current_repo
            .clone()
            .or_else(|| repo_from_runs_dir(&runs_p))
            .or_else(|| (!cwd.is_empty()).then(|| PathBuf::from(cwd)));
        candidates.push((repo_p, runs_p, true));
    } else if let Some(ref cur) = current_repo {
        candidates.push((
            Some(cur.clone()),
            cur.join(".local").join("run").join("delegate"),
            true,
        ));
    } else if !cwd.is_empty() {
        let p = PathBuf::from(cwd);
        let rd = p.join(".local").join("run").join("delegate");
        if rd.is_dir() {
            candidates.push((Some(p), rd, true));
        }
    }

    if !current_only {
        // 1) Machine repo registry (`state/repos.json`).
        let repos_doc = json(state.join("repos.json"));
        if let Some(map) = repos_doc["repos"].as_object() {
            for entry in map.values() {
                let runs_str = s(entry, "runs");
                if runs_str.is_empty() {
                    continue;
                }
                let runs_p = PathBuf::from(runs_str);
                let repo_str = s(entry, "repo");
                let repo_p = if !repo_str.is_empty() {
                    Some(PathBuf::from(repo_str))
                } else {
                    repo_from_runs_dir(&runs_p)
                };
                candidates.push((repo_p, runs_p, false));
            }
        }
        // 2) Materialized index (`state/index.json`).
        for entry in index_runs.values() {
            let runs_str = s(entry, "runsRoot");
            if runs_str.is_empty() {
                continue;
            }
            let runs_p = PathBuf::from(runs_str);
            let repo_str = s(entry, "repo");
            let repo_p = if !repo_str.is_empty() {
                Some(PathBuf::from(repo_str))
            } else {
                repo_from_runs_dir(&runs_p)
            };
            candidates.push((repo_p, runs_p, false));
        }
        // 3) Live slot files under state_dir.
        if let Ok(rd) = fs::read_dir(state) {
            for ent in rd.flatten() {
                let p = ent.path();
                if p.extension().and_then(|e| e.to_str()) != Some("slot") {
                    continue;
                }
                let raw = read(&p);
                let run_dir = PathBuf::from(raw.trim());
                if !run_dir.join("meta.json").is_file() {
                    continue;
                }
                let Some(runs_p) = run_dir.parent().map(Path::to_path_buf) else {
                    continue;
                };
                let repo_p = repo_from_runs_dir(&runs_p).or_else(|| {
                    let meta = json(run_dir.join("meta.json"));
                    let src = s(&meta["worktree"], "source");
                    let chosen = if !src.is_empty() {
                        src
                    } else if !s(&meta, "top").is_empty() {
                        s(&meta, "top")
                    } else {
                        s(&meta, "workdir")
                    };
                    (!chosen.is_empty()).then(|| PathBuf::from(chosen))
                });
                candidates.push((repo_p, runs_p, false));
            }
        }
        // 4) Worktree directories under cache_dir.
        let wt_root = cache.join("worktrees");
        if let Ok(rd) = fs::read_dir(&wt_root) {
            for ent in rd.flatten() {
                let git_file = ent.path().join(".git");
                if !git_file.is_file() {
                    continue;
                }
                let line = read(&git_file);
                if let Some(rest) = line.trim().strip_prefix("gitdir:") {
                    let gitdir = rest.trim();
                    if let Some((repo_part, _)) = gitdir.split_once("/.git/worktrees/") {
                        let repo_p = PathBuf::from(repo_part);
                        let runs_p = repo_p.join(".local").join("run").join("delegate");
                        if runs_p.is_dir() {
                            candidates.push((Some(repo_p), runs_p, false));
                        }
                    }
                }
            }
        }
    }

    let mut dedup: HashMap<PathBuf, (Option<PathBuf>, PathBuf, bool)> = HashMap::new();
    let mut order: Vec<PathBuf> = Vec::new();
    for (repo_p, runs_p, is_cur) in candidates {
        if !runs_p.is_dir() {
            continue;
        }
        let key = fs::canonicalize(&runs_p).unwrap_or_else(|_| runs_p.clone());
        if let Some((prev_repo, _prev_runs, prev_cur)) = dedup.get_mut(&key) {
            if prev_repo.is_none() {
                *prev_repo = repo_p;
            }
            *prev_cur = *prev_cur || is_cur;
        } else {
            dedup.insert(key.clone(), (repo_p, runs_p, is_cur));
            order.push(key);
        }
    }

    let raw_items: Vec<(Option<PathBuf>, PathBuf, bool)> =
        order.into_iter().filter_map(|k| dedup.remove(&k)).collect();
    let mut base_counts: HashMap<String, usize> = HashMap::new();
    for (repo_p, runs_p, _) in &raw_items {
        let base = repo_base_name(repo_p.as_deref(), runs_p);
        *base_counts.entry(base).or_insert(0) += 1;
    }

    raw_items
        .into_iter()
        .map(|(repo_p, runs_p, is_current)| {
            let base = repo_base_name(repo_p.as_deref(), &runs_p);
            let label = if base_counts.get(&base).copied().unwrap_or(0) > 1 {
                if let Some(parent_name) = repo_p
                    .as_deref()
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    .and_then(|n| n.to_str())
                    .filter(|s| !s.is_empty())
                {
                    format!("{parent_name}/{base}")
                } else {
                    base
                }
            } else {
                base
            };
            DiscoveredRepo {
                repo_root: repo_p,
                runs_dir: runs_p,
                label,
                is_current,
            }
        })
        .collect()
}

fn repo_base_name(repo_p: Option<&Path>, runs_p: &Path) -> String {
    if let Some(rp) = repo_p {
        if let Some(n) = rp
            .file_name()
            .and_then(|x| x.to_str())
            .filter(|s| !s.is_empty())
        {
            return n.to_string();
        }
    }
    if let Some(n) = runs_p
        .parent()
        .and_then(Path::file_name)
        .and_then(|x| x.to_str())
        .filter(|s| !s.is_empty())
    {
        return n.to_string();
    }
    runs_p.to_string_lossy().into_owned()
}

fn strip_reply_prefixes(name: &str) -> String {
    let mut cur = name.trim();
    loop {
        if cur
            .get(..9)
            .is_some_and(|p| p.eq_ignore_ascii_case("reply to "))
        {
            cur = cur[9..].trim();
        } else {
            break;
        }
    }
    if cur.is_empty() {
        name.to_string()
    } else {
        cur.to_string()
    }
}

fn overlay_sign_status(rdir: &Path, obj: &mut Value) {
    let sign_doc = json(rdir.join("sign.json"));
    let signed_hash = s(&sign_doc, "hash").trim().to_string();
    if signed_hash.is_empty() {
        return;
    }
    let rev_file = json(rdir.join("review.json"));
    let rev = if rev_file.is_object() {
        &rev_file
    } else if obj["review"].is_object() {
        &obj["review"]
    } else {
        return;
    };
    let cur_hash = s(&rev["controllerReview"], "hash").trim().to_string();
    if cur_hash.is_empty() {
        return;
    }
    let valid = signed_hash == cur_hash;
    obj["sign"] = json!({
        "required": true,
        "signed": true,
        "valid": valid,
        "invalidated": !valid,
        "hash": cur_hash,
        "signedHash": signed_hash,
    });
}

fn overlay_index_meta(cand: &mut Value, idx_meta: &Value, rdir: Option<&Path>) {
    if !idx_meta.is_object() {
        return;
    }
    for key in ["resultReadAt", "appliedAt", "repliedAt"] {
        let val = s(idx_meta, key).trim();
        if !val.is_empty() && s(cand, key).trim().is_empty() {
            cand[key] = json!(val);
        }
    }
    let applied_marker = rdir.is_some_and(crate::runs::is_applied_marker);
    let gen_pending = rdir.is_some_and(|d| d.join(".generate-pending").exists());
    if applied_marker
        || (!gen_pending && (b(idx_meta, "appliedCleaned") || !s(cand, "appliedAt").trim().is_empty()))
    {
        cand["applied"] = json!(true);
    }
}

fn read_run_snapshot(rdir: &Path, now: i64) -> Option<Value> {
    let meta = json(rdir.join("meta.json"));
    if !meta.is_object() {
        return None;
    }
    let rid = {
        let r = s(&meta, "run");
        if !r.is_empty() {
            r.to_string()
        } else {
            rdir.file_name()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_string()
        }
    };
    if rid.is_empty() {
        return None;
    }
    let name = {
        let n = s(&meta, "name");
        if !n.is_empty() {
            n.to_string()
        } else {
            rid.clone()
        }
    };
    let mode = s(&meta, "mode").to_string();
    let parent = s(&meta, "parent").to_string();
    let summary = json(rdir.join("summary.json"));
    let exit_file = rdir.join("exit_code");
    let st = if exit_file.is_file() {
        let ss = s(&summary, "state");
        if !ss.is_empty() {
            ss.to_string()
        } else {
            let code = read(&exit_file).trim().to_string();
            if code == "0" {
                if mode == "write" {
                    "delivered".to_string()
                } else {
                    "answered".to_string()
                }
            } else {
                "failed".to_string()
            }
        }
    } else if rdir.join(".scouting").exists() {
        "scouting".to_string()
    } else if rdir.join(".waiting").exists() {
        "waiting".to_string()
    } else {
        let mut alive = false;
        for pf in [rdir.join("pid"), rdir.join("agent.pid")] {
            if pf.is_file() {
                if let Ok(pid) = read(&pf).trim().parse::<i32>() {
                    if pid_alive(pid) {
                        alive = true;
                        break;
                    }
                }
            }
        }
        if alive {
            "running".to_string()
        } else {
            let started_ep = meta["startedEpoch"].as_f64().unwrap_or(0.0);
            if started_ep > 0.0 && (now as f64 - started_ep) < 10.0 {
                "starting".to_string()
            } else {
                "crashed".to_string()
            }
        }
    };

    let is_applied = crate::runs::is_applied_marker(rdir)
        || ((rdir.join(".applied-cleaned").exists()
            || !s(&meta, "appliedAt").trim().is_empty())
            && !rdir.join(".generate-pending").exists());

    let wt_path = s(&meta["worktree"], "path").to_string();
    let pm = json(rdir.join("premerge.json"));
    let rev_file = json(rdir.join("review.json"));
    let rev = if rev_file.is_object() {
        rev_file
    } else if summary["review"].is_object() {
        summary["review"].clone()
    } else {
        Value::Null
    };

    let mut obj = json!({
        "run": rid,
        "name": name,
        "chainName": strip_reply_prefixes(&name),
        "state": st,
        "mode": mode,
        "parent": parent,
        "caller": s(&meta, "caller").trim(),
        "applied": is_applied,
        "cleaned": false,
        "question": b(&summary, "question"),
        "worktree": wt_path,
        "dir": rdir.to_string_lossy(),
    });
    for mkey in [
        "resultReadAt",
        "appliedAt",
        "repliedAt",
        "startedAt",
        "workId",
        "netBase",
        "top",
    ] {
        let mval = s(&meta, mkey).trim();
        if !mval.is_empty() {
            obj[mkey] = json!(mval);
        }
    }
    if let Some(ep) = meta["startedEpoch"].as_f64() {
        obj["startedEpoch"] = json!(ep);
    }
    let fin_at = s(&summary, "finishedAt").trim();
    if !fin_at.is_empty() {
        obj["finishedAt"] = json!(fin_at);
    }
    if summary["pendingChanges"].is_object() {
        obj["pendingChanges"] = summary["pendingChanges"].clone();
    }
    if summary["changes"].is_object() {
        obj["changes"] = summary["changes"].clone();
    }
    if summary["files"].is_array() {
        obj["files"] = summary["files"].clone();
    }
    if b(&meta, "isMerge") {
        obj["isMerge"] = json!(true);
        if meta["merge"].is_object() {
            obj["merge"] = meta["merge"].clone();
        }
    }
    if pm.is_object() {
        obj["premerge"] = pm;
    }
    if rev.is_object() {
        obj["review"] = rev;
    }
    overlay_sign_status(rdir, &mut obj);

    if st == "delivered" && mode == "write" && !is_applied {
        let rev_verdict = s(&obj["review"], "verdict");
        let next = if rev_verdict == crate::review::REVIEW_VERDICT_ENV_ERROR
            || b(&obj["review"], "envError")
        {
            format!("card {rid}")
        } else if rev_verdict == "NEEDS_CONTROLLER" {
            format!("review {rid}")
        } else {
            format!("apply {rid}")
        };
        obj["next"] = json!(next);
    }
    Some(obj)
}

fn fetch_status(runs_dir: &Path, index_runs: &HashMap<String, Value>, now: i64) -> Vec<Value> {
    let Ok(rd) = fs::read_dir(runs_dir) else {
        return Vec::new();
    };
    let mut actual_dirs = Vec::new();
    for ent in rd.flatten() {
        let p = ent.path();
        if p.is_dir() && p.join("meta.json").is_file() {
            actual_dirs.push(fs::canonicalize(&p).unwrap_or(p));
        }
    }
    if actual_dirs.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(actual_dirs.len());
    for rdir in actual_dirs {
        let key = rdir.to_string_lossy().into_owned();
        let mut st_obj: Option<Value> = None;
        if let Some(entry) = index_runs.get(&key) {
            if entry["status"].is_object() {
                let mut cand = entry["status"].clone();
                let st = s(&cand, "state").to_string();
                let mut stale = false;
                if matches!(st.as_str(), "running" | "starting" | "waiting" | "queued" | "scouting") {
                    if rdir.join("exit_code").is_file() {
                        stale = true;
                    } else {
                        let pf = rdir.join("pid");
                        if pf.is_file() {
                            match read(&pf).trim().parse::<i32>() {
                                Ok(pid) if pid_alive(pid) => {}
                                _ => stale = true,
                            }
                        }
                    }
                } else {
                    let has_applied_marker = crate::runs::is_applied_marker(&rdir)
                        || (rdir.join(".applied-cleaned").exists()
                            && !rdir.join(".generate-pending").exists());
                    if has_applied_marker && !b(&cand, "applied") {
                        stale = true;
                    }
                }
                if !stale {
                    if s(&cand, "dir").is_empty() {
                        cand["dir"] = json!(key);
                    }
                    overlay_index_meta(&mut cand, &entry["meta"], Some(&rdir));
                    let pm = json(rdir.join("premerge.json"));
                    if pm.is_object() {
                        cand["premerge"] = pm;
                    }
                    overlay_sign_status(&rdir, &mut cand);
                    if matches!(st.as_str(), "running" | "starting" | "waiting" | "queued" | "scouting")
                        && !b(&cand, "isMerge")
                    {
                        let meta = json(rdir.join("meta.json"));
                        if b(&meta, "isMerge") {
                            cand["isMerge"] = json!(true);
                            if meta["merge"].is_object() {
                                cand["merge"] = meta["merge"].clone();
                            }
                        }
                    }
                    st_obj = Some(cand);
                }
            }
        }
        let obj = st_obj.or_else(|| read_run_snapshot(&rdir, now));
        if let Some(v) = obj {
            if !s(&v, "run").is_empty() {
                out.push(v);
            }
        }
    }
    out
}

fn split_cmdline(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for ch in line.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && !in_single {
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if ch.is_whitespace() && !in_single && !in_double {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn list_processes() -> Vec<(Vec<String>, Option<PathBuf>)> {
    if let Ok(raw) = env::var("DELEGATE_STOP_HOOK_PROCS") {
        return raw
            .lines()
            .map(str::trim)
            .filter(|ln| !ln.is_empty())
            .map(|ln| (split_cmdline(ln), None))
            .collect();
    }
    let proc_dir = Path::new("/proc");
    if !proc_dir.is_dir() {
        return Vec::new();
    }
    let my_pid = std::process::id();
    let mut procs = Vec::new();
    let Ok(rd) = fs::read_dir(proc_dir) else {
        return Vec::new();
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };
        if pid == my_pid {
            continue;
        }
        let p = ent.path();
        if let Ok(stat_txt) = fs::read_to_string(p.join("stat")) {
            if let Some(rparen) = stat_txt.rfind(')') {
                if stat_txt.as_bytes().get(rparen + 2) == Some(&b'Z') {
                    continue;
                }
            }
        } else {
            continue;
        }
        let Ok(raw) = fs::read(p.join("cmdline")) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let tokens: Vec<String> = raw
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        if tokens.is_empty() {
            continue;
        }
        let pcwd = fs::read_link(p.join("cwd")).ok();
        procs.push((tokens, pcwd));
    }
    procs
}

fn cwd_in_repo(proc_cwd: Option<&Path>, repo_root: Option<&Path>) -> bool {
    let Some(pcwd) = proc_cwd else {
        return true;
    };
    let Some(rroot) = repo_root else {
        return false;
    };
    let p = fs::canonicalize(pcwd).unwrap_or_else(|_| pcwd.to_path_buf());
    let r = fs::canonicalize(rroot).unwrap_or_else(|_| rroot.to_path_buf());
    p == r || p.starts_with(&r)
}

fn match_delegate_subcmd(tokens: &[String], subcmd: &str) -> Option<Vec<String>> {
    if tokens.is_empty() {
        return None;
    }
    let mut idx = 0usize;
    let first_base = Path::new(&tokens[0])
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&tokens[0]);
    if first_base == "env" {
        idx = 1;
        while idx < tokens.len() {
            let tok = &tokens[idx];
            if tok == "--" {
                idx += 1;
                break;
            }
            if matches!(tok.as_str(), "-u" | "-C" | "-S") {
                idx += 2;
                continue;
            }
            if tok.starts_with('-') || tok.contains('=') {
                idx += 1;
                continue;
            }
            break;
        }
    }
    let cmd = tokens.get(idx)?;
    let base = Path::new(cmd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(cmd);
    if base != "delegate" {
        return None;
    }
    idx += 1;
    while tokens.get(idx).is_some_and(|t| t.starts_with('-')) {
        idx += 1;
    }
    if tokens.get(idx).map(String::as_str) == Some(subcmd) {
        Some(tokens[idx + 1..].to_vec())
    } else {
        None
    }
}

fn get_run_caller(obj: &mut Value) -> String {
    let raw = s(obj, "caller").trim();
    if !raw.is_empty() {
        return raw.to_string();
    }
    let rdir = s(obj, "dir").to_string();
    if !rdir.is_empty() {
        let meta = json(Path::new(&rdir).join("meta.json"));
        let val = s(&meta, "caller").trim().to_string();
        if !val.is_empty() {
            obj["caller"] = json!(val);
            return val;
        }
    }
    String::new()
}

fn get_run_caller_readonly(obj: &Value) -> String {
    let raw = s(obj, "caller").trim();
    if !raw.is_empty() {
        return raw.to_string();
    }
    let rdir = s(obj, "dir");
    if !rdir.is_empty() {
        let meta = json(Path::new(rdir).join("meta.json"));
        let val = s(&meta, "caller").trim();
        if !val.is_empty() {
            return val.to_string();
        }
    }
    String::new()
}

fn run_belongs_to_scope(obj: &Value, is_current: bool, caller_id: &str) -> bool {
    if is_current {
        return true;
    }
    let run_caller = get_run_caller_readonly(obj);
    if !caller_id.is_empty() {
        return run_caller == caller_id;
    }
    run_caller.is_empty()
}

fn touch_caller_session_for(
    state: &Path,
    caller_id: &str,
    caller_source: &str,
    cwd: &str,
    now: i64,
    write_disk: bool,
) -> (Option<(u32, u64)>, String, String) {
    let source_opt = (!caller_source.is_empty()).then_some(caller_source);
    let harness = find_session_harness_pid(source_opt);
    let mut harness_pt = harness.map(|(p, t, _)| (p, t));
    let mut cur_tty = current_terminal_id(harness.map_or(0, |(_, _, tty)| tty)).unwrap_or_default();
    let mut cur_cwd = norm_path_str(cwd);
    if caller_id.is_empty() || !write_disk {
        return (harness_pt, cur_tty, cur_cwd);
    }
    let _ = fs::create_dir_all(state);
    let path = state.join("sessions.json");
    let mut doc = json(&path);
    if !doc.is_object() {
        doc = json!({});
    }
    let cutoff = now - 14 * 86_400;
    if let Some(map) = doc.as_object_mut() {
        let prev = map.get(caller_id).cloned().unwrap_or(Value::Null);
        let mut rec = json!({
            "lastSeenAt": format_epoch(now, false, "%Y-%m-%dT%H:%M:%SZ"),
        });
        if let Some((pid, ticks, _)) = harness {
            rec["pid"] = json!(pid);
            rec["startTicks"] = json!(ticks);
        } else if let Some(prev_pid) = prev["pid"]
            .as_u64()
            .filter(|&p| p > 0 && p <= u32::MAX as u64)
        {
            let prev_ticks = prev["startTicks"].as_u64();
            if pid_alive_with_ticks(prev_pid as u32, prev_ticks) {
                rec["pid"] = json!(prev_pid);
                if let Some(t) = prev_ticks {
                    rec["startTicks"] = json!(t);
                    if harness_pt.is_none() {
                        harness_pt = Some((prev_pid as u32, t));
                    }
                }
            }
        }
        if !cur_tty.is_empty() {
            rec["tty"] = json!(cur_tty);
        } else {
            let prev_tty = s(&prev, "tty").trim();
            if !prev_tty.is_empty() {
                rec["tty"] = json!(prev_tty);
                cur_tty = prev_tty.to_string();
            }
        }
        if !cur_cwd.is_empty() {
            rec["cwd"] = json!(cur_cwd);
        } else {
            let prev_cwd = s(&prev, "cwd").trim();
            if !prev_cwd.is_empty() {
                let nc = norm_path_str(prev_cwd);
                rec["cwd"] = json!(nc);
                cur_cwd = nc;
            }
        }
        map.insert(caller_id.to_string(), rec);
        map.retain(|k, v| {
            k == caller_id || iso_epoch(s(v, "lastSeenAt")).is_none_or(|ts| ts >= cutoff)
        });
    }
    let _ = write_json(&path, &doc);
    (harness_pt, cur_tty, cur_cwd)
}

fn find_alive_callers_in_proc(target_callers: &HashSet<String>) -> HashSet<String> {
    let mut found = HashSet::new();
    if target_callers.is_empty() {
        return found;
    }
    let proc_dir = Path::new("/proc");
    if !proc_dir.is_dir() {
        return found;
    }
    let my_pid = std::process::id();
    let my_uid = unsafe { libc::getuid() };
    let env_prefixes: Vec<Vec<u8>> = CALLER_ENV
        .iter()
        .map(|k| format!("{k}=").into_bytes())
        .collect();
    let target_bytes: HashMap<Vec<u8>, String> = target_callers
        .iter()
        .filter(|c| !c.is_empty())
        .map(|c| (c.as_bytes().to_vec(), c.clone()))
        .collect();
    if target_bytes.is_empty() {
        return found;
    }
    let Ok(rd) = fs::read_dir(proc_dir) else {
        return found;
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };
        if pid == my_pid {
            continue;
        }
        let p = ent.path();
        let Ok(md) = fs::metadata(&p) else {
            continue;
        };
        if md.uid() != my_uid {
            continue;
        }
        if let Ok(stat_txt) = fs::read_to_string(p.join("stat")) {
            if let Some(rparen) = stat_txt.rfind(')') {
                if stat_txt.as_bytes().get(rparen + 2) == Some(&b'Z') {
                    continue;
                }
            }
        } else {
            continue;
        }
        if let Ok(env_raw) = fs::read(p.join("environ")) {
            for item in env_raw.split(|&b| b == 0) {
                for pfx in &env_prefixes {
                    if let Some(rest) = item.strip_prefix(pfx.as_slice()) {
                        let trimmed = trim_ascii_bytes(rest);
                        if let Some(matched) = target_bytes.get(trimmed) {
                            found.insert(matched.clone());
                            if found.len() == target_callers.len() {
                                return found;
                            }
                        }
                    }
                }
            }
        }
        if let Ok(cmd_raw) = fs::read(p.join("cmdline")) {
            let toks: Vec<&[u8]> = cmd_raw
                .split(|&b| b == 0)
                .filter(|t| !t.is_empty())
                .collect();
            for i in 0..toks.len() {
                if (toks[i] == b"--session-id" || toks[i] == b"--resume") && i + 1 < toks.len() {
                    let trimmed = trim_ascii_bytes(toks[i + 1]);
                    if let Some(matched) = target_bytes.get(trimmed) {
                        found.insert(matched.clone());
                        if found.len() == target_callers.len() {
                            return found;
                        }
                    }
                }
            }
        }
    }
    found
}

fn trim_ascii_bytes(b: &[u8]) -> &[u8] {
    let start = b
        .iter()
        .position(|c| !c.is_ascii_whitespace())
        .unwrap_or(b.len());
    let end = b
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &b[start..end]
}

fn get_run_unclaimed_ts(obj: &Value) -> Option<i64> {
    let st = s(obj, "state");
    let is_active = matches!(st, "running" | "starting" | "waiting" | "queued" | "scouting");
    if !is_active {
        if let Some(fin) = iso_epoch(s(obj, "finishedAt")) {
            return Some(fin);
        }
        let rdir = s(obj, "dir");
        if !rdir.is_empty() {
            let summary = json(Path::new(rdir).join("summary.json"));
            if let Some(fin) = iso_epoch(s(&summary, "finishedAt")) {
                return Some(fin);
            }
        }
    }
    if let Some(st_at) = iso_epoch(s(obj, "startedAt")) {
        return Some(st_at);
    }
    if let Some(ep) = obj["startedEpoch"].as_f64().filter(|&v| v > 0.0) {
        return Some(ep as i64);
    }
    let rdir = s(obj, "dir");
    if !rdir.is_empty() {
        let meta = json(Path::new(rdir).join("meta.json"));
        if let Some(st_at) = iso_epoch(s(&meta, "startedAt")) {
            return Some(st_at);
        }
        if let Some(ep) = meta["startedEpoch"].as_f64().filter(|&v| v > 0.0) {
            return Some(ep as i64);
        }
    }
    None
}

struct CallerLivenessChecker {
    cursors_doc: Value,
    sessions: serde_json::Map<String, Value>,
    now: i64,
    orphan_secs: f64,
    cur_caller: String,
    cur_harness: Option<(u32, u64)>,
    cur_tty: String,
    cur_cwd: String,
    alive_cache: HashMap<String, bool>,
    proc_checked: HashSet<String>,
    proc_alive: HashSet<String>,
}

impl CallerLivenessChecker {
    #[allow(clippy::too_many_arguments)]
    fn new(
        state: &Path,
        cursors_doc: Value,
        now: i64,
        orphan_secs: f64,
        cur_caller: String,
        cur_harness: Option<(u32, u64)>,
        cur_tty: String,
        cur_cwd: String,
    ) -> Self {
        let sessions_val = json(state.join("sessions.json"));
        let sessions = sessions_val.as_object().cloned().unwrap_or_default();
        Self {
            cursors_doc,
            sessions,
            now,
            orphan_secs,
            cur_caller,
            cur_harness,
            cur_tty,
            cur_cwd: norm_path_str(&cur_cwd),
            alive_cache: HashMap::new(),
            proc_checked: HashSet::new(),
            proc_alive: HashSet::new(),
        }
    }

    fn check_proc(&mut self, caller: &str) -> bool {
        if !self.proc_checked.contains(caller) {
            let mut set = HashSet::new();
            set.insert(caller.to_string());
            let found = find_alive_callers_in_proc(&set);
            self.proc_checked.insert(caller.to_string());
            self.proc_alive.extend(found);
        }
        self.proc_alive.contains(caller)
    }

    fn is_superseded_same_harness(&self, orig_caller: &str, rec: &Value) -> bool {
        let Some(pid) = rec["pid"]
            .as_u64()
            .filter(|&p| p > 0 && p <= u32::MAX as u64)
        else {
            return false;
        };
        let pid_u32 = pid as u32;
        let ticks = rec["startTicks"].as_u64();
        if let Some((hp, ht)) = self.cur_harness {
            if !self.cur_caller.is_empty()
                && self.cur_caller != orig_caller
                && hp == pid_u32
                && ticks.is_none_or(|t| t == ht)
            {
                return true;
            }
        }
        let orig_ts = iso_epoch(s(rec, "lastSeenAt"));
        for (other_id, other_rec) in &self.sessions {
            if other_id == orig_caller || !other_rec.is_object() {
                continue;
            }
            if other_rec["pid"].as_u64() != Some(pid) {
                continue;
            }
            let other_ticks = other_rec["startTicks"].as_u64();
            if let (Some(t1), Some(t2)) = (ticks, other_ticks) {
                if t1 != t2 {
                    continue;
                }
            }
            if other_id == &self.cur_caller {
                return true;
            }
            if let Some(other_ts) = iso_epoch(s(other_rec, "lastSeenAt")) {
                if orig_ts.is_none_or(|ots| other_ts > ots) {
                    return true;
                }
            }
        }
        false
    }

    fn is_alive(&mut self, orig_caller: &str) -> bool {
        if orig_caller.is_empty() {
            return false;
        }
        if let Some(&cached) = self.alive_cache.get(orig_caller) {
            return cached;
        }
        let mut alive = false;
        let mut has_recorded_pid = false;
        let mut superseded = false;
        let rec_opt = self.sessions.get(orig_caller).cloned();
        if let Some(ref rec) = rec_opt {
            if let Some(pid) = rec["pid"]
                .as_u64()
                .filter(|&p| p > 0 && p <= u32::MAX as u64)
            {
                has_recorded_pid = true;
                if self.is_superseded_same_harness(orig_caller, rec) {
                    superseded = true;
                } else if pid_alive_with_ticks(pid as u32, rec["startTicks"].as_u64()) {
                    alive = true;
                }
            }
        }
        if !alive && !superseded && self.check_proc(orig_caller) {
            alive = true;
        }
        if !alive && !has_recorded_pid {
            let mut last_ts: Option<i64> =
                rec_opt.as_ref().and_then(|r| iso_epoch(s(r, "lastSeenAt")));
            if let Some(cts) = self.cursors_doc["callers"]
                .get(orig_caller)
                .and_then(|c| iso_epoch(s(c, "updatedAt")))
            {
                if last_ts.is_none_or(|lt| cts > lt) {
                    last_ts = Some(cts);
                }
            }
            if let Some(lt) = last_ts {
                if ((self.now - lt) as f64) < self.orphan_secs {
                    alive = true;
                }
            }
        }
        self.alive_cache.insert(orig_caller.to_string(), alive);
        alive
    }

    fn same_terminal(&self, orig_caller: &str) -> bool {
        let Some(rec) = self.sessions.get(orig_caller) else {
            return false;
        };
        if let (Some((hp, ht)), Some(pid)) = (
            self.cur_harness,
            rec["pid"]
                .as_u64()
                .filter(|&p| p > 0 && p <= u32::MAX as u64),
        ) {
            let ticks = rec["startTicks"].as_u64();
            if hp == pid as u32 && ticks.is_none_or(|t| t == ht) {
                return true;
            }
        }
        if !self.cur_tty.is_empty() {
            let orig_tty = s(rec, "tty").trim();
            if !orig_tty.is_empty() && orig_tty == self.cur_tty {
                return true;
            }
        }
        false
    }

    fn same_cwd(&self, orig_caller: &str) -> bool {
        if self.cur_cwd.is_empty() {
            return false;
        }
        let Some(rec) = self.sessions.get(orig_caller) else {
            return false;
        };
        let orig_cwd = norm_path_str(s(rec, "cwd"));
        !orig_cwd.is_empty() && orig_cwd == self.cur_cwd
    }

    fn same_terminal_or_cwd(&self, orig_caller: &str) -> bool {
        self.same_terminal(orig_caller) || self.same_cwd(orig_caller)
    }

    fn is_adoptable(&mut self, orig_caller: &str) -> bool {
        if self.cur_caller.is_empty() || orig_caller.is_empty() || orig_caller == self.cur_caller {
            return false;
        }
        if self.is_alive(orig_caller) {
            return false;
        }
        self.same_terminal_or_cwd(orig_caller)
    }

    fn can_takeover(&mut self, orig_caller: &str, unclaimed_since_ts: Option<i64>) -> bool {
        if orig_caller.is_empty() {
            return true;
        }
        if self.is_alive(orig_caller) {
            return false;
        }
        match unclaimed_since_ts {
            None => true,
            Some(ts) => ((self.now - ts) as f64) >= self.orphan_secs,
        }
    }
}

fn read_locked_inodes() -> Option<HashSet<(u32, u32, u64)>> {
    let txt = fs::read_to_string("/proc/locks").ok()?;
    let mut inodes = HashSet::new();
    for line in txt.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 || fields[1] == "->" {
            continue;
        }
        let parts: Vec<&str> = fields[5].split(':').collect();
        if parts.len() != 3 {
            continue;
        }
        let (Ok(maj), Ok(min), Ok(ino)) = (
            u32::from_str_radix(parts[0], 16),
            u32::from_str_radix(parts[1], 16),
            parts[2].parse::<u64>(),
        ) else {
            continue;
        };
        inodes.insert((maj, min, ino));
    }
    Some(inodes)
}

fn has_active_waiter_lock(
    run_dir: Option<&Path>,
    locked_inodes: Option<&HashSet<(u32, u32, u64)>>,
) -> bool {
    let Some(rdir) = run_dir else {
        return false;
    };
    for name in ["waiter.lock", "waiter-attach.lock"] {
        let lock_path = rdir.join(name);
        if !lock_path.is_file() {
            continue;
        }
        let Ok(md) = fs::metadata(&lock_path) else {
            continue;
        };
        if let Some(inodes) = locked_inodes {
            let dev = md.dev();
            let maj = libc::major(dev) as u32;
            let min = libc::minor(dev) as u32;
            if inodes.contains(&(maj, min, md.ino())) {
                return true;
            }
            continue;
        }
        if read(&lock_path)
            .lines()
            .filter_map(|line| line.trim().parse::<i32>().ok())
            .any(|pid| pid > 0 && pid_alive(pid))
        {
            return true;
        }
    }
    false
}

fn active_waiters(procs: &[(Vec<String>, Option<PathBuf>)]) -> Vec<(Vec<String>, Option<PathBuf>)> {
    if let Ok(raw) = env::var("DELEGATE_STOP_HOOK_WAITERS") {
        let mut out = Vec::new();
        for ln in raw.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if let Some(args) = match_delegate_subcmd(&split_cmdline(ln), "wait") {
                out.push((args, None));
            }
        }
        return out;
    }
    let mut out = Vec::new();
    for (argv, pcwd) in procs {
        if let Some(args) = match_delegate_subcmd(argv, "wait") {
            out.push((args, pcwd.clone()));
        }
    }
    out
}

fn extract_positional_targets(args: &[String], value_flags: &[&str]) -> Vec<String> {
    let mut pos = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let tok = &args[i];
        if tok == "--" {
            pos.extend(args[i + 1..].iter().cloned());
            break;
        }
        if value_flags.contains(&tok.as_str()) {
            i += 2;
            continue;
        }
        if tok.starts_with('-') {
            i += 1;
            continue;
        }
        pos.push(tok.clone());
        i += 1;
    }
    pos
}

fn run_matches_target(rid: &str, obj: &Value, target: &str) -> bool {
    if target.is_empty() {
        return false;
    }
    let name = s(obj, "name");
    let chain = s(obj, "chainName");
    if target == rid || target == name || target == chain {
        return true;
    }
    rid.ends_with(&format!("-{target}")) || (target.len() >= 4 && rid.contains(target))
}

fn waiter_covers_run(
    run_dir: Option<&Path>,
    rid: &str,
    obj: &Value,
    repo_root: Option<&Path>,
    is_current: bool,
    waiters: &[(Vec<String>, Option<PathBuf>)],
    locked_inodes: Option<&HashSet<(u32, u32, u64)>>,
) -> bool {
    if has_active_waiter_lock(run_dir, locked_inodes) {
        return true;
    }
    for (args, pcwd) in waiters {
        if args.iter().any(|a| a == "--machine") {
            return true;
        }
        let targets = extract_positional_targets(args, &["--max", "--since"]);
        if args.iter().any(|a| a == "--since") && targets.is_empty() {
            return true;
        }
        if targets.is_empty() {
            // Untargeted `delegate wait` covers this session's runs across all repos by default;
            // only `--current-repo` restricts coverage to the waiter's own repo.
            if !args.iter().any(|a| a == "--current-repo") {
                return true;
            }
            if pcwd.is_some() {
                if cwd_in_repo(pcwd.as_deref(), repo_root) {
                    return true;
                }
            } else if is_current {
                return true;
            }
        } else if targets.iter().any(|t| run_matches_target(rid, obj, t)) {
            return true;
        }
    }
    false
}

fn active_lane_state(
    state: &Path,
    procs: &[(Vec<String>, Option<PathBuf>)],
) -> (Vec<Option<PathBuf>>, HashSet<String>) {
    let mut gate_cwds: Vec<Option<PathBuf>> = Vec::new();
    for (argv, pcwd) in procs {
        if match_delegate_subcmd(argv, "lane").is_some() {
            gate_cwds.push(pcwd.clone());
        }
    }
    let mut lane_runs = HashSet::new();
    for sub in ["lane", "lane-remote", "lane-remote-light"] {
        let ldir = state.join(sub);
        let Ok(rd) = fs::read_dir(&ldir) else {
            continue;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("ticket") {
                continue;
            }
            let doc = json(&p);
            if !b(&doc, "running") {
                continue;
            }
            let pid = doc["pid"].as_i64().unwrap_or(0) as i32;
            if pid > 0 && !pid_alive(pid) {
                continue;
            }
            let run_val = s(&doc, "run").trim().to_string();
            let label = s(&doc, "label").trim();
            if run_val.is_empty()
                || label.starts_with("premerge-")
                || label.starts_with("merge-")
                || label.starts_with("apply-")
            {
                let tcwd = (pid > 0)
                    .then(|| fs::read_link(format!("/proc/{pid}/cwd")).ok())
                    .flatten();
                gate_cwds.push(tcwd);
            }
            if !run_val.is_empty() {
                if let Some(fname) = Path::new(&run_val).file_name().and_then(|x| x.to_str()) {
                    lane_runs.insert(fname.to_string());
                }
                lane_runs.insert(run_val);
            }
        }
    }
    (gate_cwds, lane_runs)
}

fn is_premerge_running(obj: &Value) -> bool {
    let pm = &obj["premerge"];
    if !pm.is_object() || s(pm, "status") != "running" {
        return false;
    }
    if let Some(wpid) = pm["workerPid"].as_i64().filter(|&p| p > 0) {
        return pid_alive(wpid as i32);
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn is_run_being_handled(
    rid: &str,
    obj: &Value,
    repo_root: Option<&Path>,
    applying_procs: &[(Vec<String>, Option<PathBuf>)],
    merging_procs: &[(Vec<String>, Option<PathBuf>)],
    active_merge_sources: &HashSet<String>,
    gate_cwds: &[Option<PathBuf>],
    lane_runs: &HashSet<String>,
) -> bool {
    if gate_cwds
        .iter()
        .any(|gcwd| cwd_in_repo(gcwd.as_deref(), repo_root))
    {
        return true;
    }
    if lane_runs.contains(rid) || lane_runs.iter().any(|lr| lr.contains(rid)) {
        return true;
    }
    if active_merge_sources.contains(rid) {
        return true;
    }
    if is_premerge_running(obj) {
        return true;
    }
    for (args, pcwd) in applying_procs {
        let targets = extract_positional_targets(args, &["--closes", "--message", "-m", "--work"]);
        if targets.is_empty() {
            if cwd_in_repo(pcwd.as_deref(), repo_root) {
                return true;
            }
        } else if targets.iter().any(|t| {
            ((t == "last" || t == "--all") && cwd_in_repo(pcwd.as_deref(), repo_root))
                || run_matches_target(rid, obj, t)
        }) {
            return true;
        }
    }
    for (args, pcwd) in merging_procs {
        if args.iter().any(|a| a == "--ready") {
            if cwd_in_repo(pcwd.as_deref(), repo_root) {
                return true;
            }
            continue;
        }
        let targets = extract_positional_targets(args, &["--name", "--base"]);
        if targets.is_empty() {
            if cwd_in_repo(pcwd.as_deref(), repo_root) {
                return true;
            }
        } else if targets.iter().any(|t| run_matches_target(rid, obj, t)) {
            return true;
        }
    }
    false
}

fn work_deferred_runs(work_items: &[WorkItem], now: i64) -> (HashSet<String>, HashSet<String>) {
    let mut fresh = HashSet::new();
    let mut expired = HashSet::new();
    for item in work_items {
        if item.state == "done" {
            continue;
        }
        let until_raw = item.defer_until.trim();
        let reason_raw = item.defer_reason.trim();
        if !item.runs.is_empty() {
            if item.state == "deferred" {
                if iso_epoch(until_raw).is_some_and(|ts| ts > now) {
                    for r in &item.runs {
                        fresh.insert(r.clone());
                    }
                } else {
                    for r in &item.runs {
                        expired.insert(r.clone());
                    }
                }
            } else if !reason_raw.is_empty() && until_raw.is_empty() {
                for r in &item.runs {
                    expired.insert(r.clone());
                }
            }
        }
        for (rid, rdef) in &item.run_defers {
            if rid.is_empty() {
                continue;
            }
            if iso_epoch(rdef.until.trim()).is_some_and(|ts| ts > now) {
                fresh.insert(rid.clone());
            } else {
                expired.insert(rid.clone());
            }
        }
    }
    (fresh, expired)
}

fn resolve_source_repo_for_run(obj: &Value, repo_root: Option<&Path>) -> Option<PathBuf> {
    if let Some(rr) = repo_root {
        if rr.is_dir() {
            return Some(rr.to_path_buf());
        }
    }
    let pms = s(&obj["premerge"], "source").trim();
    if !pms.is_empty() && Path::new(pms).is_dir() {
        return Some(PathBuf::from(pms));
    }
    let rdir_raw = s(obj, "dir").trim();
    if !rdir_raw.is_empty() {
        let meta = json(Path::new(rdir_raw).join("meta.json"));
        let src = s(&meta["worktree"], "source").trim();
        if !src.is_empty() && Path::new(src).is_dir() {
            return Some(PathBuf::from(src));
        }
    }
    let top = s(obj, "top").trim();
    if !top.is_empty() && Path::new(top).is_dir() {
        return Some(PathBuf::from(top));
    }
    None
}

fn git_batch_blobs(source_root: &Path, specs: &[String]) -> Option<Vec<(String, Option<Vec<u8>>)>> {
    if specs.is_empty() {
        return Some(Vec::new());
    }
    let mut cmd = git_command();
    cmd.arg("-C")
        .arg(source_root)
        .args(["cat-file", "--batch"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let stdin_data = format!("{}\n", specs.join("\n"));
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(stdin_data.as_bytes()).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let buf = &output.stdout;
    let mut pos = 0usize;
    let n = buf.len();
    let mut results = Vec::with_capacity(specs.len());
    for _ in specs {
        let nl = buf[pos..].iter().position(|&b| b == b'\n')? + pos;
        let header = &buf[pos..nl];
        pos = nl + 1;
        if header.ends_with(b" missing") {
            results.push(("missing".to_string(), None));
            continue;
        }
        let parts: Vec<&[u8]> = header.split(|&b| b == b' ').collect();
        if parts.len() != 3 {
            return None;
        }
        let obj_type = String::from_utf8_lossy(parts[1]).into_owned();
        let size: usize = std::str::from_utf8(parts[2]).ok()?.parse().ok()?;
        if pos + size > n {
            return None;
        }
        let body = buf[pos..pos + size].to_vec();
        pos += size;
        if pos < n && buf[pos] == b'\n' {
            pos += 1;
        }
        results.push((obj_type, Some(body)));
    }
    Some(results)
}

/// Return true if all of a write run's net changes already appear verbatim in source HEAD.
/// Fast path checks `read_git_head(source_root) == netBase` via pure filesystem reads (zero git calls).
fn is_run_verbatim_in_source_head(
    obj: &Value,
    repo_root: Option<&Path>,
    cache: &mut HashMap<String, bool>,
) -> bool {
    let rid = s(obj, "run").trim().to_string();
    if !rid.is_empty() {
        if let Some(&cached) = cache.get(&rid) {
            return cached;
        }
    }
    let mut store = |val: bool| -> bool {
        if !rid.is_empty() {
            cache.insert(rid.clone(), val);
        }
        val
    };
    if b(obj, "applied") {
        return store(true);
    }
    if s(obj, "mode") != "write" {
        return store(false);
    }
    let rdir_raw = s(obj, "dir").trim();
    if rdir_raw.is_empty() {
        return store(false);
    }
    let rdir = Path::new(rdir_raw);
    let Some(source_root) = resolve_source_repo_for_run(obj, repo_root) else {
        return store(false);
    };

    // Zero-subprocess fast path: if source HEAD has not advanced past `netBase` and no
    // `.sync-base` rebased the worktree, HEAD cannot contain non-empty diffs against itself.
    let net_base = {
        let nb = s(obj, "netBase").trim();
        if !nb.is_empty() {
            nb.to_string()
        } else {
            let meta = json(rdir.join("meta.json"));
            s(&meta, "netBase").trim().to_string()
        }
    };
    if !net_base.is_empty() && !rdir.join(".sync-base").exists() {
        if let Some(cur_head) = read_git_head(&source_root) {
            if cur_head == net_base {
                return store(false);
            }
        }
    }

    let rec = json(rdir.join("changes.json"));
    if !rec.is_object() {
        return store(false);
    }

    if let Some(final_entries) = rec["finalEntries"].as_object().filter(|m| !m.is_empty()) {
        let mut paths = Vec::with_capacity(final_entries.len());
        let mut expected = Vec::with_capacity(final_entries.len());
        for (path, entry) in final_entries {
            if path.is_empty() || !entry.is_object() || s(entry, "mode").is_empty() {
                return store(false);
            }
            paths.push(format!("HEAD:{path}"));
            expected.push(entry);
        }
        let Some(blobs) = git_batch_blobs(&source_root, &paths) else {
            return store(false);
        };
        if blobs.len() != expected.len() {
            return store(false);
        }
        for (entry, (obj_type, content)) in expected.into_iter().zip(blobs) {
            if s(entry, "mode") == "absent" {
                if obj_type != "missing" {
                    return store(false);
                }
                continue;
            }
            let Some(bytes) = content else {
                return store(false);
            };
            if obj_type != "blob" {
                return store(false);
            }
            let Some(exp_size) = entry["size"].as_u64() else {
                return store(false);
            };
            if bytes.len() as u64 != exp_size {
                return store(false);
            }
            let exp_hash = s(entry, "hash");
            let actual_hash = sha1_smol::Sha1::from(&bytes).digest().to_string();
            if actual_hash != exp_hash {
                return store(false);
            }
        }
        return store(true);
    }

    // Fallback for records without `finalEntries`: compare `after` tree blobs against `HEAD`.
    let after_tree = s(&rec, "after").trim();
    if let Some(raw_changes) = rec["changes"]
        .as_array()
        .filter(|a| !a.is_empty() && !after_tree.is_empty())
    {
        let mut paths = Vec::with_capacity(raw_changes.len());
        for ch in raw_changes {
            if !ch.is_object() || b(ch, "large") || b(ch, "submodule") {
                return store(false);
            }
            let p = s(ch, "path").trim();
            if p.is_empty() {
                return store(false);
            }
            paths.push(p.to_string());
        }
        let mut specs = Vec::with_capacity(paths.len() * 2);
        for p in &paths {
            specs.push(format!("{after_tree}:{p}"));
        }
        for p in &paths {
            specs.push(format!("HEAD:{p}"));
        }
        let Some(blobs) = git_batch_blobs(&source_root, &specs) else {
            return store(false);
        };
        if blobs.len() != specs.len() {
            return store(false);
        }
        let half = paths.len();
        for i in 0..half {
            if blobs[i] != blobs[half + i] {
                return store(false);
            }
        }
        return store(true);
    }

    store(false)
}

fn load_queue_items_by_name(state: &Path) -> Option<HashMap<String, Vec<Value>>> {
    let doc = json(state.join("dag-queue.json"));
    let items = doc["items"].as_array()?;
    let mut by_name: HashMap<String, Vec<Value>> = HashMap::new();
    for item in items {
        let name = s(item, "name").trim();
        if !name.is_empty() {
            by_name
                .entry(name.to_string())
                .or_default()
                .push(item.clone());
        }
    }
    Some(by_name)
}

#[derive(Default)]
struct ResolvingSeqs {
    applied: HashMap<String, u64>,
    cleaned: HashMap<String, u64>,
    stopped: HashMap<String, u64>,
    terminal: HashMap<String, u64>,
    premerge_ready: HashMap<String, u64>,
    review_pass: HashMap<String, u64>,
    replied: HashMap<String, u64>,
}

impl ResolvingSeqs {
    fn bump(map: &mut HashMap<String, u64>, key: &str, seq: u64) {
        if key.is_empty() {
            return;
        }
        let e = map.entry(key.to_string()).or_insert(0);
        if seq > *e {
            *e = seq;
        }
    }
}

fn inspect_run_resolution(
    rid: &str,
    seq: u64,
    run_obj: Option<&Value>,
    replied_parents: &HashSet<String>,
    resolving: &ResolvingSeqs,
    cache: &Path,
) -> (bool, bool, bool, bool) {
    let mut is_applied = resolving.applied.get(rid).copied().unwrap_or(0) > seq
        || run_obj.is_some_and(|o| b(o, "applied") || !s(o, "appliedAt").trim().is_empty());
    let mut is_cleaned = resolving.cleaned.get(rid).copied().unwrap_or(0) > seq
        || run_obj.is_some_and(|o| b(o, "cleaned"));
    let mut is_replied = replied_parents.contains(rid)
        || resolving.replied.get(rid).copied().unwrap_or(0) > seq
        || run_obj.is_some_and(|o| !s(o, "repliedAt").trim().is_empty());
    let mut has_result_read = run_obj.is_some_and(|o| !s(o, "resultReadAt").trim().is_empty());

    let Some(obj) = run_obj else {
        if !is_cleaned && cache.join("archive").join(rid).join("record").is_dir() {
            is_cleaned = true;
        }
        return (is_applied, is_cleaned, is_replied, has_result_read);
    };

    let rdir_str = s(obj, "dir");
    if !rdir_str.is_empty() && !(is_applied && is_replied && has_result_read) {
        let rdir = Path::new(rdir_str);
        if !is_applied
            && (crate::runs::is_applied_marker(rdir)
                || (rdir.join(".applied-cleaned").exists()
                    && !rdir.join(".generate-pending").exists()))
        {
            is_applied = true;
        }
        if !(is_applied && is_replied && has_result_read) {
            let meta = json(rdir.join("meta.json"));
            if meta.is_object() {
                if !s(&meta, "resultReadAt").trim().is_empty() {
                    has_result_read = true;
                }
                if !s(&meta, "appliedAt").trim().is_empty()
                    && !rdir.join(".generate-pending").exists()
                {
                    is_applied = true;
                }
                if !s(&meta, "repliedAt").trim().is_empty() {
                    is_replied = true;
                }
            }
        }
    }
    (is_applied, is_cleaned, is_replied, has_result_read)
}

#[allow(clippy::too_many_arguments)]
fn is_alert_event_stale(
    ev: &Value,
    seq: u64,
    now: i64,
    work_by_id: &HashMap<String, &WorkItem>,
    all_runs_by_id: &HashMap<String, Value>,
    replied_parents: &HashSet<String>,
    resolving: &ResolvingSeqs,
    queue_items_by_name: Option<&HashMap<String, Vec<Value>>>,
    cache: &Path,
    verbatim_head_cache: &mut HashMap<String, bool>,
) -> bool {
    let kind = s(ev, "kind");
    let pl = &ev["payload"];

    if matches!(kind, "work_ready" | "work_defer_expired" | "work_stale") {
        let wid = s(pl, "workId").trim();
        let Some(w) = (!wid.is_empty()).then(|| work_by_id.get(wid)).flatten() else {
            return false;
        };
        if kind == "work_ready" {
            return w.state != "ready";
        }
        if w.state == "done" {
            return true;
        }
        if w.state == "deferred" && iso_epoch(w.defer_until.trim()).is_some_and(|ts| ts > now) {
            return true;
        }
        if kind == "work_defer_expired" {
            return w.state == "active";
        }
        if kind == "work_stale" {
            let ev_state = s(pl, "state").trim();
            if !ev_state.is_empty() && w.state != ev_state {
                return true;
            }
            return false;
        }
    }

    if kind == "queue_paused" {
        let qname = {
            let q = s(pl, "queueName").trim();
            if !q.is_empty() {
                q
            } else {
                s(pl, "name").trim()
            }
        };
        if !qname.is_empty() {
            if let Some(entries) = queue_items_by_name.and_then(|m| m.get(qname)) {
                if !entries.is_empty() && !entries.iter().any(|e| s(e, "status") == "paused") {
                    return true;
                }
            }
        }
        return false;
    }

    let rid = s(ev, "run").trim();
    if rid.is_empty() {
        return false;
    }
    let run_obj = all_runs_by_id.get(rid);
    let (is_applied, is_cleaned, is_replied, has_result_read) =
        inspect_run_resolution(rid, seq, run_obj, replied_parents, resolving, cache);
    if is_applied || is_cleaned || is_replied {
        return true;
    }
    if let Some(ro) = run_obj {
        if s(ro, "mode") == "write"
            && matches!(s(ro, "state"), "delivered" | "rejected")
            && is_run_verbatim_in_source_head(ro, None, verbatim_head_cache)
        {
            return true;
        }
    }
    if has_result_read
        && (matches!(kind, "delivered" | "question" | "rejected" | "failed")
            || (kind == "needs_attention" && b(pl, "policyRefused")))
    {
        return true;
    }
    if kind == "stall_warning" {
        if resolving.stopped.get(rid).copied().unwrap_or(0) > seq
            || resolving.terminal.get(rid).copied().unwrap_or(0) > seq
        {
            return true;
        }
        if let Some(ro) = run_obj {
            let rst = s(ro, "state");
            if !rst.is_empty() && !matches!(rst, "running" | "starting" | "waiting" | "queued" | "scouting") {
                return true;
            }
        }
        return false;
    }
    if matches!(kind, "premerge_conflict" | "premerge_failed") {
        if resolving.premerge_ready.get(rid).copied().unwrap_or(0) > seq {
            return true;
        }
        if let Some(ro) = run_obj {
            if s(&ro["premerge"], "status") == "ready" {
                return true;
            }
        }
        return false;
    }
    if kind == "needs_attention"
        || (kind == "review_verdict" && s(pl, "verdict") == "NEEDS_CONTROLLER")
    {
        if resolving.review_pass.get(rid).copied().unwrap_or(0) > seq {
            return true;
        }
        if let Some(ro) = run_obj {
            if b(&ro["sign"], "valid") {
                return true;
            }
        }
        return false;
    }
    false
}

fn read_capacity_limits(current_repo: Option<&Path>) -> (usize, usize) {
    let user_cfg = json(config_dir().join("config.json"));
    let repo_cfg = current_repo
        .map(|r| json(r.join(".delegate.json")))
        .unwrap_or(Value::Null);
    let resolve = |env_key: &str, cfg_key: &str, default: usize| -> usize {
        if let Some(v) = parse_nonneg_usize_env(env_key) {
            return v;
        }
        for cfg in [&repo_cfg, &user_cfg] {
            if let Some(v) = cfg[cfg_key].as_u64() {
                return v as usize;
            }
        }
        default
    };
    (
        resolve("DELEGATE_MAX_ACTIVE", "maxActive", DEFAULT_MAX_ACTIVE),
        resolve(
            "DELEGATE_REPO_MAX_ACTIVE",
            "repoMaxActive",
            DEFAULT_REPO_MAX_ACTIVE,
        ),
    )
}

fn has_idle_slot(
    state: &Path,
    current_repo: Option<&Path>,
    repo_items: &[(DiscoveredRepo, Vec<Value>)],
    now: i64,
) -> bool {
    let (max_active, repo_max_active) = read_capacity_limits(current_repo);
    let mut cur_active = HashSet::new();
    let mut cur_queued = HashSet::new();
    let mut total_active = HashSet::new();

    for (repo, items) in repo_items {
        for obj in items {
            let rid = s(obj, "run").trim();
            if rid.is_empty() {
                continue;
            }
            let st = s(obj, "state");
            if matches!(st, "running" | "starting" | "waiting" | "queued" | "scouting") {
                total_active.insert(rid.to_string());
                if repo.is_current {
                    cur_active.insert(rid.to_string());
                    if matches!(st, "waiting" | "queued") {
                        cur_queued.insert(rid.to_string());
                    }
                }
            }
        }
    }
    if let Ok(rd) = fs::read_dir(state) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("slot") {
                continue;
            }
            let rdir = PathBuf::from(read(&p).trim());
            if !rdir.is_dir() || rdir.join("exit_code").is_file() {
                continue;
            }
            if let Some(snap) = read_run_snapshot(&rdir, now) {
                let rid = s(&snap, "run").trim();
                let st = s(&snap, "state");
                if !rid.is_empty() && matches!(st, "running" | "starting" | "waiting" | "queued" | "scouting") {
                    total_active.insert(rid.to_string());
                }
            }
        }
    }

    if !cur_queued.is_empty() {
        return false;
    }
    if repo_max_active > 0 && cur_active.len() >= repo_max_active {
        return false;
    }
    if max_active > 0 && total_active.len() >= max_active {
        return false;
    }
    true
}

fn load_work_remind(state: &Path) -> BTreeMap<String, BTreeMap<String, i64>> {
    let doc = json(state.join("work-remind.json"));
    let mut out: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
    let Some(map) = doc.as_object() else {
        return out;
    };
    for (k, v) in map {
        let wid = k.trim();
        if wid.is_empty() {
            continue;
        }
        if let Some(inner) = v.as_object() {
            let mut per_caller = BTreeMap::new();
            for (ck, cv) in inner {
                let c = ck.trim();
                if c.is_empty() {
                    continue;
                }
                if let Some(ts) = parse_remind_ts(cv) {
                    per_caller.insert(c.to_string(), ts);
                }
            }
            if !per_caller.is_empty() {
                out.insert(wid.to_string(), per_caller);
            }
        } else if let Some(ts) = parse_remind_ts(v) {
            let mut per_caller = BTreeMap::new();
            per_caller.insert("default".to_string(), ts);
            out.insert(wid.to_string(), per_caller);
        }
    }
    out
}

fn parse_remind_ts(v: &Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    if let Some(f) = v.as_f64() {
        return Some(f as i64);
    }
    v.as_str().and_then(iso_epoch)
}

fn save_work_remind(state: &Path, remind_map: &BTreeMap<String, BTreeMap<String, i64>>, now: i64) {
    let _ = fs::create_dir_all(state);
    let cutoff = now - 7 * 86_400;
    let mut root = serde_json::Map::new();
    for (wid, per_caller) in remind_map {
        let mut inner = serde_json::Map::new();
        for (ck, &ts) in per_caller {
            if ts >= cutoff {
                inner.insert(
                    ck.clone(),
                    json!(format_epoch(ts, false, "%Y-%m-%dT%H:%M:%SZ")),
                );
            }
        }
        if !inner.is_empty() {
            root.insert(wid.clone(), Value::Object(inner));
        }
    }
    let _ = write_json(state.join("work-remind.json"), &Value::Object(root));
}

fn append_stop_log(state: &Path, rec: Value, now: i64) {
    let _ = fs::create_dir_all(state);
    let path = state.join("stop-hook.jsonl");
    let mut row = rec.as_object().cloned().unwrap_or_default();
    row.insert(
        "ts".to_string(),
        json!(format_epoch(now, false, "%Y-%m-%dT%H:%M:%SZ")),
    );
    let line = format!("{}\n", Value::Object(row));
    if let Ok(mut fh) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = fh.write_all(line.as_bytes());
    }
}

fn run_label(obj: &Value, repo_label: &str, is_current: bool) -> String {
    let raw = {
        let c = s(obj, "chainName");
        if !c.is_empty() {
            c
        } else {
            let n = s(obj, "name");
            if !n.is_empty() {
                n
            } else {
                let r = s(obj, "run");
                if !r.is_empty() {
                    r
                } else {
                    "?"
                }
            }
        }
    };
    let clean = raw.replace(['\n', '\r'], " ");
    if is_current {
        clean
    } else {
        format!("【{repo_label}】{clean}")
    }
}

fn build_chain_ids_for_run(
    rid: &str,
    obj: &Value,
    parent_map: &HashMap<String, String>,
    repo_root: Option<&Path>,
    state: &Path,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    if !rid.is_empty() && seen.insert(rid.to_string()) {
        out.push(rid.to_string());
    }
    let mut cur = s(obj, "parent").trim().to_string();
    for _ in 0..64 {
        if cur.is_empty() || !seen.insert(cur.clone()) {
            break;
        }
        out.push(cur.clone());
        if let Some(next) = parent_map.get(&cur) {
            cur = next.clone();
        } else {
            let repo_str = repo_root.map(|p| p.to_string_lossy().into_owned());
            let tail = work::run_chain_ids_for_ref(&cur, repo_str.as_deref(), state);
            for t in tail.into_iter().skip(1) {
                if seen.insert(t.clone()) {
                    out.push(t);
                }
            }
            break;
        }
    }
    out
}

fn format_downstream_preview(ids: &[String]) -> String {
    if ids.len() <= 3 {
        ids.join("、")
    } else {
        format!("{} 等", ids[..3].join("、"))
    }
}

fn build_unmerged_reason(
    obj: &Value,
    expired_deferred: bool,
    blocked_downstream: &[String],
    wait_mins: u64,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    let pm = &obj["premerge"];
    if pm.is_object() {
        match s(pm, "status") {
            "failed" => {
                let layer = s(pm, "failedLayer").trim();
                if !layer.is_empty() {
                    parts.push(format!("预合并失败：{layer}"));
                } else {
                    parts.push("预合并失败".to_string());
                }
            }
            "conflict" => {
                let conflicts = strings(&pm["conflicts"]);
                if let Some(first) = conflicts.first() {
                    if conflicts.len() > 1 {
                        parts.push(format!("预合并冲突：{first} 等 {} 个文件", conflicts.len()));
                    } else {
                        parts.push(format!("预合并冲突：{first}"));
                    }
                } else {
                    parts.push("预合并冲突".to_string());
                }
            }
            _ => {}
        }
    }
    if s(&obj["review"], "verdict") == "NEEDS_CONTROLLER" && !b(&obj["sign"], "valid") {
        parts.push("需主控审查签字".to_string());
    }
    if expired_deferred {
        parts.push("暂缓已到期".to_string());
    }
    if s(obj, "state") == "rejected" {
        parts.push("验收未通过".to_string());
    }
    if !blocked_downstream.is_empty() {
        let preview = format_downstream_preview(blocked_downstream);
        parts.push(format!(
            "挡 {} 个下游（{preview}），已等 {wait_mins} 分钟",
            blocked_downstream.len()
        ));
    }
    parts.join("；")
}

fn repo_prefix_cmd(repo_root: Option<&Path>, is_current: bool, cmd: &str) -> String {
    if is_current {
        cmd.to_string()
    } else if let Some(rr) = repo_root {
        format!("cd {} && {cmd}", shell_quote(&rr.to_string_lossy()))
    } else {
        cmd.to_string()
    }
}

#[derive(Clone, Debug)]
enum TemplateKind {
    UnreadDelivery {
        name: String,
        run: String,
    },
    Question {
        name: String,
        run: String,
    },
    Waiter {
        name: String,
        count: usize,
        any_current: bool,
        run_ids: Vec<String>,
        fastest_eta: Option<String>,
    },
    Unmerged {
        name: String,
        run: String,
        reason: String,
    },
    WorkReady {
        wid: String,
        title: String,
        hours: u64,
        brief: String,
    },
    WorkMissingBrief {
        wid: String,
        title: String,
    },
    WorkVerify {
        wid: String,
        title: String,
        wait_secs: i64,
        blocked_downstream: Vec<String>,
    },
    UnreadEvent {
        preview: String,
        cursor_seq: u64,
    },
    Adopt {
        name: String,
        run: String,
    },
}

#[derive(Clone, Debug)]
pub struct PendingItem {
    priority_tier: u8,
    stall_score: u64,
    blocked_downstream: Vec<String>,
    wait_secs: i64,
    is_orphan: bool,
    hook_kind: &'static str,
    category: &'static str,
    category_label: &'static str,
    target_id: String,
    display_name: String,
    repo_root: Option<PathBuf>,
    is_current: bool,
    status_text: String,
    work_id_for_cooldown: Option<String>,
    template: TemplateKind,
}

impl PendingItem {
    fn next_command(&self, format: HookFormat) -> String {
        match &self.template {
            TemplateKind::UnreadDelivery { run, .. } => {
                let cmd = repo_prefix_cmd(
                    self.repo_root.as_deref(),
                    self.is_current,
                    &format!("delegate card {run}"),
                );
                format!("`{cmd}`")
            }
            TemplateKind::Question { run, .. } => {
                let c1 = repo_prefix_cmd(
                    self.repo_root.as_deref(),
                    self.is_current,
                    &format!("delegate result {run}"),
                );
                let c2 = repo_prefix_cmd(
                    self.repo_root.as_deref(),
                    self.is_current,
                    &format!("delegate reply {run} \"<决定>\""),
                );
                format!("`{c1}`，再 `{c2}`")
            }
            TemplateKind::Waiter { any_current, .. } => {
                let wcmd = if *any_current {
                    "delegate wait --any"
                } else {
                    "delegate wait --machine"
                };
                match format {
                    HookFormat::Agy => {
                        format!("run_command 设 WaitMsBeforeAsync=500 后台跑 `{wcmd}`")
                    }
                    HookFormat::Claude | HookFormat::Json => format!("后台跑 `{wcmd}`"),
                }
            }
            TemplateKind::Unmerged { run, .. } => {
                let card_cmd = repo_prefix_cmd(
                    self.repo_root.as_deref(),
                    self.is_current,
                    &format!("delegate card {run}"),
                );
                format!(
                    "`{card_cmd}`；合不了用 `delegate work defer {run} --until 4h --reason \"…\"`"
                )
            }
            TemplateKind::WorkReady { wid, brief, .. } => {
                let start_cmd = repo_prefix_cmd(
                    self.repo_root.as_deref(),
                    self.is_current,
                    &format!("delegate start --work {wid} --prompt-file {brief}"),
                );
                format!("`{start_cmd}`")
            }
            TemplateKind::WorkMissingBrief { wid, .. } => {
                format!("`delegate work wait {wid} --brief <文件>`")
            }
            TemplateKind::WorkVerify { wid, .. } => {
                format!("`delegate work close {wid} --evidence \"…\"`")
            }
            TemplateKind::UnreadEvent { cursor_seq, .. } => {
                format!("`delegate events --since {cursor_seq}`")
            }
            TemplateKind::Adopt { run, .. } => {
                format!("`delegate adopt {run}`")
            }
        }
    }

    pub fn format_block_reason(&self, remaining: usize, format: HookFormat) -> String {
        let more = if remaining > 0 {
            format!("（另有 {remaining} 件：`delegate hook pending`）")
        } else {
            String::new()
        };
        let next = self.next_command(format);
        match &self.template {
            TemplateKind::UnreadDelivery { name, .. } => {
                format!("{name} 已交付待看。下一步：{next}{more}")
            }
            TemplateKind::Question { name, .. } => {
                format!("{name} 在等你决定。下一步：{next}{more}")
            }
            TemplateKind::Waiter {
                name,
                count,
                fastest_eta,
                ..
            } => {
                let tail = match (
                    fastest_eta.as_deref().filter(|s| !s.is_empty()),
                    remaining > 0,
                ) {
                    (Some(eta), true) => {
                        format!("（最快预计 {eta}；另有 {remaining} 件：`delegate hook pending`）")
                    }
                    (Some(eta), false) => format!("（最快预计 {eta}）"),
                    (None, true) => more,
                    (None, false) => String::new(),
                };
                format!("{name} 等 {count} 个委派在跑，没有挂等待。下一步：{next}{tail}")
            }
            TemplateKind::Unmerged { name, reason, .. } => {
                let paren = if reason.is_empty() {
                    String::new()
                } else {
                    format!("（{reason}）")
                };
                format!("{name} 已交付未合入{paren}。下一步：{next}{more}")
            }
            TemplateKind::WorkReady {
                wid, title, hours, ..
            } => {
                let t = clip_title(title, TITLE_MAX_CHARS);
                format!("[{wid}] {t} 就绪 {hours} 小时没派。下一步：{next}{more}")
            }
            TemplateKind::WorkMissingBrief { wid, title } => {
                let t = clip_title(title, TITLE_MAX_CHARS);
                format!("[{wid}] {t} 没有任务说明。下一步：{next}{more}")
            }
            TemplateKind::WorkVerify {
                wid,
                title,
                wait_secs,
                blocked_downstream,
            } => {
                let t = clip_title(title, TITLE_MAX_CHARS);
                if blocked_downstream.is_empty() {
                    let h = (*wait_secs).max(0) / 3600;
                    format!("[{wid}] {t} 待闭环 {h} 小时。下一步：{next}{more}")
                } else {
                    let preview = format_downstream_preview(blocked_downstream);
                    let dur = if *wait_secs < 3600 {
                        format!("{} 分钟", (*wait_secs).max(0) / 60)
                    } else {
                        format!("{} 小时", (*wait_secs).max(0) / 3600)
                    };
                    format!(
                        "[{wid}] {t} 待闭环 {dur}（挡 {} 个下游：{preview}）。下一步：{next}{more}",
                        blocked_downstream.len()
                    )
                }
            }
            TemplateKind::UnreadEvent { preview, .. } => {
                format!("{preview}。下一步：{next}{more}")
            }
            TemplateKind::Adopt { name, .. } => {
                format!("{name} 属同终端/同工作目录已失效会话。下一步：{next}{more}")
            }
        }
    }

    fn format_pending_line(&self) -> String {
        let next = self.next_command(HookFormat::Claude);
        let obj = match &self.template {
            TemplateKind::WorkReady { wid, title, .. }
            | TemplateKind::WorkMissingBrief { wid, title }
            | TemplateKind::WorkVerify { wid, title, .. } => {
                format!("[{wid}] {}", clip_title(title, TITLE_MAX_CHARS))
            }
            _ => self.display_name.clone(),
        };
        format!(
            "[{}] {obj} · {} · 下一步：{next}",
            self.category_label, self.status_text
        )
    }

    fn to_json(&self, format: HookFormat) -> Value {
        let mut obj = json!({
            "category": self.category,
            "categoryLabel": self.category_label,
            "hook": self.hook_kind,
            "priority": self.priority_tier,
            "stallScore": self.stall_score,
            "blockedDownstream": self.blocked_downstream,
            "target": self.target_id,
            "name": self.display_name,
            "repo": self.repo_root.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
            "isCurrent": self.is_current,
            "status": self.status_text,
            "next": self.next_command(format),
            "message": self.format_block_reason(0, format),
        });
        if let TemplateKind::Waiter {
            fastest_eta: Some(eta),
            ..
        } = &self.template
        {
            obj["fastestEta"] = json!(eta);
        }
        obj
    }

    fn stop_log_record(&self, format: HookFormat) -> Value {
        let fmt_str = match format {
            HookFormat::Claude => "claude",
            HookFormat::Agy => "agy",
            HookFormat::Json => "json",
        };
        match &self.template {
            TemplateKind::Waiter { run_ids, .. } => json!({
                "hook": self.hook_kind,
                "runs": self.display_name,
                "runIds": run_ids,
                "format": fmt_str,
            }),
            TemplateKind::WorkReady { wid, .. }
            | TemplateKind::WorkMissingBrief { wid, .. }
            | TemplateKind::WorkVerify { wid, .. } => json!({
                "hook": self.hook_kind,
                "runs": wid,
                "workIds": [wid],
                "format": fmt_str,
            }),
            _ => json!({
                "hook": self.hook_kind,
                "runs": self.display_name,
                "runIds": if self.target_id.is_empty() { Vec::<String>::new() } else { vec![self.target_id.clone()] },
                "format": fmt_str,
            }),
        }
    }
}

/// Strictly zero-lock, zero-disk-write, zero-git fastest delivery clock for uncovered
/// live runs in the existing `waiter` reminder (never adds new blocking conditions).
fn fastest_waiter_eta(
    state: &Path,
    now: i64,
    uncovered: &[(String, String, bool, Option<PathBuf>, Value)],
) -> Option<String> {
    if uncovered.is_empty() {
        return None;
    }
    let model = crate::speed::load_or_refresh_eta_model_in(state, now, false);
    let mut visible: Vec<PathBuf> = Vec::new();
    let mut precomputed: HashMap<PathBuf, Value> = HashMap::new();
    for (_, _, _, rdir, obj) in uncovered {
        if let Some(p) = rdir {
            visible.push(p.clone());
            precomputed.insert(p.clone(), obj.clone());
        }
    }
    let ctx = crate::eta::LiveContext::capture_readonly(state, now, &visible, precomputed, None);
    let mut best: Option<(i64, String)> = None;
    for (_, _, _, rdir, obj) in uncovered {
        let re = if let Some(p) = rdir.as_deref().filter(|p| p.join("meta.json").is_file()) {
            crate::eta::estimate_run_with_status(p, obj, &model, &ctx)
        } else if let Some(fb) = crate::eta::estimate_live_status_fallback(obj, &model, now) {
            fb
        } else {
            continue;
        };
        if re.blocker.is_some() {
            continue;
        }
        let Some(del) = re.to_delivered else {
            continue;
        };
        let verb = if re.mode == "write" { "交付" } else { "答复" };
        let text = if del.p50_sec < 300.0 {
            format!("<5m {verb}")
        } else {
            format!("{} {verb}", crate::eta::format_clock(del.rounded_p50_at, now))
        };
        if best.as_ref().is_none_or(|(ts, _)| del.rounded_p50_at < *ts) {
            best = Some((del.rounded_p50_at, text));
        }
    }
    best.map(|(_, text)| text)
}

struct EvalOutput {
    items: Vec<PendingItem>,
    pending_runs: Vec<String>,
    controller_pending_runs: Vec<String>,
    stale_pending_runs: Vec<String>,
    caller_id: String,
}

fn evaluate_core(
    state: &Path,
    cache: &Path,
    payload: &Value,
    cwd: &str,
    now: i64,
    mode: EvalMode,
) -> EvalOutput {
    let (caller_id, caller_source) = resolve_caller(payload);
    let (cur_harness, cur_tty, cur_cwd) = touch_caller_session_for(
        state,
        &caller_id,
        &caller_source,
        cwd,
        now,
        mode == EvalMode::Check,
    );
    let orphan_secs = parse_pos_f64_env(
        "DELEGATE_ORPHAN_TAKEOVER_HOURS",
        DEFAULT_ORPHAN_TAKEOVER_HOURS,
    ) * 3600.0;
    let stall_penetrate_secs = (parse_pos_f64_env(
        "DELEGATE_STALL_PENETRATE_MINUTES",
        DEFAULT_STALL_PENETRATE_MINUTES,
    ) * 60.0) as i64;
    let stall_verify_secs = (parse_pos_f64_env(
        "DELEGATE_STALL_VERIFY_MINUTES",
        DEFAULT_STALL_VERIFY_MINUTES,
    ) * 60.0) as i64;
    let verify_stale_secs = (parse_pos_f64_env(
        "DELEGATE_WORK_STALE_VERIFY_HOURS",
        DEFAULT_STALE_VERIFY_HOURS,
    ) * 3600.0) as i64;
    let ready_stale_secs =
        (parse_pos_f64_env("DELEGATE_WORK_STALE_READY_HOURS", DEFAULT_STALE_READY_HOURS) * 3600.0)
            as i64;
    let missing_brief_grace_secs = (parse_pos_f64_env(
        "DELEGATE_WORK_MISSING_BRIEF_GRACE_HOURS",
        DEFAULT_MISSING_BRIEF_GRACE_HOURS,
    ) * 3600.0) as i64;
    let cooldown_secs = (parse_pos_f64_env(
        "DELEGATE_WORK_REMIND_COOLDOWN_HOURS",
        DEFAULT_REMIND_COOLDOWN_HOURS,
    ) * 3600.0) as i64;

    let current_only = env_nonempty("DELEGATE_STOP_HOOK_CURRENT_ONLY").as_deref() == Some("1");
    let cursors_doc = json(state.join("cursors.json"));
    let mut liveness = CallerLivenessChecker::new(
        state,
        cursors_doc.clone(),
        now,
        orphan_secs,
        caller_id.clone(),
        cur_harness,
        cur_tty,
        cur_cwd,
    );

    let index_runs = load_index_runs(state);
    let work_items = work::load_readonly_in(state);
    let work_by_id: HashMap<String, &WorkItem> = work_items
        .iter()
        .filter(|w| !w.id.is_empty())
        .map(|w| (w.id.clone(), w))
        .collect();
    let repos = discover_repos(state, cache, cwd, &index_runs, current_only);

    let mut repo_items: Vec<(DiscoveredRepo, Vec<Value>)> = Vec::new();
    let mut all_runs_by_id: HashMap<String, Value> = HashMap::new();
    let mut replied_parents: HashSet<String> = HashSet::new();
    let mut parent_map: HashMap<String, String> = HashMap::new();

    for (rdir_key, entry) in &index_runs {
        if entry["status"].is_object() {
            let mut st_obj = entry["status"].clone();
            if s(&st_obj, "dir").is_empty() {
                st_obj["dir"] = json!(rdir_key);
            }
            overlay_index_meta(
                &mut st_obj,
                &entry["meta"],
                (!rdir_key.is_empty()).then(|| Path::new(rdir_key.as_str())),
            );
            let rid = s(&st_obj, "run").to_string();
            let par = s(&st_obj, "parent").to_string();
            if !rid.is_empty() {
                if !par.is_empty() {
                    parent_map.insert(rid.clone(), par.clone());
                    replied_parents.insert(par);
                }
                all_runs_by_id.insert(rid, st_obj);
            }
        }
    }

    let mut all_repo_runs_flat: Vec<Value> = Vec::new();
    for repo in repos {
        let items = fetch_status(&repo.runs_dir, &index_runs, now);
        for obj in &items {
            let rid = s(obj, "run").to_string();
            let par = s(obj, "parent").to_string();
            if !rid.is_empty() {
                if !par.is_empty() {
                    parent_map.insert(rid.clone(), par.clone());
                    replied_parents.insert(par);
                }
                all_runs_by_id.insert(rid, obj.clone());
            }
            all_repo_runs_flat.push(obj.clone());
        }
        repo_items.push((repo, items));
    }

    let pending_runs = classify_pending_runs(&all_repo_runs_flat);
    let work_items_json: Vec<Value> = work_items.iter().map(work::WorkItem::to_json).collect();
    let pending_split = crate::pulse::split_pending_runs(&all_repo_runs_flat, &work_items_json, now);

    let mut out_items: Vec<PendingItem> = Vec::new();
    let mut covered_run_ids: HashSet<String> = HashSet::new();
    let mut adoptable_runs: Vec<(String, String, Option<PathBuf>, bool)> = Vec::new();
    let mut adoptable_seen: HashSet<String> = HashSet::new();

    let (work_fresh_def, work_expired_def) = work_deferred_runs(&work_items, now);
    let procs = if !repo_items.is_empty() {
        list_processes()
    } else {
        Vec::new()
    };
    let applying_procs: Vec<(Vec<String>, Option<PathBuf>)> = procs
        .iter()
        .filter_map(|(argv, pcwd)| {
            match_delegate_subcmd(argv, "apply")
                .or_else(|| match_delegate_subcmd(argv, "land"))
                .map(|a| (a, pcwd.clone()))
        })
        .collect();
    let merging_procs: Vec<(Vec<String>, Option<PathBuf>)> = procs
        .iter()
        .filter_map(|(argv, pcwd)| match_delegate_subcmd(argv, "merge").map(|a| (a, pcwd.clone())))
        .collect();
    let (gate_cwds, lane_runs) = active_lane_state(state, &procs);

    let mut live_current: Vec<(String, String, Value, Option<PathBuf>)> = Vec::new();
    let mut live_other: Vec<(String, String, Value, Option<PathBuf>)> = Vec::new();
    let mut pending_unmerged_candidates: Vec<PendingItem> = Vec::new();
    let mut pending_unread_fallback_runs: Vec<PendingItem> = Vec::new();
    let mut verbatim_head_cache: HashMap<String, bool> = HashMap::new();

    // ------------------------------------------------------------------
    // 1. Scan leaf runs across discovered repositories
    // ------------------------------------------------------------------
    for (repo, items) in &repo_items {
        if items.is_empty() {
            continue;
        }
        let mut all_by_id: HashMap<String, Value> = HashMap::new();
        let mut parents: HashSet<String> = HashSet::new();
        let mut active_merge_sources: HashSet<String> = HashSet::new();
        for obj in items {
            let rid = s(obj, "run");
            if !rid.is_empty() {
                all_by_id.insert(rid.to_string(), obj.clone());
            }
            let par = s(obj, "parent");
            if !par.is_empty() {
                parents.insert(par.to_string());
            }
            if b(obj, "isMerge")
                && matches!(
                    s(obj, "state"),
                    "running" | "waiting" | "starting" | "queued" | "scouting"
                )
            {
                for src in strings(&obj["merge"]["sourceRuns"]) {
                    if !src.is_empty() {
                        active_merge_sources.insert(src);
                    }
                }
            }
        }

        for orig_obj in items {
            let mut obj = orig_obj.clone();
            let rid = s(&obj, "run").to_string();
            if rid.is_empty() || parents.contains(&rid) {
                continue;
            }
            let st = s(&obj, "state").to_string();
            let run_mode = s(&obj, "mode").to_string();
            let is_applied = b(&obj, "applied") || !s(&obj, "appliedAt").trim().is_empty();
            let is_cleaned = b(&obj, "cleaned") || st == "cleaned";
            let is_replied = !s(&obj, "repliedAt").trim().is_empty();
            let is_question = b(&obj, "question") && !is_replied;
            if is_cleaned || is_applied {
                continue;
            }

            let base_disp = run_label(&obj, &repo.label, repo.is_current);
            let run_caller = get_run_caller(&mut obj);
            let unclaimed_ts = get_run_unclaimed_ts(&obj);
            let wait_secs = unclaimed_ts.map_or(0, |ts| (now - ts).max(0));
            let wait_mins = (wait_secs / 60) as u64;

            // Compute chain IDs and transitive blocked downstream work items for unmerged write runs
            let chain_ids =
                build_chain_ids_for_run(&rid, &obj, &parent_map, repo.repo_root.as_deref(), state);
            let mut owning_wids = Vec::new();
            let wid_meta = s(&obj, "workId").trim();
            if !wid_meta.is_empty() {
                owning_wids.push(wid_meta.to_string());
            }
            let blocked_downstream = if st == "delivered" && run_mode == "write" {
                work::blocked_downstream_in(&work_items, &chain_ids, &owning_wids)
            } else {
                Vec::new()
            };

            let is_deferred_chain = chain_ids.iter().any(|cid| work_fresh_def.contains(cid));
            let is_expired_def_chain = chain_ids.iter().any(|cid| work_expired_def.contains(cid));

            let is_adoptable = liveness.is_adoptable(&run_caller);
            let in_normal_scope = run_belongs_to_scope(&obj, repo.is_current, &caller_id);

            // Check stall-chain cross-repo / cross-session penetration (追加 1)
            let zero_change = is_zero_change_delivery(&obj, &all_by_id);
            let is_unmerged_write = st == "delivered" && run_mode == "write" && !zero_change;
            if is_unmerged_write
                && is_run_verbatim_in_source_head(
                    &obj,
                    repo.repo_root.as_deref(),
                    &mut verbatim_head_cache,
                )
            {
                continue;
            }
            let caller_dead = !run_caller.is_empty() && !liveness.is_alive(&run_caller);
            let stall_penetrates = is_unmerged_write
                && !blocked_downstream.is_empty()
                && !is_deferred_chain
                && (caller_dead || wait_secs >= stall_penetrate_secs);

            if !in_normal_scope
                && !(is_adoptable && liveness.same_terminal(&run_caller))
                && !stall_penetrates
            {
                continue;
            }

            let mut adopt_only = false;
            let mut is_orphan = false;
            let mut disp = base_disp.clone();
            if !run_caller.is_empty() && run_caller != caller_id {
                if stall_penetrates {
                    // Cross-session stall-chain penetration: directly blocks active controller!
                    if caller_dead {
                        is_orphan = true;
                    }
                } else if repo.is_current && liveness.can_takeover(&run_caller, unclaimed_ts) {
                    is_orphan = true;
                    disp = format!("[接盘]{base_disp}");
                } else if is_adoptable {
                    adopt_only = true;
                } else {
                    continue;
                }
            }

            let mut record_adoptable = || {
                if is_adoptable && adoptable_seen.insert(rid.clone()) {
                    adoptable_runs.push((
                        base_disp.clone(),
                        rid.clone(),
                        repo.repo_root.clone(),
                        repo.is_current,
                    ));
                }
            };

            // 1) Questioning runs -> Tier 1
            if is_question {
                record_adoptable();
                if !adopt_only {
                    covered_run_ids.insert(rid.clone());
                    out_items.push(PendingItem {
                        priority_tier: 1,
                        stall_score: 0,
                        blocked_downstream: Vec::new(),
                        wait_secs,
                        is_orphan,
                        hook_kind: "question",
                        category: "question",
                        category_label: "提问",
                        target_id: rid.clone(),
                        display_name: disp.clone(),
                        repo_root: repo.repo_root.clone(),
                        is_current: repo.is_current,
                        status_text: "等你决定".to_string(),
                        work_id_for_cooldown: None,
                        template: TemplateKind::Question {
                            name: disp,
                            run: rid,
                        },
                    });
                }
                continue;
            }

            // 2) Live runs -> Tier 4 (collected for waiter check)
            if matches!(st.as_str(), "running" | "waiting" | "starting" | "queued" | "scouting") {
                record_adoptable();
                if !adopt_only {
                    if repo.is_current {
                        live_current.push((disp, rid, obj, repo.repo_root.clone()));
                    } else {
                        live_other.push((disp, rid, obj, repo.repo_root.clone()));
                    }
                }
                continue;
            }

            // 3) Unmerged write delivery (non-zero-change)
            if is_unmerged_write {
                if is_run_being_handled(
                    &rid,
                    &obj,
                    repo.repo_root.as_deref(),
                    &applying_procs,
                    &merging_procs,
                    &active_merge_sources,
                    &gate_cwds,
                    &lane_runs,
                ) {
                    continue;
                }
                if is_deferred_chain {
                    continue;
                }
                record_adoptable();
                if adopt_only {
                    continue;
                }
                let reason = build_unmerged_reason(
                    &obj,
                    is_expired_def_chain,
                    &blocked_downstream,
                    wait_mins,
                );
                let stall_score = (blocked_downstream.len() as u64).saturating_mul(wait_mins);
                let status_text = if reason.is_empty() {
                    "已交付未合入".to_string()
                } else {
                    format!("已交付未合入（{reason}）")
                };
                if !blocked_downstream.is_empty() {
                    // Tier 2: Stall-chain blocking unmerged run (always supersedes unread delivery)
                    covered_run_ids.insert(rid.clone());
                    out_items.push(PendingItem {
                        priority_tier: 2,
                        stall_score,
                        blocked_downstream,
                        wait_secs,
                        is_orphan,
                        hook_kind: "unmerged",
                        category: "stall_unmerged",
                        category_label: "卡链未合",
                        target_id: rid.clone(),
                        display_name: disp.clone(),
                        repo_root: repo.repo_root.clone(),
                        is_current: repo.is_current,
                        status_text,
                        work_id_for_cooldown: None,
                        template: TemplateKind::Unmerged {
                            name: disp,
                            run: rid,
                            reason,
                        },
                    });
                } else {
                    // Candidate for Tier 5 (added after unread events so we don't duplicate Tier 3)
                    pending_unmerged_candidates.push(PendingItem {
                        priority_tier: 5,
                        stall_score: 0,
                        blocked_downstream: Vec::new(),
                        wait_secs,
                        is_orphan,
                        hook_kind: "unmerged",
                        category: "unmerged",
                        category_label: "未合入",
                        target_id: rid.clone(),
                        display_name: disp.clone(),
                        repo_root: repo.repo_root.clone(),
                        is_current: repo.is_current,
                        status_text,
                        work_id_for_cooldown: None,
                        template: TemplateKind::Unmerged {
                            name: disp,
                            run: rid,
                            reason,
                        },
                    });
                }
                continue;
            }

            // 4) Other actionable runs (unread answered / unread failed / unread rejected / unread zero-change)
            //    Tracked for `hook pending` fallback when no unread event exists.
            let result_read = !s(&obj, "resultReadAt").trim().is_empty();
            if mode == EvalMode::Pending
                && !adopt_only
                && is_run_pending_actionable(
                    &st,
                    &run_mode,
                    is_applied,
                    is_cleaned,
                    is_question,
                    result_read,
                    false,
                    zero_change,
                )
            {
                let status_text = match st.as_str() {
                    "failed" => "失败未读",
                    "rejected" => "被拒未读",
                    "answered" => "已答复未读",
                    _ => "已交付待看",
                }
                .to_string();
                pending_unread_fallback_runs.push(PendingItem {
                    priority_tier: 3,
                    stall_score: 0,
                    blocked_downstream: Vec::new(),
                    wait_secs,
                    is_orphan,
                    hook_kind: "events",
                    category: "unread_delivery",
                    category_label: "未读交付",
                    target_id: rid.clone(),
                    display_name: disp.clone(),
                    repo_root: repo.repo_root.clone(),
                    is_current: repo.is_current,
                    status_text,
                    work_id_for_cooldown: None,
                    template: TemplateKind::UnreadDelivery {
                        name: disp,
                        run: rid,
                    },
                });
            }
        }
    }

    // ------------------------------------------------------------------
    // 2. Unread alert events past caller's cursor (Tier 3)
    // ------------------------------------------------------------------
    collect_unread_events(
        state,
        cache,
        cwd,
        now,
        current_only,
        &caller_id,
        &cursors_doc,
        &work_by_id,
        &all_runs_by_id,
        &replied_parents,
        &mut liveness,
        &mut covered_run_ids,
        &mut adoptable_runs,
        &mut adoptable_seen,
        &mut verbatim_head_cache,
        &mut out_items,
    );

    // Add Tier 5 unmerged runs that were NOT already emitted as Tier 2 or Tier 3 unread deliveries
    for item in pending_unmerged_candidates {
        if !covered_run_ids.contains(&item.target_id) {
            covered_run_ids.insert(item.target_id.clone());
            out_items.push(item);
        }
    }

    // In `hook pending` mode, include unread actionable runs not already covered by unread events
    if mode == EvalMode::Pending {
        for item in pending_unread_fallback_runs {
            if !covered_run_ids.contains(&item.target_id) {
                covered_run_ids.insert(item.target_id.clone());
                out_items.push(item);
            }
        }
    }

    // ------------------------------------------------------------------
    // 3. Uncovered live runs -> Tier 4 (`waiter`)
    // ------------------------------------------------------------------
    if !live_current.is_empty() || !live_other.is_empty() {
        let waiters = active_waiters(&procs);
        let locked_inodes = read_locked_inodes();
        let mut uncovered: Vec<(String, String, bool, Option<PathBuf>, Value)> = Vec::new();
        for (disp, rid, obj, rroot) in &live_current {
            let rdir_str = s(obj, "dir");
            let rdir = (!rdir_str.is_empty()).then(|| Path::new(rdir_str));
            if !waiter_covers_run(
                rdir,
                rid,
                obj,
                rroot.as_deref(),
                true,
                &waiters,
                locked_inodes.as_ref(),
            ) {
                uncovered.push((
                    disp.clone(),
                    rid.clone(),
                    true,
                    rdir.map(Path::to_path_buf),
                    obj.clone(),
                ));
            }
        }
        for (disp, rid, obj, rroot) in &live_other {
            let rdir_str = s(obj, "dir");
            let rdir = (!rdir_str.is_empty()).then(|| Path::new(rdir_str));
            if !waiter_covers_run(
                rdir,
                rid,
                obj,
                rroot.as_deref(),
                false,
                &waiters,
                locked_inodes.as_ref(),
            ) {
                uncovered.push((
                    disp.clone(),
                    rid.clone(),
                    false,
                    rdir.map(Path::to_path_buf),
                    obj.clone(),
                ));
            }
        }
        if !uncovered.is_empty() {
            uncovered.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
            let any_current = uncovered.iter().any(|(_, _, cur, _, _)| *cur);
            let first_name = uncovered[0].0.clone();
            let count = uncovered.len();
            let run_ids: Vec<String> = uncovered
                .iter()
                .take(8)
                .map(|(_, r, _, _, _)| r.clone())
                .collect();
            let target_id = uncovered[0].1.clone();
            let fastest_eta = fastest_waiter_eta(state, now, &uncovered);
            out_items.push(PendingItem {
                priority_tier: 4,
                stall_score: 0,
                blocked_downstream: Vec::new(),
                wait_secs: 0,
                is_orphan: false,
                hook_kind: "waiter",
                category: "waiter",
                category_label: "在跑无人等",
                target_id,
                display_name: first_name.clone(),
                repo_root: None,
                is_current: any_current,
                status_text: format!("等 {count} 个委派在跑，没有挂等待"),
                work_id_for_cooldown: None,
                template: TemplateKind::Waiter {
                    name: first_name,
                    count,
                    any_current,
                    run_ids,
                    fastest_eta,
                },
            });
        }
    }

    // ------------------------------------------------------------------
    // 4. Adoptable runs from dead same-terminal/same-cwd session -> Tier 6
    // ------------------------------------------------------------------
    for (disp, rid, rroot, is_cur) in adoptable_runs {
        if covered_run_ids.contains(&rid) {
            continue;
        }
        out_items.push(PendingItem {
            priority_tier: 6,
            stall_score: 0,
            blocked_downstream: Vec::new(),
            wait_secs: 0,
            is_orphan: true,
            hook_kind: "adopt",
            category: "adopt",
            category_label: "待接手",
            target_id: rid.clone(),
            display_name: disp.clone(),
            repo_root: rroot,
            is_current: is_cur,
            status_text: "原会话已失效，同终端/同目录可接手".to_string(),
            work_id_for_cooldown: None,
            template: TemplateKind::Adopt {
                name: disp,
                run: rid,
            },
        });
    }

    // ------------------------------------------------------------------
    // 5. Work items:
    //    - Blocking `verify` (>= 30m, never gated by has_idle_slot, penetrates cross-repo/session) -> Tier 2
    //    - Non-blocking `verify` (>= 4h, never gated by has_idle_slot) -> Tier 7
    //    - `ready` with brief (>= 8h, gated by has_idle_slot) -> Tier 7
    //    - `ready` missing brief (>= 1h grace period, gated by has_idle_slot) -> Tier 7
    //    - `blocked` items NEVER alert (only the upstream item blocking them alerts).
    // ------------------------------------------------------------------
    let current_repo = git_toplevel_pure(cwd);
    let current_repo_str = current_repo
        .as_deref()
        .map(|p| norm_path_str(&p.to_string_lossy()))
        .unwrap_or_default();
    let idle_slot = has_idle_slot(state, current_repo.as_deref(), &repo_items, now);
    let remind_map = if mode == EvalMode::Check {
        load_work_remind(state)
    } else {
        BTreeMap::new()
    };
    let caller_key = if caller_id.is_empty() {
        "default"
    } else {
        caller_id.as_str()
    };

    for item in &work_items {
        if item.owner.is_some() || item.id.trim().is_empty() {
            continue;
        }
        if !matches!(item.state.as_str(), "ready" | "verify") {
            continue;
        }
        let item_repo_norm = norm_path_str(&item.repo);
        let item_is_current = current_repo_str.is_empty()
            || item_repo_norm.is_empty()
            || item_repo_norm == current_repo_str;
        let item_repo_path =
            (!item.repo.trim().is_empty()).then(|| PathBuf::from(item.repo.trim()));

        let since_raw = if !item.state_since.trim().is_empty() {
            item.state_since.trim()
        } else {
            item.created_at.trim()
        };
        let since_ts = iso_epoch(since_raw);
        let age_secs = since_ts.map_or(0, |ts| (now - ts).max(0));
        let wait_mins = (age_secs / 60) as u64;

        let created_raw = if !item.created_at.trim().is_empty() {
            item.created_at.trim()
        } else {
            item.state_since.trim()
        };
        let created_age_secs = iso_epoch(created_raw).map_or(0, |ts| (now - ts).max(0));

        if mode == EvalMode::Check {
            if let Some(&last_rem) = remind_map.get(&item.id).and_then(|m| m.get(caller_key)) {
                if (now - last_rem) < cooldown_secs {
                    continue;
                }
            }
        }

        let item_caller = item.caller.trim();
        let title_raw = if item.title.trim().is_empty() {
            item.id.clone()
        } else {
            item.title.replace(['\n', '\r'], " ").trim().to_string()
        };

        if item.state == "verify" {
            let blocked_downstream =
                work::blocked_downstream_in(&work_items, &[], std::slice::from_ref(&item.id));
            let is_blocking = !blocked_downstream.is_empty();
            let threshold_secs = if is_blocking {
                stall_verify_secs
            } else {
                verify_stale_secs
            };
            if age_secs < threshold_secs {
                continue;
            }
            if !is_blocking && current_only && !current_repo_str.is_empty() && !item_is_current {
                continue;
            }
            let caller_dead = !item_caller.is_empty() && !liveness.is_alive(item_caller);
            let mut is_orphan = false;
            if !item_caller.is_empty() && item_caller != caller_id {
                let penetrates = is_blocking && (caller_dead || age_secs >= stall_penetrate_secs);
                if penetrates {
                    is_orphan = caller_dead;
                } else if liveness.can_takeover(item_caller, since_ts) {
                    is_orphan = true;
                } else {
                    continue;
                }
            }
            let stall_score = (blocked_downstream.len() as u64).saturating_mul(wait_mins);
            let (priority_tier, category, category_label, status_text) = if is_blocking {
                let preview = format_downstream_preview(&blocked_downstream);
                (
                    2,
                    "stall_verify",
                    "卡链待闭环",
                    format!(
                        "待闭环 {} 分钟（挡 {} 个下游：{preview}）",
                        wait_mins,
                        blocked_downstream.len()
                    ),
                )
            } else {
                (
                    7,
                    "work_verify",
                    "待闭环",
                    format!("待闭环 {} 小时", age_secs / 3600),
                )
            };
            out_items.push(PendingItem {
                priority_tier,
                stall_score,
                blocked_downstream: blocked_downstream.clone(),
                wait_secs: age_secs,
                is_orphan,
                hook_kind: "work",
                category,
                category_label,
                target_id: item.id.clone(),
                display_name: title_raw.clone(),
                repo_root: item_repo_path,
                is_current: item_is_current,
                status_text,
                work_id_for_cooldown: Some(item.id.clone()),
                template: TemplateKind::WorkVerify {
                    wid: item.id.clone(),
                    title: title_raw,
                    wait_secs: age_secs,
                    blocked_downstream,
                },
            });
            continue;
        }

        // `ready` items: gated by `has_idle_slot`
        if !idle_slot {
            continue;
        }
        if current_only && !current_repo_str.is_empty() && !item_is_current {
            continue;
        }
        let mut is_orphan = false;
        if !item_caller.is_empty() && item_caller != caller_id {
            if !liveness.can_takeover(item_caller, since_ts) {
                continue;
            }
            is_orphan = true;
        }

        let brief = item.brief.trim();
        if brief.is_empty() {
            // Requirement D: 1h grace period for missing-brief ready items!
            if created_age_secs < missing_brief_grace_secs {
                continue;
            }
            out_items.push(PendingItem {
                priority_tier: 7,
                stall_score: 0,
                blocked_downstream: Vec::new(),
                wait_secs: created_age_secs,
                is_orphan,
                hook_kind: "work",
                category: "work_missing_brief",
                category_label: "缺说明",
                target_id: item.id.clone(),
                display_name: title_raw.clone(),
                repo_root: item_repo_path,
                is_current: item_is_current,
                status_text: "没有任务说明".to_string(),
                work_id_for_cooldown: Some(item.id.clone()),
                template: TemplateKind::WorkMissingBrief {
                    wid: item.id.clone(),
                    title: title_raw,
                },
            });
        } else if age_secs >= ready_stale_secs {
            let hours = (age_secs / 3600) as u64;
            out_items.push(PendingItem {
                priority_tier: 7,
                stall_score: 0,
                blocked_downstream: Vec::new(),
                wait_secs: age_secs,
                is_orphan,
                hook_kind: "work",
                category: "work_ready",
                category_label: "就绪未派",
                target_id: item.id.clone(),
                display_name: title_raw.clone(),
                repo_root: item_repo_path,
                is_current: item_is_current,
                status_text: format!("就绪 {hours} 小时没派"),
                work_id_for_cooldown: Some(item.id.clone()),
                template: TemplateKind::WorkReady {
                    wid: item.id.clone(),
                    title: title_raw,
                    hours,
                    brief: brief.to_string(),
                },
            });
        }
    }


    // ------------------------------------------------------------------
    // 6. Deterministic priority ordering
    //    Tier 1: question
    //    Tier 2: stall-chain blocking items (highest stall_score first)
    //    Tier 3: unread deliveries & unread alert events
    //    Tier 4: uncovered live runs (waiter)
    //    Tier 5: non-blocking unmerged write runs
    //    Tier 6: adoptable dead-caller runs
    //    Tier 7: work items (oldest first)
    // ------------------------------------------------------------------
    out_items.sort_by(|a, b| {
        a.priority_tier
            .cmp(&b.priority_tier)
            .then_with(|| b.stall_score.cmp(&a.stall_score))
            .then_with(|| b.blocked_downstream.len().cmp(&a.blocked_downstream.len()))
            .then_with(|| a.is_orphan.cmp(&b.is_orphan))
            .then_with(|| b.wait_secs.cmp(&a.wait_secs))
            .then_with(|| a.target_id.cmp(&b.target_id))
    });

    EvalOutput {
        items: out_items,
        pending_runs,
        controller_pending_runs: pending_split.active_pending,
        stale_pending_runs: pending_split.stale_pending,
        caller_id,
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_unread_events(
    state: &Path,
    cache: &Path,
    cwd: &str,
    now: i64,
    current_only: bool,
    caller_id: &str,
    cursors_doc: &Value,
    work_by_id: &HashMap<String, &WorkItem>,
    all_runs_by_id: &HashMap<String, Value>,
    replied_parents: &HashSet<String>,
    liveness: &mut CallerLivenessChecker,
    covered_run_ids: &mut HashSet<String>,
    adoptable_runs: &mut Vec<(String, String, Option<PathBuf>, bool)>,
    adoptable_seen: &mut HashSet<String>,
    verbatim_head_cache: &mut HashMap<String, bool>,
    out_items: &mut Vec<PendingItem>,
) {
    let events_file = state.join("events.jsonl");
    let Ok(fh) = OpenOptions::new().read(true).open(&events_file) else {
        return;
    };

    let entry = if !caller_id.is_empty() && cursors_doc["callers"][caller_id].is_object() {
        &cursors_doc["callers"][caller_id]
    } else if cursors_doc["default"].is_object() {
        &cursors_doc["default"]
    } else {
        &Value::Null
    };
    let cursor_seq = entry["seq"].as_u64().unwrap_or(0);
    let mut read_runs: HashMap<String, u64> = HashMap::new();
    if let Some(rr) = entry["readRuns"].as_object() {
        for (k, v) in rr {
            if let Some(seq) = v.as_u64() {
                read_runs.insert(k.clone(), seq);
            }
        }
    }

    let current_repo_str = git_toplevel_pure(cwd).map(|p| norm_path_str(&p.to_string_lossy()));

    let mut candidates: Vec<(u64, Value, bool)> = Vec::new();
    let mut resolving = ResolvingSeqs::default();

    for line in BufReader::new(fh).lines().map_while(Result::ok) {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(t) else {
            continue;
        };
        if !ev.is_object() {
            continue;
        }
        let Some(seq) = ev["seq"].as_u64() else {
            continue;
        };
        if seq <= cursor_seq {
            continue;
        }
        let kind = s(&ev, "kind");
        let pl = &ev["payload"];
        let run_id = s(&ev, "run").trim();

        match kind {
            "applied" => ResolvingSeqs::bump(&mut resolving.applied, run_id, seq),
            "cleaned" => ResolvingSeqs::bump(&mut resolving.cleaned, run_id, seq),
            "stopped" => ResolvingSeqs::bump(&mut resolving.stopped, run_id, seq),
            "delivered" | "rejected" | "failed" | "question" => {
                ResolvingSeqs::bump(&mut resolving.terminal, run_id, seq);
            }
            "premerge_ready" => ResolvingSeqs::bump(&mut resolving.premerge_ready, run_id, seq),
            "review_verdict" if s(pl, "verdict") == "PASS" => {
                ResolvingSeqs::bump(&mut resolving.review_pass, run_id, seq);
            }
            "started" => {
                let par = s(pl, "parent").trim();
                if !par.is_empty() {
                    ResolvingSeqs::bump(&mut resolving.replied, par, seq);
                }
            }
            _ => {}
        }

        let is_alert = UNREAD_ALERT_KINDS.contains(&kind)
            || (kind == "review_verdict" && s(pl, "verdict") == "NEEDS_CONTROLLER");
        if !is_alert {
            continue;
        }
        let ev_repo_norm = norm_path_str(s(&ev, "repo"));
        let ev_is_current = current_repo_str
            .as_deref()
            .is_some_and(|cur| !ev_repo_norm.is_empty() && ev_repo_norm == cur);
        if current_only && current_repo_str.is_some() && !ev_is_current {
            continue;
        }
        if !run_id.is_empty() && !ev_is_current {
            if let Some(run_obj) = all_runs_by_id.get(run_id) {
                if !run_belongs_to_scope(run_obj, false, caller_id) {
                    let rc = get_run_caller_readonly(run_obj);
                    if !(liveness.is_adoptable(&rc) && liveness.same_terminal(&rc)) {
                        continue;
                    }
                }
            }
        }
        if !run_id.is_empty() && read_runs.get(run_id).is_some_and(|&rseq| seq <= rseq) {
            continue;
        }
        candidates.push((seq, ev, ev_is_current));
    }

    if candidates.is_empty() {
        return;
    }

    let need_queue = candidates
        .iter()
        .any(|(_, ev, _)| s(ev, "kind") == "queue_paused");
    let queue_items_by_name = if need_queue {
        load_queue_items_by_name(state)
    } else {
        None
    };

    let mut seen_event_keys: HashSet<String> = HashSet::new();

    for (seq, ev, ev_is_current) in candidates {
        if is_alert_event_stale(
            &ev,
            seq,
            now,
            work_by_id,
            all_runs_by_id,
            replied_parents,
            &resolving,
            queue_items_by_name.as_ref(),
            cache,
            verbatim_head_cache,
        ) {
            continue;
        }
        let kind = s(&ev, "kind");
        let pl = &ev["payload"];
        let rid = s(&ev, "run").trim().to_string();
        let ev_repo_path = {
            let r = s(&ev, "repo").trim();
            (!r.is_empty()).then(|| PathBuf::from(r))
        };
        let ev_ts = iso_epoch(s(&ev, "at"));

        let mut is_orphan = false;
        if kind == "work_stale" {
            let wid = s(pl, "workId").trim();
            if let Some(w) = (!wid.is_empty()).then(|| work_by_id.get(wid)).flatten() {
                let w_caller = w.caller.trim();
                if !w_caller.is_empty() && w_caller != caller_id {
                    let since_ts = iso_epoch(w.state_since.trim()).or(ev_ts);
                    if liveness.can_takeover(w_caller, since_ts) {
                        is_orphan = true;
                    } else {
                        continue;
                    }
                }
            }
        } else if !rid.is_empty() && !matches!(kind, "work_ready" | "work_defer_expired") {
            if let Some(run_obj) = all_runs_by_id.get(&rid) {
                let run_caller = get_run_caller_readonly(run_obj);
                if !run_caller.is_empty() && run_caller != caller_id {
                    let adoptable = liveness.is_adoptable(&run_caller);
                    if !ev_is_current && !(adoptable && liveness.same_terminal(&run_caller)) {
                        continue;
                    }
                    if adoptable && adoptable_seen.insert(rid.clone()) {
                        let raw_name = {
                            let n = s(pl, "name");
                            if !n.is_empty() {
                                n
                            } else {
                                let rn = s(run_obj, "name");
                                if !rn.is_empty() {
                                    rn
                                } else {
                                    rid.as_str()
                                }
                            }
                        };
                        adoptable_runs.push((
                            strip_reply_prefixes(raw_name),
                            rid.clone(),
                            ev_repo_path.clone(),
                            ev_is_current,
                        ));
                    }
                    let ts = ev_ts.or_else(|| get_run_unclaimed_ts(run_obj));
                    if ev_is_current && liveness.can_takeover(&run_caller, ts) {
                        is_orphan = true;
                    } else {
                        continue;
                    }
                }
            }
        }

        // Deduplicate against runs already emitted in Tier 1 (question) or Tier 2 (stall_unmerged)
        if !rid.is_empty() && covered_run_ids.contains(&rid) {
            continue;
        }

        let wait_secs = ev_ts.map_or(0, |t| (now - t).max(0));
        let raw_name = {
            let n = s(pl, "name");
            if !n.is_empty() {
                n.to_string()
            } else {
                let qn = s(pl, "queueName");
                if !qn.is_empty() {
                    qn.to_string()
                } else if !rid.is_empty() {
                    rid.clone()
                } else {
                    "?".to_string()
                }
            }
        };
        let clean_name = strip_reply_prefixes(&raw_name.replace(['\n', '\r'], " "));
        let repo_label = ev_repo_path
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let base_disp = if ev_is_current || repo_label.is_empty() {
            clean_name.clone()
        } else {
            format!("【{repo_label}】{clean_name}")
        };
        let disp = if is_orphan {
            format!("[接盘]{base_disp}")
        } else {
            base_disp
        };

        if matches!(kind, "delivered" | "failed" | "rejected") && !rid.is_empty() {
            covered_run_ids.insert(rid.clone());
            out_items.push(PendingItem {
                priority_tier: 3,
                stall_score: 0,
                blocked_downstream: Vec::new(),
                wait_secs,
                is_orphan,
                hook_kind: "events",
                category: "unread_delivery",
                category_label: "未读交付",
                target_id: rid.clone(),
                display_name: disp.clone(),
                repo_root: ev_repo_path,
                is_current: ev_is_current,
                status_text: "已交付待看".to_string(),
                work_id_for_cooldown: None,
                template: TemplateKind::UnreadDelivery {
                    name: disp,
                    run: rid,
                },
            });
            continue;
        }

        if kind == "question" && !rid.is_empty() {
            covered_run_ids.insert(rid.clone());
            out_items.push(PendingItem {
                priority_tier: 1,
                stall_score: 0,
                blocked_downstream: Vec::new(),
                wait_secs,
                is_orphan,
                hook_kind: "question",
                category: "question",
                category_label: "提问",
                target_id: rid.clone(),
                display_name: disp.clone(),
                repo_root: ev_repo_path,
                is_current: ev_is_current,
                status_text: "等你决定".to_string(),
                work_id_for_cooldown: None,
                template: TemplateKind::Question {
                    name: disp,
                    run: rid,
                },
            });
            continue;
        }

        // Other alert events (`work_ready`, `queue_paused`, `needs_attention`, `review_verdict`, ...)
        let wid = s(pl, "workId").trim();
        let label = if !wid.is_empty() {
            format!("{wid}:{}", raw_name.replace(['\n', '\r'], " "))
        } else {
            raw_name.replace(['\n', '\r'], " ")
        };
        let dedup_key = format!("{kind}:{rid}:{wid}:{label}");
        if !seen_event_keys.insert(dedup_key) {
            continue;
        }
        let prefix = if is_orphan { "[接盘]" } else { "" };
        let preview = format!("{prefix}{}:{label}", kind_label(kind));
        let target_id = if !rid.is_empty() {
            rid
        } else if !wid.is_empty() {
            wid.to_string()
        } else {
            label.clone()
        };
        out_items.push(PendingItem {
            priority_tier: 3,
            stall_score: 0,
            blocked_downstream: Vec::new(),
            wait_secs,
            is_orphan,
            hook_kind: "events",
            category: "unread_event",
            category_label: "未读事件",
            target_id,
            display_name: label,
            repo_root: ev_repo_path,
            is_current: ev_is_current,
            status_text: preview.clone(),
            work_id_for_cooldown: None,
            template: TemplateKind::UnreadEvent {
                preview,
                cursor_seq,
            },
        });
    }
}

fn emit_allow(format: HookFormat, guard: Option<&str>) {
    match format {
        HookFormat::Claude => {}
        HookFormat::Agy => println!("{{}}"),
        HookFormat::Json => {
            let mut doc = json!({
                "decision": "allow",
                "reason": "",
                "remaining": 0,
                "total": 0,
            });
            if let Some(g) = guard {
                doc["guard"] = json!(g);
            }
            println!("{doc}");
        }
    }
}

/// CLI entrypoint for `delegate hook check [--format claude|agy|json] [--cwd PATH]`.
pub fn check_cmd(args: &[String]) -> Res<i32> {
    let mut format = HookFormat::Claude;
    let mut cwd_override: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (key, inline) = a
            .split_once('=')
            .map_or((a.as_str(), None), |(k, v)| (k, Some(v)));
        match key {
            "--format" => {
                let raw = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --format: expected claude|agy|json")?
                    }
                };
                format = match raw.as_str() {
                    "claude" => HookFormat::Claude,
                    "agy" => HookFormat::Agy,
                    "json" => HookFormat::Json,
                    other => {
                        return Err(format!(
                            "invalid --format '{other}' (expected: claude, agy, json)"
                        ));
                    }
                };
            }
            "--cwd" => {
                let raw = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --cwd: expected a path")?
                    }
                };
                cwd_override = Some(raw);
            }
            "--json" => format = HookFormat::Json,
            _ => {
                return Err(unrecognized_arg(
                    a,
                    &["--format", "--cwd", "--json"],
                    "delegate hook check [--format claude|agy|json] [--cwd PATH] [--json]",
                ));
            }
        }
        i += 1;
    }

    // Read stdin payload (empty stdin treated as `{}`; malformed non-empty JSON fails open)
    let mut stdin_raw = String::new();
    let _ = io::stdin().read_to_string(&mut stdin_raw);
    let trimmed = stdin_raw.trim();
    let payload: Value = if trimmed.is_empty() {
        json!({})
    } else {
        match serde_json::from_str::<Value>(trimmed) {
            Ok(v) if v.is_object() => v,
            _ => {
                emit_allow(format, Some("invalid_payload"));
                return Ok(0);
            }
        }
    };

    if let Some(guard) = check_entry_guards(&payload, format) {
        emit_allow(format, Some(guard));
        return Ok(0);
    }

    let cwd = cwd_override
        .or_else(|| {
            let p = s(&payload, "cwd").trim();
            (!p.is_empty()).then(|| p.to_string())
        })
        .unwrap_or_else(|| {
            env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        });

    let state = state_dir();
    let cache = cache_dir();
    let now = epoch() as i64;
    let out = evaluate_core(&state, &cache, &payload, &cwd, now, EvalMode::Check);
    let disk_health = crate::disk::inspect_readonly(&state, &cache, now);
    let disk_note = disk_health.format_stop_reminder();

    let Some(top) = out.items.first() else {
        match format {
            HookFormat::Claude => {
                if let Some(ref note) = disk_note {
                    println!("{}", json!({ "systemMessage": note }));
                }
            }
            HookFormat::Agy => {
                if let Some(ref note) = disk_note {
                    println!("{}", json!({ "systemMessage": note }));
                } else {
                    println!("{{}}");
                }
            }
            HookFormat::Json => {
                let mut doc = json!({
                    "decision": "allow",
                    "reason": "",
                    "remaining": 0,
                    "total": 0,
                    "diskHealth": disk_health.to_json(),
                });
                if let Some(ref note) = disk_note {
                    doc["diskReminder"] = json!(note);
                }
                println!("{doc}");
            }
        }
        return Ok(0);
    };

    let remaining = out.items.len().saturating_sub(1);
    let mut reason = top.format_block_reason(remaining, format);
    if let Some(ref note) = disk_note {
        reason.push_str(" | ");
        reason.push_str(note);
    }

    // Requirement E: ONLY the single displayed work item is recorded in `work-remind.json`.
    if let Some(ref wid) = top.work_id_for_cooldown {
        let mut remind_map = load_work_remind(&state);
        let caller_key = if out.caller_id.is_empty() {
            "default".to_string()
        } else {
            out.caller_id.clone()
        };
        remind_map
            .entry(wid.clone())
            .or_default()
            .insert(caller_key, now);
        save_work_remind(&state, &remind_map, now);
    }

    append_stop_log(&state, top.stop_log_record(format), now);

    match format {
        HookFormat::Claude => {
            println!("{}", json!({ "decision": "block", "reason": reason }));
        }
        HookFormat::Agy => {
            println!("{}", json!({ "decision": "continue", "reason": reason }));
        }
        HookFormat::Json => {
            let mut doc = json!({
                "decision": "block",
                "reason": reason,
                "item": top.to_json(format),
                "remaining": remaining,
                "total": out.items.len(),
                "pendingRunCount": out.pending_runs.len(),
                "controllerPendingCount": out.controller_pending_runs.len(),
                "stalePendingCount": out.stale_pending_runs.len(),
                "diskHealth": disk_health.to_json(),
            });
            if let Some(ref note) = disk_note {
                doc["diskReminder"] = json!(note);
            }
            println!("{doc}");
        }
    }
    Ok(0)
}

/// CLI entrypoint for `delegate hook pending [--json] [--cwd PATH]`.
/// Strictly read-only, zero locks, zero git, zero subprocesses.
pub fn pending_cmd(args: &[String]) -> Res<i32> {
    let mut json_mode = crate::output::json_enabled();
    let mut cwd_override: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (key, inline) = a
            .split_once('=')
            .map_or((a.as_str(), None), |(k, v)| (k, Some(v)));
        match key {
            "--json" => json_mode = true,
            "--cwd" => {
                let raw = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --cwd: expected a path")?
                    }
                };
                cwd_override = Some(raw);
            }
            _ => {
                return Err(unrecognized_arg(
                    a,
                    &["--json", "--cwd"],
                    "delegate hook pending [--json] [--cwd PATH]",
                ));
            }
        }
        i += 1;
    }

    let cwd = cwd_override.unwrap_or_else(|| {
        env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let state = state_dir();
    let cache = cache_dir();
    let now = epoch() as i64;
    let payload = json!({ "cwd": cwd });
    let out = evaluate_core(&state, &cache, &payload, &cwd, now, EvalMode::Pending);
    let disk_health = crate::disk::inspect_readonly(&state, &cache, now);

    if json_mode {
        let items_json: Vec<Value> = out
            .items
            .iter()
            .map(|it| it.to_json(HookFormat::Json))
            .collect();
        println!(
            "{}",
            json!({
                "total": out.items.len(),
                "pendingRunCount": out.pending_runs.len(),
                "pendingRuns": out.pending_runs,
                "controllerPendingCount": out.controller_pending_runs.len(),
                "controllerPendingRuns": out.controller_pending_runs,
                "stalePendingCount": out.stale_pending_runs.len(),
                "stalePendingRuns": out.stale_pending_runs,
                "items": items_json,
                "diskHealth": disk_health.to_json(),
            })
        );
    } else if out.items.is_empty() {
        println!("当前没有待处理项。");
        if let Some(note) = disk_health.format_stop_reminder() {
            println!("{note}");
        }
    } else {
        println!("待处理 {} 件（按优先级）：", out.items.len());
        for item in &out.items {
            println!("{}", item.format_pending_line());
        }
        if let Some(note) = disk_health.format_stop_reminder() {
            println!("{note}");
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_reply_prefixes_handles_multibyte_names() {
        assert_eq!(strip_reply_prefixes("暂缓到期时间测试"), "暂缓到期时间测试");
        assert_eq!(strip_reply_prefixes("reply to reply to 青禾"), "青禾");
        assert_eq!(strip_reply_prefixes("Reply To eta-1"), "eta-1");
    }

    #[test]
    fn shared_pending_classification_fixture_matches_hook_kernel() {
        let fixture_raw = include_str!("../../../tests/fixtures/pending_classification.json");
        let doc: Value = serde_json::from_str(fixture_raw).expect("valid shared fixture json");
        let expected_count = doc["expectedPendingCount"].as_u64().unwrap() as usize;
        let mut expected_pending: Vec<String> = doc["expectedPendingRuns"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        expected_pending.sort();

        let runs: Vec<Value> = doc["runs"].as_array().unwrap().clone();
        let actual_pending = classify_pending_runs(&runs);
        assert_eq!(actual_pending.len(), expected_count);
        assert_eq!(actual_pending, expected_pending);
    }

    #[test]
    fn clip_title_truncates_over_40_chars_with_ellipsis() {
        let exact_40 = "一".repeat(40);
        assert_eq!(clip_title(&exact_40, TITLE_MAX_CHARS), exact_40);

        let over_40 = "一".repeat(45);
        let clipped = clip_title(&over_40, TITLE_MAX_CHARS);
        assert_eq!(clipped, format!("{}…", "一".repeat(40)));
    }

    #[test]
    fn entry_guards_bypass_colleagues_loops_and_non_no_tool_call_termination() {
        let empty = json!({});
        assert_eq!(
            check_entry_guards_with(&empty, HookFormat::Claude, |k| {
                (k == "PI_DELEGATE_ACTIVE").then(|| "1".into())
            }),
            Some("colleague")
        );
        assert_eq!(
            check_entry_guards_with(&empty, HookFormat::Agy, |k| {
                (k == "DELEGATE_AGENT").then(|| "aia".into())
            }),
            Some("colleague")
        );
        assert_eq!(
            check_entry_guards_with(&empty, HookFormat::Agy, |k| {
                (k == "DELEGATE_RUN_DIR").then(|| "/tmp/run".into())
            }),
            Some("colleague")
        );
        assert_eq!(
            check_entry_guards_with(
                &json!({"stop_hook_active": true}),
                HookFormat::Claude,
                |_| { None }
            ),
            Some("loop")
        );
        assert_eq!(
            check_entry_guards_with(
                &json!({"continuationActive": true}),
                HookFormat::Agy,
                |_| None
            ),
            Some("loop")
        );
        assert_eq!(
            check_entry_guards_with(
                &json!({"terminationReason": "USER_CANCELED"}),
                HookFormat::Agy,
                |_| None
            ),
            Some("termination")
        );
        assert_eq!(
            check_entry_guards_with(
                &json!({"terminationReason": "NO_TOOL_CALL"}),
                HookFormat::Agy,
                |_| None
            ),
            None
        );
    }

    #[test]
    fn caller_resolution_priority_matches_spec() {
        let payload = json!({
            "session_id": "claude-payload",
            "conversationId": "agy-payload",
        });
        assert_eq!(
            resolve_caller_with(&payload, |k| match k {
                "DELEGATE_CALLER" => Some("explicit".into()),
                "CLAUDE_CODE_SESSION_ID" => Some("claude-env".into()),
                _ => None,
            }),
            ("explicit".into(), "DELEGATE_CALLER".into())
        );
        assert_eq!(
            resolve_caller_with(&payload, |k| {
                (k == "CLAUDE_CODE_SESSION_ID").then(|| "claude-env".into())
            }),
            ("claude-payload".into(), "CLAUDE_CODE_SESSION_ID".into())
        );
        assert_eq!(
            resolve_caller_with(&json!({"conversationId": "agy-payload"}), |k| {
                (k == "CLAUDE_CODE_SESSION_ID").then(|| "claude-env".into())
            }),
            ("agy-payload".into(), "ANTIGRAVITY_CONVERSATION_ID".into())
        );
        assert_eq!(
            resolve_caller_with(&json!({}), |k| match k {
                "ANTIGRAVITY_AGENT" => Some("1".into()),
                "ANTIGRAVITY_CONVERSATION_ID" => Some("agy-env".into()),
                "CLAUDE_CODE_SESSION_ID" => Some("claude-env".into()),
                _ => None,
            }),
            ("agy-env".into(), "ANTIGRAVITY_CONVERSATION_ID".into())
        );
    }

    #[test]
    fn all_seven_verbatim_templates_and_agy_waiter_match_spec() {
        let make = |template: TemplateKind| PendingItem {
            priority_tier: 1,
            stall_score: 0,
            blocked_downstream: Vec::new(),
            wait_secs: 0,
            is_orphan: false,
            hook_kind: "test",
            category: "test",
            category_label: "测",
            target_id: "x".into(),
            display_name: "x".into(),
            repo_root: None,
            is_current: true,
            status_text: String::new(),
            work_id_for_cooldown: None,
            template,
        };

        let t1 = make(TemplateKind::UnreadDelivery {
            name: "feat-a".into(),
            run: "r1".into(),
        });
        assert_eq!(
            t1.format_block_reason(0, HookFormat::Claude),
            "feat-a 已交付待看。下一步：`delegate card r1`"
        );
        assert_eq!(
            t1.format_block_reason(2, HookFormat::Claude),
            "feat-a 已交付待看。下一步：`delegate card r1`（另有 2 件：`delegate hook pending`）"
        );

        let t2 = make(TemplateKind::Question {
            name: "feat-q".into(),
            run: "r2".into(),
        });
        assert_eq!(
            t2.format_block_reason(1, HookFormat::Claude),
            "feat-q 在等你决定。下一步：`delegate result r2`，再 `delegate reply r2 \"<决定>\"`（另有 1 件：`delegate hook pending`）"
        );

        let t3 = make(TemplateKind::Waiter {
            name: "job-1".into(),
            count: 3,
            any_current: true,
            run_ids: vec!["r3".into()],
            fastest_eta: None,
        });
        assert_eq!(
            t3.format_block_reason(0, HookFormat::Claude),
            "job-1 等 3 个委派在跑，没有挂等待。下一步：后台跑 `delegate wait --any`"
        );
        assert_eq!(
            t3.format_block_reason(0, HookFormat::Agy),
            "job-1 等 3 个委派在跑，没有挂等待。下一步：run_command 设 WaitMsBeforeAsync=500 后台跑 `delegate wait --any`"
        );
        let t3_eta = make(TemplateKind::Waiter {
            name: "job-1".into(),
            count: 3,
            any_current: true,
            run_ids: vec!["r3".into()],
            fastest_eta: Some("02:52 交付".into()),
        });
        assert_eq!(
            t3_eta.format_block_reason(0, HookFormat::Claude),
            "job-1 等 3 个委派在跑，没有挂等待。下一步：后台跑 `delegate wait --any`（最快预计 02:52 交付）"
        );
        assert_eq!(
            t3_eta.format_block_reason(2, HookFormat::Claude),
            "job-1 等 3 个委派在跑，没有挂等待。下一步：后台跑 `delegate wait --any`（最快预计 02:52 交付；另有 2 件：`delegate hook pending`）"
        );

        let t4_plain = make(TemplateKind::Unmerged {
            name: "feat-u".into(),
            run: "r4".into(),
            reason: String::new(),
        });
        assert_eq!(
            t4_plain.format_block_reason(0, HookFormat::Claude),
            "feat-u 已交付未合入。下一步：`delegate card r4`；合不了用 `delegate work defer r4 --until 4h --reason \"…\"`"
        );

        let t4_reason = make(TemplateKind::Unmerged {
            name: "feat-u".into(),
            run: "r4".into(),
            reason: "预合并失败：generate".into(),
        });
        assert_eq!(
            t4_reason.format_block_reason(3, HookFormat::Claude),
            "feat-u 已交付未合入（预合并失败：generate）。下一步：`delegate card r4`；合不了用 `delegate work defer r4 --until 4h --reason \"…\"`（另有 3 件：`delegate hook pending`）"
        );

        let t5 = make(TemplateKind::WorkReady {
            wid: "W012".into(),
            title: "实现增量索引".into(),
            hours: 9,
            brief: "docs/w012.md".into(),
        });
        assert_eq!(
            t5.format_block_reason(0, HookFormat::Claude),
            "[W012] 实现增量索引 就绪 9 小时没派。下一步：`delegate start --work W012 --prompt-file docs/w012.md`"
        );

        let t6 = make(TemplateKind::WorkMissingBrief {
            wid: "W059".into(),
            title: "补测试说明".into(),
        });
        assert_eq!(
            t6.format_block_reason(0, HookFormat::Claude),
            "[W059] 补测试说明 没有任务说明。下一步：`delegate work wait W059 --brief <文件>`"
        );

        let t7 = make(TemplateKind::WorkVerify {
            wid: "W054".into(),
            title: "真机验过".into(),
            wait_secs: 5 * 3600,
            blocked_downstream: Vec::new(),
        });
        assert_eq!(
            t7.format_block_reason(0, HookFormat::Claude),
            "[W054] 真机验过 待闭环 5 小时。下一步：`delegate work close W054 --evidence \"…\"`"
        );
    }

    #[test]
    fn replied_question_with_result_read_is_not_pending() {
        let runs = vec![
            json!({
                "run": "scout-replied",
                "state": "answered",
                "mode": "read-only",
                "question": true,
                "resultReadAt": "2026-11-27T05:00:00Z",
                "repliedAt": "2026-11-27T05:00:01Z",
            }),
            json!({
                "run": "scout-unreplied",
                "state": "answered",
                "mode": "read-only",
                "question": true,
                "resultReadAt": "2026-11-27T05:00:00Z",
            }),
        ];
        assert_eq!(
            classify_pending_runs(&runs),
            vec!["scout-unreplied".to_string()]
        );
    }
}
