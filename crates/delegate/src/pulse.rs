#![forbid(unsafe_code)]
//! Single-source-of-truth global pulse counters and active alerts shared by
//! `delegate snapshot --json`, `delegate status`, `delegate work`, `hook pending`, and `delegate-tui`.
//! Strictly read-only, zero disk writes, zero subprocesses; quota alerts are advisory only
//! and NEVER trigger automatic account switching.

use crate::agy_accounts;
use crate::common::*;
use crate::hook_check;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Pending runs inactive for more than 24h without an open work item are classified as stale.
pub const STALE_PENDING_SECS: i64 = 24 * 3600;
/// Advisory-only remaining quota percentage reminder threshold (never auto-switches accounts).
pub const QUOTA_LOW_REMINDER_PCT: f64 = 15.0;
/// Controller wait thresholds before raising a P1 alert banner.
pub const QUESTION_STALL_SECS: i64 = 10 * 60;
pub const CONTROLLER_STALL_SECS: i64 = 30 * 60;
pub const OWNER_STALL_SECS: i64 = 2 * 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlertLevel {
    P0,
    P1,
}

impl AlertLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            AlertLevel::P0 => "P0",
            AlertLevel::P1 => "P1",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveAlert {
    pub level: AlertLevel,
    pub kind: String,
    pub label: String,
    pub message: String,
    pub target_tab: u8,
    pub target_id: Option<String>,
}

impl ActiveAlert {
    pub fn to_json(&self) -> Value {
        let mut obj = json!({
            "level": self.level.as_str(),
            "kind": self.kind,
            "label": self.label,
            "message": self.message,
            "targetTab": self.target_tab,
        });
        if let Some(tid) = &self.target_id {
            obj["targetId"] = json!(tid);
        }
        obj
    }
}

/// Global single-source-of-truth counters emitted in `snapshot["pulse"]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotPulse {
    pub owner_open: usize,
    pub controller_pending: usize,
    pub stale_pending: usize,
    pub running_runs: usize,
    pub queued_runs: usize,
    pub work_active_pool: usize,
    pub work_deferred: usize,
    pub work_timed: usize,
    pub merged_today: usize,
    pub pending_has_urgent: bool,
    pub quota_alert: bool,
    pub p0_alert: bool,
}

impl SnapshotPulse {
    pub fn to_json(&self) -> Value {
        json!({
            "ownerOpen": self.owner_open,
            "controllerPending": self.controller_pending,
            "stalePending": self.stale_pending,
            "runningRuns": self.running_runs,
            "queuedRuns": self.queued_runs,
            "workActivePool": self.work_active_pool,
            "workDeferred": self.work_deferred,
            "workTimed": self.work_timed,
            "mergedToday": self.merged_today,
            "pendingHasUrgent": self.pending_has_urgent,
            "quotaAlert": self.quota_alert,
            "p0Alert": self.p0_alert,
        })
    }
}

/// True when a snapshot work JSON object is a `blocked` item whose unsatisfied conditions
/// are exclusively `after_time:...` timed waits (not upstream work/run/owner dependencies).
pub fn is_pure_timed_wait_work_json(w: &Value) -> bool {
    if s(w, "state") != "blocked" {
        return false;
    }
    if let Some(arr) = w["waitOnStatus"].as_array() {
        let unsatisfied: Vec<&str> = arr
            .iter()
            .filter(|c| !b(c, "satisfied"))
            .map(|c| s(c, "condition"))
            .filter(|c| !c.is_empty())
            .collect();
        if !unsatisfied.is_empty() {
            return unsatisfied.iter().all(|c| c.starts_with("after_time:"));
        }
    }
    if let Some(arr) = w["waitOn"].as_array() {
        let raw: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
        if !raw.is_empty() {
            return raw.iter().all(|c| c.starts_with("after_time:"));
        }
    }
    false
}

/// Mask an account ID (usually an email) into a short recognizable label like `gui…`.
fn mask_account_short(id: &str) -> String {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return "?".to_string();
    }
    let local = trimmed.split('@').next().unwrap_or(trimmed);
    let chars: Vec<char> = local.chars().collect();
    if chars.len() <= 3 {
        local.to_string()
    } else {
        let prefix: String = chars.into_iter().take(3).collect();
        format!("{prefix}…")
    }
}

fn short_pool_name(pool: &str) -> &'static str {
    let low = pool.to_ascii_lowercase();
    if low.contains("gemini") {
        "Gemini"
    } else if low.contains("claude") || low.contains("gpt") {
        "Claude"
    } else {
        "额度"
    }
}

fn short_window_name(win: &str) -> &'static str {
    match win {
        "fiveHour" => "5h",
        _ => "本周",
    }
}

fn format_compact_duration(secs: i64) -> String {
    let s = secs.max(0) as u64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        let h = s / 3600;
        let m = (s % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m:02}m")
        }
    } else {
        let d = s / 86400;
        format!("{d}d")
    }
}

fn run_has_queue(st: &Value) -> bool {
    !s(st, "queueReason").is_empty()
        || st["queueAhead"].as_u64().is_some()
        || st["queue"].is_object()
}

fn run_last_activity_secs(st: &Value, now_ep: i64) -> i64 {
    if let Some(fin_ago) = st["finishedAgoSeconds"].as_i64() {
        return fin_ago.max(0);
    }
    if let Some(fin_ep) = iso_epoch(s(st, "finishedAt")) {
        return (now_ep - fin_ep).max(0);
    }
    if let Some(age) = st["ageSeconds"].as_i64() {
        return age.max(0);
    }
    if let Some(st_ep) = iso_epoch(s(st, "startedAt")) {
        return (now_ep - st_ep).max(0);
    }
    0
}

fn run_display_name(st: &Value) -> String {
    let raw = s(st, "name");
    let stripped = crate::notes::strip_reply_prefixes(raw).0.trim();
    if !stripped.is_empty() {
        stripped.to_string()
    } else {
        s(st, "run").to_string()
    }
}

fn is_run_urgent_failure_or_conflict(st: &Value, applied: bool, cleaned: bool, result_read: bool) -> bool {
    if applied || cleaned {
        return false;
    }
    let state = s(st, "state");
    let has_violation = st["protectViolation"]
        .as_array()
        .is_some_and(|a| !a.is_empty())
        || b(st, "sourceWriteViolation")
        || st["conflictMarkers"]
            .as_array()
            .is_some_and(|a| !a.is_empty());
    if has_violation {
        return true;
    }
    let pm_status = s(&st["premerge"], "status");
    if matches!(pm_status, "conflict" | "failed") {
        return true;
    }
    if !result_read
        && matches!(
            state,
            "failed" | "crashed" | "timeout" | "stalled" | "malformed" | "rejected"
        )
    {
        return true;
    }
    false
}

/// True when ANY configured account (active OR backup) has a quota depletion-before-reset warning,
/// remaining percentage `< 15%`, or `liveDrifted` / `identityMismatch`.
/// Used for the `4 额度(!)` tab badge (advisory only).
pub fn compute_quota_alert_flag(agy_accounts_snap: &Value) -> bool {
    if !agy_accounts_snap.is_object() {
        return false;
    }
    if b(agy_accounts_snap, "liveDrifted") {
        return true;
    }
    if !agy_accounts::detect_quota_depletion_warnings(agy_accounts_snap).is_empty() {
        return true;
    }
    if let Some(accounts) = agy_accounts_snap["accounts"].as_array() {
        for acct in accounts {
            let quota = &acct["quota"];
            if b(quota, "identityMismatch") {
                return true;
            }
            if b(quota, "ok") {
                if let Some(entries) = quota["entries"].as_array() {
                    for e in entries {
                        if let Some(pct) = e["remainingPct"].as_f64() {
                            if pct < QUOTA_LOW_REMINDER_PCT {
                                return true;
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

/// Unified worktable pool breakdown shared by `delegate work` and `delegate snapshot`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkPulseCounts {
    pub owner_open: usize,
    pub active_pool: usize,
    pub waiting_dep: usize,
    pub timed: usize,
    pub deferred: usize,
}

/// Count worktable buckets using the exact single-source-of-truth rules:
/// - `owner_open`: open owner items (`owner` object present, `state != "done"`).
/// - `active_pool`: open engineering/owner items + ephemeral runs, excluding `deferred` and pure `after_time:` timed waits.
/// - `waiting_dep`: open non-owner `blocked` items that are NOT pure `after_time:` timed waits.
/// - `timed`: open non-owner `blocked` items waiting exclusively on `after_time:`.
/// - `deferred`: open non-owner `deferred` items.
pub fn count_work_pulse(work_items_json: &[Value]) -> WorkPulseCounts {
    let mut c = WorkPulseCounts::default();
    for w in work_items_json {
        let state = s(w, "state");
        if state == "done" {
            continue;
        }
        if b(w, "ephemeral") {
            c.active_pool += 1;
            continue;
        }
        if w["owner"].is_object() {
            c.owner_open += 1;
            c.active_pool += 1;
        } else if state == "deferred" {
            c.deferred += 1;
        } else if is_pure_timed_wait_work_json(w) {
            c.timed += 1;
        } else {
            c.active_pool += 1;
            if state == "blocked" {
                c.waiting_dep += 1;
            }
        }
    }
    c
}

/// True when a run status JSON is actively executing (running/starting without queue metadata).
pub fn is_run_active_running_state(st: &Value) -> bool {
    let state = s(st, "state");
    matches!(state, "running" | "starting" | "scouting") && !run_has_queue(st)
}

/// True when a run status JSON is queued/waiting (`waiting`, `queued`, `blocked_mainline`,
/// or `running`/`starting` carrying queue metadata).
pub fn is_run_queued_state(st: &Value) -> bool {
    let state = s(st, "state");
    matches!(state, "waiting" | "queued" | "blocked_mainline")
        || (matches!(state, "running" | "starting") && run_has_queue(st))
}

/// Split of actionable pending run IDs into active (`<= 24h` or attached to an open work item)
/// vs stale (`> 24h` and not attached to any open work item).
/// Shared by `delegate snapshot`, `delegate status`, and `delegate hook pending`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingRunSplit {
    pub active_pending: Vec<String>,
    pub stale_pending: Vec<String>,
}

pub fn split_pending_runs(
    runs: &[Value],
    work_items_json: &[Value],
    now_ep: i64,
) -> PendingRunSplit {
    let refs: Vec<&Value> = runs.iter().collect();
    split_pending_runs_refs(&refs, work_items_json, now_ep)
}

pub fn split_pending_runs_refs(
    all_runs: &[&Value],
    work_items_json: &[Value],
    now_ep: i64,
) -> PendingRunSplit {
    let mut open_work_ids: HashSet<&str> = HashSet::new();
    let mut open_work_run_ids: HashSet<&str> = HashSet::new();
    for w in work_items_json {
        if b(w, "ephemeral") || s(w, "state") == "done" {
            continue;
        }
        let wid = s(w, "id");
        if !wid.is_empty() {
            open_work_ids.insert(wid);
        }
        if let Some(runs_arr) = w["runs"].as_array() {
            for r in runs_arr {
                if let Some(rid) = r.as_str() {
                    if !rid.is_empty() {
                        open_work_run_ids.insert(rid);
                    }
                }
            }
        }
    }

    let mut by_id: HashMap<String, &Value> = HashMap::with_capacity(all_runs.len());
    let mut superseded_ids: HashSet<&str> = HashSet::new();
    for &st in all_runs {
        let rid = s(st, "run");
        if !rid.is_empty() {
            by_id.insert(rid.to_string(), st);
        }
        let parent = s(st, "parent");
        if !parent.is_empty() {
            superseded_ids.insert(parent);
        }
    }

    let mut out = PendingRunSplit::default();
    for &st in all_runs {
        let rid = s(st, "run");
        if rid.is_empty() || superseded_ids.contains(rid) {
            continue;
        }
        if is_run_active_running_state(st) || is_run_queued_state(st) {
            continue;
        }
        let state = s(st, "state");
        let mode = s(st, "mode");
        let applied = b(st, "applied") || !s(st, "appliedAt").trim().is_empty();
        let cleaned = b(st, "cleaned") || state == "cleaned";
        let replied = !s(st, "repliedAt").trim().is_empty();
        let question = b(st, "question") && !replied;
        let result_read = !s(st, "resultReadAt").trim().is_empty();
        let zero_change = hook_check::is_zero_change_delivery_ref(st, &by_id);
        if !hook_check::is_run_pending_actionable(
            state,
            mode,
            applied,
            cleaned,
            question,
            result_read,
            false,
            zero_change,
        ) {
            continue;
        }
        let age_secs = run_last_activity_secs(st, now_ep);
        let attached_to_open_work = open_work_run_ids.contains(rid)
            || (!s(st, "workId").is_empty() && open_work_ids.contains(s(st, "workId")));
        if age_secs > STALE_PENDING_SECS && !attached_to_open_work {
            out.stale_pending.push(rid.to_string());
        } else {
            out.active_pending.push(rid.to_string());
        }
    }
    out.active_pending.sort();
    out.stale_pending.sort();
    out
}

/// Format the advisory-only single-line quota alert for `delegate status`, using the exact same
/// active-account P0 rules as `compute_pulse_and_alerts`:
/// depletion before reset, `< 15%` remaining reminder, and `liveDrifted` / `identityMismatch`.
pub fn format_status_quota_alert_line_in(state_root: &Path, now_ep: i64) -> Option<String> {
    let agy_snap = agy_accounts::quota_snapshot_for_alerts_in(state_root, now_ep)?;
    format_status_quota_alert_line_from_snapshot(&agy_snap)
}

pub fn format_status_quota_alert_line_from_snapshot(agy_accounts_snap: &Value) -> Option<String> {
    if !agy_accounts_snap.is_object() {
        return None;
    }
    let dep_warnings = agy_accounts::detect_quota_depletion_warnings(agy_accounts_snap);
    if let Some(line) = agy_accounts::format_status_quota_warning_line(&dep_warnings) {
        return Some(line);
    }

    let accounts = agy_accounts_snap["accounts"].as_array()?;
    let is_auto = s(agy_accounts_snap, "mode") == agy_accounts::AUTO_ACCOUNT_ID
        || b(&agy_accounts_snap["autoSwitch"], "enabled");
    let active_acct = accounts.iter().find(|a| b(a, "active"));
    let active_id = active_acct
        .map(|a| s(a, "id"))
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| s(agy_accounts_snap, "active"));
    let active_short = if active_id.is_empty() {
        "当前账号".to_string()
    } else {
        mask_account_short(active_id)
    };

    if b(agy_accounts_snap, "liveDrifted") {
        return Some(format!(
            "⚠ 额度提醒：★{active_short} 当前本机登录态与记录不一致（请运行 delegate agy account status 核对，仅提醒不自动切号）"
        ));
    }
    if active_acct.is_some_and(|a| b(&a["quota"], "identityMismatch")) {
        return Some(format!(
            "⚠ 额度提醒：★{active_short} 额度缓存账号身份不匹配（请运行 delegate agy account status 核对，仅提醒不自动切号）"
        ));
    }
    if is_auto {
        // In auto mode, only remind on `< 15%` when ALL healthy accounts for that pool are non-Green.
        let healthy: Vec<&Value> = accounts
            .iter()
            .filter(|a| b(&a["quota"], "ok") && !b(&a["quota"], "identityMismatch"))
            .collect();
        let mut low_parts = Vec::new();
        for pool in agy_accounts::QuotaPool::ALL {
            let any_green = healthy.iter().any(|a| {
                a["quotaEstimate"]["pools"]
                    .as_array()
                    .and_then(|arr| {
                        arr.iter()
                            .find(|pe| {
                                agy_accounts::classify_quota_pool_name(s(pe, "pool")) == Some(pool)
                            })
                            .map(|pe| s(pe, "tier") == "green")
                    })
                    .unwrap_or(false)
            });
            if any_green {
                continue;
            }
            let pkey = pool.key();
            let sel_id = s(&agy_accounts_snap["autoSwitch"]["poolSummary"][pkey], "selected");
            let target = healthy
                .iter()
                .copied()
                .find(|a| !sel_id.is_empty() && s(a, "id") == sel_id)
                .or(active_acct);
            if let Some(acct) = target {
                if let Some(entries) = acct["quota"]["entries"].as_array() {
                    for e in entries {
                        if agy_accounts::classify_quota_pool_name(s(e, "pool")) != Some(pool) {
                            continue;
                        }
                        let Some(pct) = e["remainingPct"].as_f64() else {
                            continue;
                        };
                        if pct < QUOTA_LOW_REMINDER_PCT {
                            let pool_short = short_pool_name(s(e, "pool"));
                            let win_short = short_window_name(s(e, "window"));
                            low_parts.push(format!("{pool_short}{win_short}仅剩{}%", pct.round() as i64));
                        }
                    }
                }
            }
        }
        if !low_parts.is_empty() {
            return Some(format!(
                "⚠ 额度偏低：auto 模式所有可用号 {}（请留意重置时间或调整任务节奏）",
                low_parts.join("、")
            ));
        }
        return None;
    }
    if let Some(acct) = active_acct {
        let quota = &acct["quota"];
        if b(quota, "ok") && !b(quota, "identityMismatch") {
            if let Some(entries) = quota["entries"].as_array() {
                let mut low_parts = Vec::new();
                for e in entries {
                    let Some(pct) = e["remainingPct"].as_f64() else {
                        continue;
                    };
                    if pct < QUOTA_LOW_REMINDER_PCT {
                        let pool_short = short_pool_name(s(e, "pool"));
                        let win_short = short_window_name(s(e, "window"));
                        low_parts.push(format!("{pool_short}{win_short}仅剩{}%", pct.round() as i64));
                    }
                }
                if !low_parts.is_empty() {
                    return Some(format!(
                        "⚠ 额度偏低：★{active_short} {}（可按需切号或启用 auto：delegate agy account use auto）",
                        low_parts.join("、")
                    ));
                }
            }
        }
    }
    None
}

/// Compute unified `SnapshotPulse` + ordered `Vec<ActiveAlert>` from the snapshot inputs.
/// Strictly read-only, single source of truth for TUI, `delegate status`, `delegate work`, and `hook pending`.
pub fn compute_pulse_and_alerts(
    all_runs: &[&Value],
    work_items_json: &[Value],
    agy_accounts_snap: &Value,
    merged_today: usize,
    now_ep: i64,
) -> (SnapshotPulse, Vec<ActiveAlert>) {
    // 1. Worktable counts from the shared `count_work_pulse` source of truth,
    //    plus owner downstream wait counts for P1 owner_blocked alerts.
    let work_pulse = count_work_pulse(work_items_json);
    let owner_open = work_pulse.owner_open;
    let work_active_pool = work_pulse.active_pool;
    let work_deferred = work_pulse.deferred;
    let work_timed = work_pulse.timed;
    let mut owner_blocked_downstream: HashMap<&str, usize> = HashMap::new();

    // Count open downstream work items waiting on each open owner item (`owner:Qxx` or `work:Qxx`).
    for w in work_items_json {
        if b(w, "ephemeral") || s(w, "state") == "done" {
            continue;
        }
        if let Some(wait_arr) = w["waitOn"].as_array() {
            for cond in wait_arr.iter().filter_map(Value::as_str) {
                if let Some(target) = cond
                    .strip_prefix("owner:")
                    .or_else(|| cond.strip_prefix("work:"))
                    .or_else(|| cond.strip_prefix("work_done:"))
                {
                    *owner_blocked_downstream.entry(target).or_insert(0) += 1;
                }
            }
        }
    }

    // 2. Classify runs across all repos using shared `split_pending_runs_refs` + running/queued helpers.
    let mut superseded_ids: HashSet<&str> = HashSet::new();
    for &st in all_runs {
        let parent = s(st, "parent");
        if !parent.is_empty() {
            superseded_ids.insert(parent);
        }
    }
    let pending_split = split_pending_runs_refs(all_runs, work_items_json, now_ep);
    let active_pending_set: HashSet<&str> = pending_split
        .active_pending
        .iter()
        .map(String::as_str)
        .collect();
    let controller_pending = pending_split.active_pending.len();
    let stale_pending = pending_split.stale_pending.len();

    let mut running_runs = 0usize;
    let mut queued_runs = 0usize;
    let mut pending_has_urgent = false;

    let mut p0_alerts: Vec<ActiveAlert> = Vec::new();
    let mut p1_alerts: Vec<ActiveAlert> = Vec::new();

    struct StalledRun {
        run: String,
        name: String,
        age_secs: i64,
        is_question: bool,
        fast_forward: bool,
    }
    let mut stalled_runs: Vec<StalledRun> = Vec::new();
    let mut urgent_run_alerts: Vec<(i64, ActiveAlert)> = Vec::new();

    for &st in all_runs {
        let rid = s(st, "run");
        if rid.is_empty() || superseded_ids.contains(rid) {
            continue;
        }
        if is_run_active_running_state(st) {
            running_runs += 1;
            continue;
        }
        if is_run_queued_state(st) {
            queued_runs += 1;
            continue;
        }
        if !active_pending_set.contains(rid) {
            continue;
        }

        let state = s(st, "state");
        let applied = b(st, "applied") || !s(st, "appliedAt").trim().is_empty();
        let cleaned = b(st, "cleaned") || state == "cleaned";
        let replied = !s(st, "repliedAt").trim().is_empty();
        let question = b(st, "question") && !replied;
        let result_read = !s(st, "resultReadAt").trim().is_empty();
        let age_secs = run_last_activity_secs(st, now_ep);
        let name = run_display_name(st);
        let is_urgent = is_run_urgent_failure_or_conflict(st, applied, cleaned, result_read);
        if is_urgent {
            pending_has_urgent = true;
            let pm_status = s(&st["premerge"], "status");
            let reason = if pm_status == "conflict" {
                let first = st["premerge"]["conflicts"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if first.is_empty() {
                    "预合并冲突".to_string()
                } else {
                    format!("预合并冲突({first})")
                }
            } else if pm_status == "failed" {
                "预合并失败".to_string()
            } else if b(st, "sourceWriteViolation") {
                "越权改源码".to_string()
            } else if st["protectViolation"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
            {
                "受控路径违规".to_string()
            } else if st["conflictMarkers"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
            {
                "含冲突标记".to_string()
            } else if state == "rejected" {
                if let Some(code) = st["accept"]["exitCode"].as_i64() {
                    format!("验收✗(exit={code})")
                } else {
                    "验收未通过".to_string()
                }
            } else {
                format!("执行异常({state})")
            };
            urgent_run_alerts.push((
                age_secs,
                ActiveAlert {
                    level: AlertLevel::P0,
                    kind: if pm_status == "conflict" {
                        "premerge_conflict".to_string()
                    } else {
                        "run_failure".to_string()
                    },
                    label: "失败/冲突".to_string(),
                    message: format!("{name} {reason} → 按 3 查审查卡处理"),
                    target_tab: 3,
                    target_id: Some(rid.to_string()),
                },
            ));
        } else if question && age_secs >= QUESTION_STALL_SECS {
            pending_has_urgent = true;
            stalled_runs.push(StalledRun {
                run: rid.to_string(),
                name,
                age_secs,
                is_question: true,
                fast_forward: false,
            });
        } else if age_secs >= CONTROLLER_STALL_SECS {
            pending_has_urgent = true;
            let ff = s(&st["premerge"], "status") == "ready";
            stalled_runs.push(StalledRun {
                run: rid.to_string(),
                name,
                age_secs,
                is_question: false,
                fast_forward: ff,
            });
        }
    }

    // Order urgent run P0 alerts newest-first (most actionable on top).
    urgent_run_alerts.sort_by_key(|a| a.0);
    for (_, alert) in urgent_run_alerts {
        p0_alerts.push(alert);
    }

    // 3. Active `agy` account P0 drift alerts (ADVISORY ONLY, never auto-switch).
    //    `quota_depletion` and `quota_low` intentionally stay out of the global top alert banner
    //    to avoid layout jitter; they remain visible on the `4 额度` tab, the Overview quota card,
    //    the `4 额度(!)` tab badge (`pulse.quota_alert`), and `delegate status`.
    let quota_alert = compute_quota_alert_flag(agy_accounts_snap);
    if agy_accounts_snap.is_object() {
        let active_acct = agy_accounts_snap["accounts"]
            .as_array()
            .and_then(|arr| arr.iter().find(|a| b(a, "active")));
        let active_id = active_acct.map(|a| s(a, "id")).unwrap_or("");
        let active_short = mask_account_short(active_id);

        if b(agy_accounts_snap, "liveDrifted") {
            p0_alerts.push(ActiveAlert {
                level: AlertLevel::P0,
                kind: "quota_drift".to_string(),
                label: "额度·登录态漂移".to_string(),
                message: format!("★{active_short} 当前本机登录态与记录不一致 → 按 4 核对"),
                target_tab: 4,
                target_id: (!active_id.is_empty()).then(|| active_id.to_string()),
            });
        } else if active_acct.is_some_and(|a| b(&a["quota"], "identityMismatch")) {
            p0_alerts.push(ActiveAlert {
                level: AlertLevel::P0,
                kind: "quota_drift".to_string(),
                label: "额度·身份不匹配".to_string(),
                message: format!("★{active_short} 额度缓存账号身份不匹配 → 按 4 核对"),
                target_tab: 4,
                target_id: (!active_id.is_empty()).then(|| active_id.to_string()),
            });
        }
    }

    // 4. P1: Owner pending decision that blocks downstream OR waited > 2h.
    let mut urgent_owners: Vec<(usize, i64, String, String)> = Vec::new();
    for w in work_items_json {
        if b(w, "ephemeral") || s(w, "state") == "done" || !w["owner"].is_object() {
            continue;
        }
        let wid = s(w, "id");
        let downstream = owner_blocked_downstream.get(wid).copied().unwrap_or(0);
        let since_raw = if !s(w, "stateSince").is_empty() {
            s(w, "stateSince")
        } else {
            s(w, "createdAt")
        };
        let waited = iso_epoch(since_raw)
            .map(|ep| (now_ep - ep).max(0))
            .unwrap_or(0);
        if downstream > 0 || waited > OWNER_STALL_SECS {
            urgent_owners.push((
                downstream,
                waited,
                wid.to_string(),
                s(w, "title").to_string(),
            ));
        }
    }
    urgent_owners.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    if let Some((downstream, waited, wid, title)) = urgent_owners.first() {
        let extra = if urgent_owners.len() > 1 {
            format!(" 等{}项", urgent_owners.len())
        } else {
            String::new()
        };
        let detail = if *downstream > 0 {
            format!("阻下游 {downstream}")
        } else {
            format!("已等 {}", format_compact_duration(*waited))
        };
        p1_alerts.push(ActiveAlert {
            level: AlertLevel::P1,
            kind: "owner_blocked".to_string(),
            label: "等你拍板".to_string(),
            message: format!("[{wid}] {title}{extra}（{detail}）→ 按 2 处理"),
            target_tab: 2,
            target_id: Some(wid.clone()),
        });
    }

    // 5. P1: Runs stalled waiting on controller (>= 10m question or >= 30m pending apply/review).
    stalled_runs.sort_by(|a, b| b.is_question.cmp(&a.is_question).then_with(|| b.age_secs.cmp(&a.age_secs)));
    if let Some(top) = stalled_runs.first() {
        let dur = format_compact_duration(top.age_secs);
        let extra = if stalled_runs.len() > 1 {
            format!(" 等{}单", stalled_runs.len())
        } else {
            String::new()
        };
        let status_note = if top.is_question {
            "提问等决定"
        } else if top.fast_forward {
            "已PASS且快进✓待合入"
        } else {
            "待主控处理"
        };
        p1_alerts.push(ActiveAlert {
            level: AlertLevel::P1,
            kind: "controller_stalled".to_string(),
            label: format!("卡主控 {dur}"),
            message: format!("{}{extra} {status_note} → 按 3", top.name),
            target_tab: 3,
            target_id: Some(top.run.clone()),
        });
    }

    let p0_alert = !p0_alerts.is_empty();
    let mut alerts = p0_alerts;
    alerts.extend(p1_alerts);

    let pulse = SnapshotPulse {
        owner_open,
        controller_pending,
        stale_pending,
        running_runs,
        queued_runs,
        work_active_pool,
        work_deferred,
        work_timed,
        merged_today,
        pending_has_urgent,
        quota_alert,
        p0_alert,
    };

    (pulse, alerts)
}

/// Append advisory disk-health P0/P1 alerts (high/critical watermark and >24h/>10 GiB stale
/// slot-holding runs) onto an already-computed pulse & alert list. Never deletes runs.
pub fn append_disk_alerts(
    pulse: &mut SnapshotPulse,
    alerts: &mut Vec<ActiveAlert>,
    disk: &crate::disk::DiskHealthSummary,
) {
    use crate::disk::DiskLevel;
    let level = disk.worst_level();
    if level == DiskLevel::Critical {
        if let Some(line) = disk.format_compact_line() {
            alerts.insert(
                0,
                ActiveAlert {
                    level: AlertLevel::P0,
                    kind: "disk_critical".to_string(),
                    label: "磁盘临界".to_string(),
                    message: format!("{line} → 运行 delegate disk prune"),
                    target_tab: 1,
                    target_id: None,
                },
            );
            pulse.p0_alert = true;
        }
    } else if level == DiskLevel::High {
        if let Some(line) = disk.format_compact_line() {
            alerts.push(ActiveAlert {
                level: AlertLevel::P1,
                kind: "disk_high".to_string(),
                label: "磁盘高水位".to_string(),
                message: format!("{line} → 可运行 delegate disk prune"),
                target_tab: 1,
                target_id: None,
            });
        }
    }
    if let Some(top) = disk.stale_runs.first() {
        let extra = if disk.stale_runs.len() > 1 {
            format!(" 等{}个", disk.stale_runs.len())
        } else {
            String::new()
        };
        alerts.push(ActiveAlert {
            level: AlertLevel::P1,
            kind: "disk_stale_slot".to_string(),
            label: "长期占槽".to_string(),
            message: format!(
                "{}({}){} 已 {}h 无进展占 {} → 检查 apply/clean（不自动删）",
                top.name,
                top.slot,
                extra,
                top.idle_hours,
                format_bytes(top.bytes)
            ),
            target_tab: 3,
            target_id: Some(top.run.clone()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_counts_and_alerts_follow_single_source_of_truth() {
        let now_ep = 1_764_000_000i64;
        let fresh_run = json!({
            "run": "20261126-010000-feat-a",
            "name": "feat-a",
            "mode": "write",
            "state": "delivered",
            "applied": false,
            "finishedAgoSeconds": 3200,
            "changes": { "files": 3, "added": 50, "deleted": 10 },
            "premerge": { "status": "ready", "baseShort": "abc1234" },
        });
        let stale_run = json!({
            "run": "20261110-010000-old",
            "name": "old",
            "mode": "write",
            "state": "delivered",
            "applied": false,
            "finishedAgoSeconds": 3 * 86400,
            "changes": { "files": 1, "added": 5, "deleted": 0 },
        });
        let running_run = json!({
            "run": "20261126-030000-live",
            "name": "live",
            "mode": "write",
            "state": "running",
        });
        let conflict_run = json!({
            "run": "20261126-020000-conflict",
            "name": "conflict-task",
            "mode": "write",
            "state": "delivered",
            "applied": false,
            "finishedAgoSeconds": 600,
            "changes": { "files": 2, "added": 10, "deleted": 2 },
            "premerge": { "status": "conflict", "conflicts": ["src/auth.rs"] },
        });
        let runs_refs = vec![&fresh_run, &stale_run, &running_run, &conflict_run];

        let work_json = vec![
            json!({
                "id": "Q01",
                "title": "定 API 版本",
                "state": "attention",
                "owner": { "kind": "direction" },
                "stateSince": "2026-11-25T00:00:00Z",
            }),
            json!({
                "id": "W02",
                "title": "依赖 Q01",
                "state": "blocked",
                "waitOn": ["owner:Q01"],
            }),
            json!({
                "id": "W03",
                "title": "定时发布",
                "state": "blocked",
                "waitOn": ["after_time:2026-12-01T00:00:00Z"],
                "waitOnStatus": [{ "condition": "after_time:2026-12-01T00:00:00Z", "satisfied": false }],
            }),
            json!({
                "id": "W04",
                "title": "暂缓项",
                "state": "deferred",
            }),
            json!({
                "id": "R:live",
                "ephemeral": true,
                "state": "active",
            }),
        ];

        let agy_snap = json!({
            "accounts": [{
                "id": "guitest@example.com",
                "active": true,
                "quota": {
                    "ok": true,
                    "entries": [
                        { "pool": "Gemini", "window": "weekly", "remainingPct": 41.0, "resetInSeconds": 432000 }
                    ]
                },
                "quotaEstimate": {
                    "pools": [{
                        "pool": "Gemini",
                        "tier": "yellow",
                        "weekly": {
                            "pool": "Gemini",
                            "status": "depletes",
                            "alert": true,
                            "fastMinutes": 90,
                            "midMinutes": 109,
                            "slowMinutes": 120,
                            "runningCount": 1,
                            "basis": "24h"
                        }
                    }]
                }
            }]
        });

        let (pulse, alerts) =
            compute_pulse_and_alerts(&runs_refs, &work_json, &agy_snap, 3, now_ep);

        assert_eq!(pulse.owner_open, 1);
        assert_eq!(pulse.controller_pending, 2); // fresh_run + conflict_run (stale_run excluded)
        assert_eq!(pulse.stale_pending, 1);
        assert_eq!(pulse.running_runs, 1);
        assert_eq!(pulse.queued_runs, 0);
        assert_eq!(pulse.work_active_pool, 3); // Q01 + W02 + R:live (excludes W03 timed, W04 deferred)
        assert_eq!(pulse.work_timed, 1);
        assert_eq!(pulse.work_deferred, 1);
        assert_eq!(pulse.merged_today, 3);
        assert!(pulse.pending_has_urgent);
        assert!(pulse.quota_alert);
        assert!(pulse.p0_alert);

        assert!(alerts.iter().any(|a| a.level == AlertLevel::P0 && a.kind == "premerge_conflict"));
        assert!(
            !alerts.iter().any(|a| a.kind == "quota_depletion"),
            "quota_depletion must not enter top alert banner: {alerts:?}"
        );
        assert!(alerts.iter().any(|a| a.level == AlertLevel::P1 && a.kind == "owner_blocked"));
        assert!(alerts.iter().any(|a| a.level == AlertLevel::P1 && a.kind == "controller_stalled"));
    }

    #[test]
    fn healthy_state_emits_zero_alerts_and_low_15pct_reminds_only_active() {
        let now_ep = 1_764_000_000i64;
        // Backup account at 10% sets quota_alert badge, but does NOT fire a P0 global banner.
        let agy_snap = json!({
            "accounts": [
                {
                    "id": "active@example.com",
                    "active": true,
                    "quota": {
                        "ok": true,
                        "entries": [
                            { "pool": "Gemini", "window": "weekly", "remainingPct": 80.0, "resetInSeconds": 432000 }
                        ]
                    }
                },
                {
                    "id": "backup@example.com",
                    "active": false,
                    "quota": {
                        "ok": true,
                        "entries": [
                            { "pool": "Gemini", "window": "fiveHour", "remainingPct": 10.0, "resetInSeconds": 7200 }
                        ]
                    }
                }
            ]
        });
        let (pulse, alerts) = compute_pulse_and_alerts(&[], &[], &agy_snap, 0, now_ep);
        assert!(alerts.is_empty(), "backup low quota should not trigger P0 global banner");
        assert!(!pulse.p0_alert);
        assert!(pulse.quota_alert, "backup low quota should set 4 额度(!) badge");
    }

    #[test]
    fn pulse_parity_across_status_work_hook_pending_and_snapshot() {
        let now_ep = 1_764_000_000i64;
        let runs = vec![
            json!({
                "run": "r-active",
                "name": "live-task",
                "state": "running",
                "mode": "write"
            }),
            json!({
                "run": "r-queued",
                "name": "queued-task",
                "state": "queued",
                "mode": "write"
            }),
            json!({
                "run": "r-blocked-mainline",
                "name": "mainline-task",
                "state": "blocked_mainline",
                "mode": "write"
            }),
            json!({
                "run": "r-fresh",
                "name": "fresh-delivery",
                "state": "delivered",
                "mode": "write",
                "changes": { "files": 2 },
                "finishedAgoSeconds": 600
            }),
            json!({
                "run": "r-stale-unattached",
                "name": "stale-orphan",
                "state": "delivered",
                "mode": "write",
                "changes": { "files": 1 },
                "finishedAgoSeconds": 30 * 3600
            }),
            json!({
                "run": "r-stale-attached",
                "name": "stale-on-work",
                "state": "delivered",
                "mode": "write",
                "changes": { "files": 1 },
                "workId": "W01",
                "finishedAgoSeconds": 30 * 3600
            }),
        ];
        let work_json = vec![
            json!({ "id": "Q01", "state": "attention", "owner": { "kind": "nod" } }),
            json!({ "id": "W01", "state": "attention", "runs": ["r-stale-attached"] }),
            json!({ "id": "W02", "state": "blocked", "waitOn": ["work_applied:W01"] }),
            json!({ "id": "W03", "state": "blocked", "waitOn": ["after_time:2026-12-01T00:00:00Z"] }),
            json!({ "id": "W04", "state": "deferred" }),
            json!({ "id": "R:r-active", "ephemeral": true, "state": "active" }),
        ];
        let low_quota_snap = json!({
            "active": "alpha@example.test",
            "liveDrifted": false,
            "accounts": [{
                "id": "alpha@example.test",
                "active": true,
                "quota": {
                    "ok": true,
                    "entries": [{
                        "pool": "Gemini Models",
                        "window": "weekly",
                        "remainingPct": 12.0,
                        "resetInSeconds": 300000
                    }]
                }
            }]
        });

        let refs: Vec<&Value> = runs.iter().collect();
        let (pulse, alerts) = compute_pulse_and_alerts(&refs, &work_json, &low_quota_snap, 2, now_ep);

        // 1. Work helper parity (`delegate work` <-> `snapshot`)
        let wp = count_work_pulse(&work_json);
        assert_eq!(wp.owner_open, pulse.owner_open);
        assert_eq!(wp.active_pool, pulse.work_active_pool);
        assert_eq!(wp.timed, pulse.work_timed);
        assert_eq!(wp.deferred, pulse.work_deferred);
        assert_eq!(wp.waiting_dep, 1); // W02 only; W03 is pure timed wait
        assert_eq!(wp.timed, 1);

        // 2. Pending split parity (`hook pending` & `status` <-> `snapshot`)
        let split = split_pending_runs(&runs, &work_json, now_ep);
        assert_eq!(split.active_pending.len(), pulse.controller_pending);
        assert_eq!(split.stale_pending.len(), pulse.stale_pending);
        assert_eq!(split.active_pending, vec!["r-fresh", "r-stale-attached"]);
        assert_eq!(split.stale_pending, vec!["r-stale-unattached"]);

        // 3. Running & queued parity (`status` <-> `snapshot`)
        let running_cnt = runs.iter().filter(|r| is_run_active_running_state(r)).count();
        let queued_cnt = runs.iter().filter(|r| is_run_queued_state(r)).count();
        assert_eq!(running_cnt, pulse.running_runs);
        assert_eq!(queued_cnt, pulse.queued_runs);
        assert_eq!(queued_cnt, 2); // queued + blocked_mainline

        // 4. Advisory quota alert parity:
        //    - `delegate status` still emits the `<15%` advisory line and `pulse.quota_alert` stays true (`4 额度(!)` badge)
        //    - `quota_low` does NOT enter top `alerts` banner (prevents layout jitter)
        //    - `liveDrifted` and `identityMismatch` still enter top `alerts` banner as P0 `quota_drift`
        let low_line = format_status_quota_alert_line_from_snapshot(&low_quota_snap)
            .expect("active <15% should produce advisory status reminder");
        assert!(low_line.contains("仅剩12%"), "{low_line}");
        assert!(pulse.quota_alert, "active <15% should keep 4 额度(!) badge");
        assert!(
            !alerts.iter().any(|a| a.kind == "quota_low"),
            "quota_low must not enter top alert banner: {alerts:?}"
        );

        let drifted_snap = json!({
            "active": "alpha@example.test",
            "liveDrifted": true,
            "accounts": [{ "id": "alpha@example.test", "active": true }]
        });
        let drift_line = format_status_quota_alert_line_from_snapshot(&drifted_snap)
            .expect("liveDrifted should produce advisory status reminder");
        assert!(drift_line.contains("不一致"), "{drift_line}");
        let (drift_pulse, drift_alerts) =
            compute_pulse_and_alerts(&[], &[], &drifted_snap, 0, now_ep);
        assert!(drift_pulse.p0_alert);
        assert!(drift_pulse.quota_alert);
        assert!(drift_alerts.iter().any(|a| {
            a.level == AlertLevel::P0 && a.kind == "quota_drift" && a.label.contains("登录态漂移")
        }));

        let mismatch_snap = json!({
            "active": "alpha@example.test",
            "liveDrifted": false,
            "accounts": [{
                "id": "alpha@example.test",
                "active": true,
                "quota": { "ok": false, "identityMismatch": true }
            }]
        });
        let (mismatch_pulse, mismatch_alerts) =
            compute_pulse_and_alerts(&[], &[], &mismatch_snap, 0, now_ep);
        assert!(mismatch_pulse.p0_alert);
        assert!(mismatch_pulse.quota_alert);
        assert!(mismatch_alerts.iter().any(|a| {
            a.level == AlertLevel::P0 && a.kind == "quota_drift" && a.label.contains("身份不匹配")
        }));
    }
}
