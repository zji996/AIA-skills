//! Zero-token, lock-free ETA projection engine for single runs, work-item DAGs,
//! and `dag-queue.json` continuations.
//!
//! Consumes the unified `<state>/eta-model.json` produced by `speed.rs` (lazily
//! rebuilding it when missing, corrupt, or stale >15m with new events). Never
//! acquires `.state.lock` or `.start.lock`, runs no daemons, and never stops a
//! run on ETA overrun.
use crate::common::*;
use crate::speed::{
    self, EtaModel, FallbackLevel, RepoFormulaParams, SampleDist,
};
use crate::work::WorkItem;
use crate::{config, dag_queue, fill, has, lane, launch, notes, parse_simple, queue, runs, value, work};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

// ---------------------------------------------------------------------------
// Core math: RSS p75 combination, hysteresis rounding & refresh thresholds
// ---------------------------------------------------------------------------

/// Combine independent `(p50, p75)` duration segments (in seconds):
/// `p50_sum = sum(p50_i)`, `p75_sum = p50_sum + sqrt(sum((p75_i - p50_i)^2))`.
/// Avoids inflating p75 sums into p95+.
pub fn combine_p50_p75(segments: &[(f64, f64)]) -> (f64, f64) {
    let mut p50_sum = 0.0_f64;
    let mut spread_sq = 0.0_f64;
    for &(p50, p75) in segments {
        let a = p50.max(0.0);
        let b = p75.max(a);
        p50_sum += a;
        let d = b - a;
        spread_sq += d * d;
    }
    let p75_sum = p50_sum + spread_sq.sqrt();
    (speed::round2(p50_sum), speed::round2(p75_sum))
}

/// Deterministic display bucketing (design.md §4):
/// - `< 5m`: nearest 1m (short text displays `<5m`)
/// - `5–15m`: round to 1m (60s)
/// - `15–90m`: round to 5m (300s)
/// - `> 90m`: round to 10m (600s)
pub fn round_eta_epoch(now_ts: i64, target_ts: i64) -> i64 {
    let rem = (target_ts - now_ts).max(0);
    let step = if rem <= 15 * 60 {
        60
    } else if rem <= 90 * 60 {
        5 * 60
    } else {
        10 * 60
    };
    let rounded = ((target_ts + step / 2) / step) * step;
    rounded.max(now_ts)
}

/// Format an ETA duration (seconds) with display bucketing:
/// - `< 5m` -> `<5m`
/// - `5–15m` -> 1m granularity (`8m`)
/// - `15–90m` -> 5m granularity (`35m`)
/// - `> 90m` -> 0.1h granularity (`2.4h`)
pub fn format_eta_duration(secs: f64) -> String {
    let s = secs.max(0.0);
    if s < 300.0 {
        return "<5m".to_string();
    }
    if s <= 15.0 * 60.0 {
        let m = (s / 60.0).round() as i64;
        return format!("{m}m");
    }
    if s <= 90.0 * 60.0 {
        let m = ((s / 300.0).round() as i64) * 5;
        return format!("{m}m");
    }
    let h = (s / 3600.0 * 10.0).round() / 10.0;
    format!("{h:.1}h")
}

/// Format a duration range (e.g. `2.4–3.6h` or `20–35m` or `<5m`).
pub fn format_duration_range(p50_sec: f64, p75_sec: f64) -> String {
    let a = p50_sec.max(0.0);
    let b = p75_sec.max(a);
    if b < 300.0 {
        return "<5m".to_string();
    }
    if b >= 90.0 * 60.0 {
        let h1 = (a / 3600.0 * 10.0).round() / 10.0;
        let h2 = (b / 3600.0 * 10.0).round() / 10.0;
        if (h2 - h1).abs() < 0.05 {
            format!("{h1:.1}h")
        } else {
            format!("{h1:.1}–{h2:.1}h")
        }
    } else {
        let m1 = if a < 300.0 {
            5
        } else if a <= 15.0 * 60.0 {
            (a / 60.0).round() as i64
        } else {
            ((a / 300.0).round() as i64) * 5
        };
        let m2 = if b <= 15.0 * 60.0 {
            (b / 60.0).round() as i64
        } else {
            ((b / 300.0).round() as i64) * 5
        }
        .max(m1);
        if m1 == m2 {
            format!("{m1}m")
        } else {
            format!("{m1}–{m2}m")
        }
    }
}

/// Format an elapsed/age duration concisely (`45s`, `12m`, `1.5h`, `2d`).
pub fn format_age_short(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        let h = s / 3600;
        let m = (s % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        }
    } else {
        format!("{}d", s / 86_400)
    }
}

/// Format a target epoch as local `HH:MM` (or `MM-DD HH:MM` when >18h away).
pub fn format_clock(ts: i64, now_ts: i64) -> String {
    if (ts - now_ts).abs() > 18 * 3600 {
        format_epoch(ts, true, "%m-%d %H:%M")
    } else {
        format_epoch(ts, true, "%H:%M")
    }
}

/// Format an epoch as local short time matching TUI (`HH:MM` if same local calendar day,
/// otherwise `MM-DD HH:MM`).
pub fn format_local_short(ts: i64, now_ts: i64) -> String {
    if format_epoch(ts, true, "%Y-%m-%d") == format_epoch(now_ts, true, "%Y-%m-%d") {
        format_epoch(ts, true, "%H:%M")
    } else {
        format_epoch(ts, true, "%m-%d %H:%M")
    }
}

/// Format a compact ETA clock badge guaranteed to stay within <= 6 display columns
/// for narrow/phone TUI layouts (`<5m`, `~03:40`, `~20h`, `~2d`).
pub fn format_compact_clock(ts: i64, now_ts: i64, rem_sec: f64) -> String {
    if rem_sec < 300.0 {
        return "<5m".to_string();
    }
    let delta = (ts - now_ts).max(0);
    if delta <= 18 * 3600 {
        format!("~{}", format_epoch(ts, true, "%H:%M"))
    } else if delta < 48 * 3600 {
        let h = ((delta as f64) / 3600.0).round() as i64;
        format!("~{h}h")
    } else {
        let d = ((delta as f64) / 86_400.0).round() as i64;
        format!("~{d}d")
    }
}

/// Hysteresis refresh check for write paths (design.md §4):
/// Refresh `shownP50At` only when:
/// 1. No prior `shownP50At`,
/// 2. Stage changed,
/// 3. Blocker status changed,
/// 4. Shift exceeds `max(5m, 15% * remaining)`, or
/// 5. Current time has passed `shownP50At` (smoothly slide toward `p75At`).
#[allow(dead_code)] // Consumed by Chain 3/4 write paths and unit tests.
pub fn should_refresh_shown_eta(
    prev_shown_p50_at: Option<i64>,
    prev_stage: Option<&str>,
    prev_blocked: bool,
    new_p50_at: i64,
    new_stage: &str,
    new_blocked: bool,
    now_ts: i64,
) -> bool {
    let Some(shown) = prev_shown_p50_at else {
        return true;
    };
    if prev_stage != Some(new_stage) || prev_blocked != new_blocked {
        return true;
    }
    if now_ts > shown {
        return true;
    }
    let rem = (new_p50_at - now_ts).max(0) as f64;
    let threshold = 300.0_f64.max(0.15 * rem);
    ((new_p50_at - shown).abs() as f64) > threshold
}

/// Compute real-time heavy lane wait estimate (design.md §3.2):
/// `W_lane = (sum_{j=1..L} cond_rem(lane_held, e_j) + max(0, k - 1) * lane_held_p50) / L`
/// where `k = ahead` is 1-based queue position (1 = next in line once a slot frees).
pub fn compute_lane_wait(
    lane_held: &SampleDist,
    holders_elapsed: &[f64],
    max_heavy: usize,
    ahead: usize,
) -> (f64, f64) {
    let l = max_heavy.max(1) as f64;
    let k = ahead.max(1);
    let base_p50 = lane_held.p50_sec.unwrap_or(120.0).max(1.0);
    let base_p75 = lane_held.p75_sec.unwrap_or(base_p50 * 1.5).max(base_p50);

    let mut holder_segs: Vec<(f64, f64)> = Vec::new();
    if holders_elapsed.is_empty() {
        // No holder details visible; conservatively assume L active holders at half p50.
        for _ in 0..(max_heavy.max(1)) {
            let (r50, r75, _) = lane_held
                .conditional_residual(base_p50 * 0.5)
                .unwrap_or((base_p50 * 0.5, base_p75 * 0.7, false));
            holder_segs.push((r50 / l, r75 / l));
        }
    } else {
        for &e in holders_elapsed.iter().take(max_heavy.max(1)) {
            let (r50, r75, _) = lane_held
                .conditional_residual(e.max(0.0))
                .unwrap_or((base_p50, base_p75, false));
            holder_segs.push((r50 / l, r75 / l));
        }
    }

    // Add `(k - 1)` full queued jobs ahead of this run, each scaled by 1/L.
    let queued_ahead = k.saturating_sub(1);
    for _ in 0..queued_ahead {
        holder_segs.push((base_p50 / l, base_p75 / l));
    }

    combine_p50_p75(&holder_segs)
}

/// Compute effective live concurrency for DAG throughput bound (design.md §3.2):
/// When `p_live > 0`: `min(max(p_live, 1.0), p_eff_p75, fill_target)`
/// When `p_live == 0` (idle planning): `min(max(p_eff_hist, 1.0), p_eff_p75, fill_target)`
pub fn effective_concurrency(
    p_live: usize,
    formula: &RepoFormulaParams,
    fill_target: Option<usize>,
) -> f64 {
    let hist_mean = formula.p_eff_mean.unwrap_or(2.0).max(1.0);
    let hist_p75 = formula
        .p_eff_p75
        .unwrap_or(hist_mean)
        .max(1.0);
    let raw = if p_live > 0 {
        (p_live as f64).max(1.0).min(hist_p75)
    } else {
        hist_mean.min(hist_p75)
    };
    let capped = match fill_target {
        Some(t) if t > 0 => raw.min(t as f64),
        _ => raw,
    };
    speed::round2(capped.max(1.0))
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    CapacityQueue,
    AgentWork,
    LaneQueue,
    Accept,
    ReviewParallel,
    Rework,
    Premerge,
    MergeWait,
    Done,
    Blocked,
}

impl Stage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CapacityQueue => "capacity_queue",
            Self::AgentWork => "agent_work",
            Self::LaneQueue => "lane_queue",
            Self::Accept => "accept",
            Self::ReviewParallel => "review_parallel",
            Self::Rework => "rework",
            Self::Premerge => "premerge",
            Self::MergeWait => "merge_wait",
            Self::Done => "done",
            Self::Blocked => "blocked",
        }
    }

    pub fn label_zh(&self) -> &'static str {
        match self {
            Self::CapacityQueue => "容量排队",
            Self::AgentWork => "模型工作中",
            Self::LaneQueue => "等 lane",
            Self::Accept => "验收中",
            Self::ReviewParallel => "慢层+审查",
            Self::Rework => "返工中",
            Self::Premerge => "预合并中",
            Self::MergeWait => "待合入",
            Self::Done => "已完成",
            Self::Blocked => "停表阻塞",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockerKind {
    ControllerQuestion,
    ControllerReview,
    ControllerSign,
    ControllerMerge,
    ControllerAttention,
    PremergeConflict,
    PremergeFailed,
    MainlineBlocked,
    Owner,
    ExternalTime,
    StallWarning,
    MemoryWait,
    QueuePaused,
}

impl BlockerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ControllerQuestion => "controller_question",
            Self::ControllerReview => "controller_review",
            Self::ControllerSign => "controller_sign",
            Self::ControllerMerge => "controller_merge",
            Self::ControllerAttention => "controller_attention",
            Self::PremergeConflict => "premerge_conflict",
            Self::PremergeFailed => "premerge_failed",
            Self::MainlineBlocked => "mainline_blocked",
            Self::Owner => "owner",
            Self::ExternalTime => "external_time",
            Self::StallWarning => "stall_warning",
            Self::MemoryWait => "memory_wait",
            Self::QueuePaused => "queue_paused",
        }
    }

    pub fn compact_badge(&self) -> &'static str {
        match self {
            Self::ControllerQuestion
            | Self::ControllerReview
            | Self::ControllerSign
            | Self::ControllerMerge
            | Self::ControllerAttention => "卡主控",
            Self::PremergeConflict | Self::PremergeFailed => "卡冲突",
            Self::MainlineBlocked => "卡主干",
            Self::Owner => "卡所有",
            Self::ExternalTime => "等定时",
            Self::StallWarning => "卡住",
            Self::MemoryWait => "等内存",
            Self::QueuePaused => "已暂停",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BlockerInfo {
    pub kind: BlockerKind,
    pub who: String,
    pub reason: String,
    pub since_sec: Option<i64>,
    pub target: Option<String>,
    pub affected_count: Option<usize>,
    pub unlock_at: Option<i64>,
    pub after_unlock_p50_sec: Option<f64>,
    pub after_unlock_p75_sec: Option<f64>,
    pub label: String,
}

impl BlockerInfo {
    pub fn to_json(&self, now_ts: i64) -> Value {
        let mut obj = json!({
            "kind": self.kind.as_str(),
            "who": self.who,
            "reason": self.reason,
            "label": self.label,
            "compact": self.kind.compact_badge(),
        });
        if let Some(sec) = self.since_sec {
            obj["sinceSeconds"] = json!(sec);
        }
        if let Some(t) = &self.target {
            obj["target"] = json!(t);
        }
        if let Some(n) = self.affected_count {
            obj["affectedCount"] = json!(n);
        }
        if let Some(u) = self.unlock_at {
            obj["unlockAt"] = json!(format_epoch(u, false, "%Y-%m-%dT%H:%M:%SZ"));
            obj["unlockClock"] = json!(format_clock(u, now_ts));
        }
        if let (Some(a), Some(b)) = (self.after_unlock_p50_sec, self.after_unlock_p75_sec) {
            obj["afterUnlockP50Sec"] = json!(speed::round2(a));
            obj["afterUnlockP75Sec"] = json!(speed::round2(b));
            obj["afterUnlockRange"] = json!(format_duration_range(a, b));
        }
        obj
    }
}

#[derive(Debug, Clone)]
pub struct EtaRange {
    pub p50_sec: f64,
    pub p75_sec: f64,
    pub p50_at: i64,
    pub p75_at: i64,
    pub rounded_p50_at: i64,
    pub rounded_p75_at: i64,
    pub anomaly: bool,
}

impl EtaRange {
    pub fn from_remaining(now_ts: i64, p50_sec: f64, p75_sec: f64, anomaly: bool) -> Self {
        let p50 = p50_sec.max(0.0);
        let p75 = p75_sec.max(p50);
        let p50_at = now_ts + p50.round() as i64;
        let p75_at = now_ts + p75.round() as i64;
        let rounded_p50_at = round_eta_epoch(now_ts, p50_at);
        let rounded_p75_at = round_eta_epoch(now_ts, p75_at).max(rounded_p50_at);
        Self {
            p50_sec: speed::round2(p50),
            p75_sec: speed::round2(p75),
            p50_at,
            p75_at,
            rounded_p50_at,
            rounded_p75_at,
            anomaly,
        }
    }

    pub fn to_json(&self, now_ts: i64) -> Value {
        json!({
            "p50Sec": self.p50_sec,
            "p75Sec": self.p75_sec,
            "p50Min": speed::sec_to_min(self.p50_sec),
            "p75Min": speed::sec_to_min(self.p75_sec),
            "rawP50Epoch": self.p50_at,
            "rawP75Epoch": self.p75_at,
            "p50At": format_epoch(self.rounded_p50_at, false, "%Y-%m-%dT%H:%M:%SZ"),
            "p75At": format_epoch(self.rounded_p75_at, false, "%Y-%m-%dT%H:%M:%SZ"),
            "p50Clock": format_clock(self.rounded_p50_at, now_ts),
            "p75Clock": format_clock(self.rounded_p75_at, now_ts),
            "durationText": format_eta_duration(self.p50_sec),
            "anomaly": self.anomaly,
        })
    }
}

#[derive(Debug, Clone)]
pub struct LaneWaitEstimate {
    pub ahead: usize,
    pub holders: usize,
    pub max_heavy: usize,
    pub wait_p50_sec: f64,
    pub wait_p75_sec: f64,
}

impl LaneWaitEstimate {
    pub fn to_json(&self) -> Value {
        json!({
            "ahead": self.ahead,
            "holders": self.holders,
            "maxHeavy": self.max_heavy,
            "waitP50Sec": self.wait_p50_sec,
            "waitP75Sec": self.wait_p75_sec,
            "waitP50Min": speed::sec_to_min(self.wait_p50_sec),
            "waitP75Min": speed::sec_to_min(self.wait_p75_sec),
        })
    }
}

#[derive(Debug, Clone)]
pub struct CapacityWaitEstimate {
    pub reason: String,
    pub ahead: usize,
    pub wait_p50_sec: f64,
    pub wait_p75_sec: f64,
}

impl CapacityWaitEstimate {
    pub fn to_json(&self) -> Value {
        json!({
            "reason": self.reason,
            "ahead": self.ahead,
            "waitP50Sec": self.wait_p50_sec,
            "waitP75Sec": self.wait_p75_sec,
            "waitP50Min": speed::sec_to_min(self.wait_p50_sec),
            "waitP75Min": speed::sec_to_min(self.wait_p75_sec),
        })
    }
}

#[derive(Debug, Clone)]
pub struct RunEta {
    pub run: String,
    pub name: String,
    pub repo: String,
    pub mode: String,
    pub class: String,
    pub state: String,
    pub stage: Stage,
    pub stage_label: String,
    pub stage_elapsed_sec: Option<f64>,
    pub round: usize,
    pub target_milestone: &'static str,
    pub to_delivered: Option<EtaRange>,
    pub to_applied: Option<EtaRange>,
    pub blocker: Option<BlockerInfo>,
    pub lane_wait: Option<LaneWaitEstimate>,
    pub capacity_wait: Option<CapacityWaitEstimate>,
    pub fallback_level: FallbackLevel,
    pub bucket_key: String,
    pub is_prior: bool,
    pub rework_rate: Option<f64>,
    pub short_text: String,
    pub compact_badge: String,
    pub detail_text: String,
    pub now_ts: i64,
}

impl RunEta {
    pub fn to_json(&self) -> Value {
        let mut obj = json!({
            "run": self.run,
            "name": self.name,
            "repo": self.repo,
            "repoName": speed::short_repo_name(&self.repo),
            "mode": self.mode,
            "class": self.class,
            "state": self.state,
            "stage": self.stage.as_str(),
            "stageLabel": self.stage_label,
            "round": self.round,
            "targetMilestone": self.target_milestone,
            "stoppedClock": self.blocker.is_some(),
            "fallbackLevel": self.fallback_level.as_str(),
            "bucketKey": self.bucket_key,
            "isPrior": self.is_prior,
            "shortText": self.short_text,
            "compactBadge": self.compact_badge,
            "detailText": self.detail_text,
        });
        if let Some(el) = self.stage_elapsed_sec {
            obj["stageElapsedSec"] = json!(speed::round2(el));
        }
        if let Some(rr) = self.rework_rate {
            obj["reworkRate"] = json!((rr * 1000.0).round() / 1000.0);
        }
        if let Some(d) = &self.to_delivered {
            obj["toDelivered"] = d.to_json(self.now_ts);
        }
        if let Some(a) = &self.to_applied {
            obj["toApplied"] = a.to_json(self.now_ts);
        }
        if let Some(b) = &self.blocker {
            obj["blocker"] = b.to_json(self.now_ts);
        }
        if let Some(lw) = &self.lane_wait {
            obj["laneWait"] = lw.to_json();
        }
        if let Some(cw) = &self.capacity_wait {
            obj["capacityWait"] = cw.to_json();
        }
        obj
    }
}

#[derive(Debug, Clone)]
pub struct WorkItemEta {
    pub id: String,
    pub title: String,
    pub repo: String,
    pub state: String,
    pub class: String,
    pub active_run: Option<String>,
    pub needs_verify: bool,
    pub external_est_at: Option<i64>,
    pub blocker: Option<BlockerInfo>,
    pub remaining: Option<EtaRange>,
    pub dev_p50_sec: f64,
    pub dev_p75_sec: f64,
    pub upstream_path: Vec<String>,
    pub short_text: String,
    pub focus_badge: String,
    pub compact_badge: String,
    pub is_prior: bool,
    pub now_ts: i64,
}

impl WorkItemEta {
    pub fn to_json(&self) -> Value {
        let mut obj = json!({
            "id": self.id,
            "title": self.title,
            "repo": self.repo,
            "repoName": speed::short_repo_name(&self.repo),
            "state": self.state,
            "class": self.class,
            "needsVerify": self.needs_verify,
            "stoppedClock": self.blocker.is_some(),
            "devP50Sec": speed::round2(self.dev_p50_sec),
            "devP75Sec": speed::round2(self.dev_p75_sec),
            "upstreamPath": self.upstream_path,
            "shortText": self.short_text,
            "focusBadge": self.focus_badge,
            "compactBadge": self.compact_badge,
            "isPrior": self.is_prior,
        });
        if let Some(r) = &self.active_run {
            obj["activeRun"] = json!(r);
        }
        if let Some(est) = self.external_est_at {
            obj["externalEstAt"] = json!(format_epoch(est, false, "%Y-%m-%dT%H:%M:%SZ"));
            obj["externalEstClock"] = json!(format_clock(est, self.now_ts));
        }
        if let Some(rem) = &self.remaining {
            obj["remaining"] = rem.to_json(self.now_ts);
        }
        if let Some(b) = &self.blocker {
            obj["blocker"] = b.to_json(self.now_ts);
        }
        obj
    }
}

#[derive(Debug, Clone)]
pub struct BlockerItem {
    pub id: String,
    pub title: String,
    pub blocker: BlockerInfo,
}

impl BlockerItem {
    pub fn to_json(&self, now_ts: i64) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "blocker": self.blocker.to_json(now_ts),
        })
    }
}

#[derive(Debug, Clone)]
pub struct DagEtaSummary {
    pub scope: String,
    pub write_chains: usize,
    pub depth: usize,
    pub active_count: usize,
    pub p_eff_now: f64,
    pub p_eff_live: usize,
    pub p_eff_hist: f64,
    pub fill_target: Option<usize>,
    pub lane_congested: bool,
    pub lane_extra_sec: f64,
    pub dev_wall_p50_sec: f64,
    pub dev_wall_p75_sec: f64,
    pub earliest_finish_p50_at: Option<i64>,
    pub earliest_finish_p75_at: Option<i64>,
    pub critical_path: Vec<String>,
    pub external_wait_count: usize,
    pub deferred_count: usize,
    pub earliest_deferred_at: Option<i64>,
    pub date_wait_count: usize,
    pub earliest_date_wait_at: Option<i64>,
    pub owner_blocked: Vec<BlockerItem>,
    pub blockers: Vec<BlockerItem>,
    pub is_prior: bool,
    pub headline: String,
    pub items: Vec<WorkItemEta>,
    pub now_ts: i64,
}

impl DagEtaSummary {
    pub fn to_json(&self) -> Value {
        let mut obj = json!({
            "scope": self.scope,
            "writeChains": self.write_chains,
            "depth": self.depth,
            "activeCount": self.active_count,
            "pEffNow": self.p_eff_now,
            "pEffLive": self.p_eff_live,
            "pEffHist": speed::round2(self.p_eff_hist),
            "fillTarget": self.fill_target,
            "laneCongested": self.lane_congested,
            "laneExtraSec": speed::round2(self.lane_extra_sec),
            "devWallP50Sec": speed::round2(self.dev_wall_p50_sec),
            "devWallP75Sec": speed::round2(self.dev_wall_p75_sec),
            "devWallRange": format_duration_range(self.dev_wall_p50_sec, self.dev_wall_p75_sec),
            "criticalPath": self.critical_path,
            "externalWaitCount": self.external_wait_count,
            "deferredCount": self.deferred_count,
            "dateWaitCount": self.date_wait_count,
            "ownerBlocked": self.owner_blocked.iter().map(|b| b.to_json(self.now_ts)).collect::<Vec<_>>(),
            "blockers": self.blockers.iter().map(|b| b.to_json(self.now_ts)).collect::<Vec<_>>(),
            "isPrior": self.is_prior,
            "headline": self.headline,
            "items": self.items.iter().map(WorkItemEta::to_json).collect::<Vec<_>>(),
        });
        if let Some(p50_at) = self.earliest_finish_p50_at {
            let r50 = round_eta_epoch(self.now_ts, p50_at);
            obj["earliestFinishP50At"] = json!(format_epoch(r50, false, "%Y-%m-%dT%H:%M:%SZ"));
            obj["earliestFinishP50Clock"] = json!(format_clock(r50, self.now_ts));
        }
        if let Some(p75_at) = self.earliest_finish_p75_at {
            let r75 = round_eta_epoch(self.now_ts, p75_at);
            obj["earliestFinishP75At"] = json!(format_epoch(r75, false, "%Y-%m-%dT%H:%M:%SZ"));
            obj["earliestFinishP75Clock"] = json!(format_clock(r75, self.now_ts));
        }
        if let Some(dt) = self.earliest_deferred_at {
            obj["earliestDeferredAt"] = json!(format_epoch(dt, false, "%Y-%m-%dT%H:%M:%SZ"));
            obj["earliestDeferredClock"] = json!(format_local_short(dt, self.now_ts));
        }
        if let Some(wt) = self.earliest_date_wait_at {
            obj["earliestDateWaitAt"] = json!(format_epoch(wt, false, "%Y-%m-%dT%H:%M:%SZ"));
            obj["earliestDateWaitClock"] = json!(format_local_short(wt, self.now_ts));
        }
        obj
    }
}

// ---------------------------------------------------------------------------
// Live context snapshot (strictly zero-lock, captured once per invocation)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct LiveLaneTicket {
    pub label: String,
    pub since_ts: Option<i64>,
    pub running: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LiveLaneSnapshot {
    pub local_max_heavy: usize,
    pub local: Vec<LiveLaneTicket>,
    pub remote_max_heavy: usize,
    pub remote: Vec<LiveLaneTicket>,
    pub remote_max_light: usize,
    pub remote_light: Vec<LiveLaneTicket>,
}

impl LiveLaneSnapshot {
    pub fn from_status_json(v: &Value) -> Self {
        let parse_items = |arr: &Value| -> Vec<LiveLaneTicket> {
            arr.as_array()
                .into_iter()
                .flatten()
                .map(|it| LiveLaneTicket {
                    label: s(it, "label").to_string(),
                    since_ts: iso_epoch(s(it, "since")),
                    running: b(it, "running"),
                })
                .collect()
        };
        Self {
            local_max_heavy: v["maxHeavy"].as_u64().unwrap_or(1).max(1) as usize,
            local: parse_items(&v["local"]),
            remote_max_heavy: v["remoteMaxHeavy"].as_u64().unwrap_or(1).max(1) as usize,
            remote: parse_items(&v["remote"]),
            remote_max_light: v["remoteMaxLight"].as_u64().unwrap_or(4).max(1) as usize,
            remote_light: parse_items(&v["remoteLight"]),
        }
    }

    pub fn capture() -> Self {
        match lane::status_json_readonly() {
            Ok(v) => Self::from_status_json(&v),
            Err(_) => Self {
                local_max_heavy: 1,
                remote_max_heavy: 1,
                remote_max_light: 4,
                ..Self::default()
            },
        }
    }

    fn holders_and_queue_pos(
        &self,
        is_remote: bool,
        label: &str,
        fallback_ahead: Option<usize>,
        now_ts: i64,
    ) -> (usize, Vec<f64>, usize) {
        let (max_h, list) = if is_remote {
            let in_light = !label.is_empty()
                && self.remote_light.iter().any(|t| !t.running && t.label == label);
            if in_light {
                (self.remote_max_light.max(1), &self.remote_light)
            } else {
                (self.remote_max_heavy.max(1), &self.remote)
            }
        } else {
            (self.local_max_heavy.max(1), &self.local)
        };
        let mut holders_elapsed = Vec::new();
        let mut queued_idx = 0usize;
        let mut matched_pos: Option<usize> = None;
        for t in list {
            if t.running {
                let e = t.since_ts.map(|st| (now_ts - st).max(0) as f64).unwrap_or(0.0);
                holders_elapsed.push(e);
            } else {
                queued_idx += 1;
                if matched_pos.is_none() && !label.is_empty() && t.label == label {
                    matched_pos = Some(queued_idx);
                }
            }
        }
        let ahead = matched_pos
            .or(fallback_ahead)
            .unwrap_or_else(|| queued_idx.max(1));
        (max_h, holders_elapsed, ahead.max(1))
    }

    fn running_ticket_for_run(&self, run: &Path, is_remote: bool) -> Option<&LiveLaneTicket> {
        let fname = run.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if fname.is_empty() {
            return None;
        }
        let lists: &[&[LiveLaneTicket]] = if is_remote {
            &[&self.remote, &self.remote_light]
        } else {
            &[&self.local]
        };
        lists.iter().flat_map(|l| l.iter()).find(|t| {
            t.running
                && (t.label == format!("accept {fname}")
                    || t.label == fname
                    || t.label.contains(fname))
        })
    }

    fn queued_ticket_for_run(&self, run: &Path, is_remote: bool) -> Option<(usize, &LiveLaneTicket)> {
        let fname = run.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if fname.is_empty() {
            return None;
        }
        let lists: &[&[LiveLaneTicket]] = if is_remote {
            &[&self.remote, &self.remote_light]
        } else {
            &[&self.local]
        };
        for list in lists {
            let mut q_idx = 0usize;
            for t in *list {
                if !t.running {
                    q_idx += 1;
                    if t.label == format!("accept {fname}")
                        || t.label == fname
                        || t.label.contains(fname)
                    {
                        return Some((q_idx, t));
                    }
                }
            }
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct MainlineBlockCluster {
    pub file: String,
    pub runs: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LiveContext {
    pub now_ts: i64,
    pub lane: LiveLaneSnapshot,
    pub active_machine_runs: Vec<PathBuf>,
    /// Shared mainline-blocking files per repo where `>= 2` runs failed on the same outside file.
    pub repo_mainline_clusters: HashMap<String, Vec<MainlineBlockCluster>>,
    /// Configured `fill.target` per repo path (if any).
    pub fill_targets: HashMap<String, usize>,
    /// Optional explicit run directories mapped by run id (used in tests & cross-repo lookups).
    pub run_dir_index: HashMap<String, PathBuf>,
    /// Precomputed run status objects (e.g. from `snapshot --json` or `index.json`), avoiding
    /// redundant `runs::status` reads on hot zero-lock/zero-git paths.
    pub precomputed_statuses: HashMap<PathBuf, Value>,
    /// When `true`, never fall back to `runs::resolve_head` (which may probe git top-level).
    pub no_git_fallback: bool,
}

impl LiveContext {
    pub fn capture(state: &Path, now_ts: i64, visible_runs: &[PathBuf]) -> Self {
        Self::capture_inner(state, now_ts, visible_runs, HashMap::new(), None, false)
    }

    /// Strictly zero-lock, zero-disk-write, zero-git live context capture for `snapshot --json`
    /// and Stop hook evaluation. Reuses an already-read lane status JSON and cached run statuses.
    pub fn capture_readonly(
        state: &Path,
        now_ts: i64,
        visible_runs: &[PathBuf],
        precomputed_statuses: HashMap<PathBuf, Value>,
        lane_json: Option<&Value>,
    ) -> Self {
        Self::capture_inner(state, now_ts, visible_runs, precomputed_statuses, lane_json, true)
    }

    fn capture_inner(
        state: &Path,
        now_ts: i64,
        visible_runs: &[PathBuf],
        precomputed_statuses: HashMap<PathBuf, Value>,
        lane_json: Option<&Value>,
        no_git_fallback: bool,
    ) -> Self {
        let lane = match lane_json {
            Some(v) if v.is_object() => LiveLaneSnapshot::from_status_json(v),
            _ => LiveLaneSnapshot::capture(),
        };
        let active_machine_runs = launch::machine_runs(state);
        let mut all_paths: Vec<PathBuf> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for p in visible_runs.iter().chain(active_machine_runs.iter()) {
            if seen.insert(p.clone()) {
                all_paths.push(p.clone());
            }
        }
        let mut run_dir_index = HashMap::new();
        let mut repo_set = HashSet::new();
        for p in &all_paths {
            let meta = json(p.join("meta.json"));
            let rid = run_id_from(p, &meta);
            if !rid.is_empty() {
                run_dir_index.insert(rid, p.clone());
            }
            let repo = repo_from_meta(&meta);
            if !repo.is_empty() {
                repo_set.insert(repo);
            }
        }

        let repo_mainline_clusters = detect_mainline_block_clusters(&all_paths);

        let mut fill_targets = HashMap::new();
        for repo in repo_set {
            if let Ok(cfg) = config::load(Some(Path::new(&repo))) {
                if let Some(fc) = config::parse_fill_config(cfg.value.get("fill")) {
                    fill_targets.insert(repo, fc.target);
                }
            }
        }

        Self {
            now_ts,
            lane,
            active_machine_runs,
            repo_mainline_clusters,
            fill_targets,
            run_dir_index,
            precomputed_statuses,
            no_git_fallback,
        }
    }

    pub fn mainline_cluster_for_run(&self, repo: &str, run_id: &str) -> Option<&MainlineBlockCluster> {
        let clusters = self.repo_mainline_clusters.get(repo)?;
        clusters
            .iter()
            .filter(|c| c.runs.iter().any(|r| r == run_id))
            .max_by_key(|c| c.runs.len())
    }

    pub fn fill_target_for_repo(&self, repo: &str) -> Option<usize> {
        if let Some(&t) = self.fill_targets.get(repo) {
            return Some(t);
        }
        if !repo.is_empty() {
            if let Ok(cfg) = config::load(Some(Path::new(repo))) {
                if let Some(fc) = config::parse_fill_config(cfg.value.get("fill")) {
                    return Some(fc.target);
                }
            }
        }
        None
    }

    fn resolve_live_run(&self, rid: &str, preferred_dirs: &BTreeMap<String, String>) -> Option<PathBuf> {
        if let Some(p) = preferred_dirs.get(rid) {
            let pb = PathBuf::from(p);
            if pb.join("meta.json").is_file() {
                return Some(pb);
            }
        }
        if let Some(pb) = self.run_dir_index.get(rid) {
            if pb.join("meta.json").is_file() {
                return Some(pb.clone());
            }
        }
        for pb in self.run_dir_index.values() {
            if pb.file_name().and_then(|n| n.to_str()) == Some(rid) && pb.join("meta.json").is_file() {
                return Some(pb.clone());
            }
        }
        if !self.no_git_fallback {
            if let Ok(pb) = runs::resolve_head(rid) {
                if pb.join("meta.json").is_file() {
                    return Some(pb);
                }
            }
        }
        None
    }
}

/// Detect repo-wide mainline blocks (design.md §3.3 path ②):
/// When `>= 2` unapplied runs in the same repo have `premerge` failure or `accept`
/// `failureAttribution.outsideRunFiles` / `mainlineBlock.blockingFiles` pointing to
/// the same file outside the run's own changes, group them into a `MainlineBlockCluster`.
pub fn detect_mainline_block_clusters(run_dirs: &[PathBuf]) -> HashMap<String, Vec<MainlineBlockCluster>> {
    let mut by_repo_file: HashMap<(String, String), BTreeSet<String>> = HashMap::new();

    for run in run_dirs {
        if run.join(".applied").exists() || run.join(".applied-cleaned").exists() {
            continue;
        }
        let meta = json(run.join("meta.json"));
        if !meta.is_object() {
            continue;
        }
        let repo = repo_from_meta(&meta);
        if repo.is_empty() {
            continue;
        }
        let rid = run_id_from(run, &meta);
        if rid.is_empty() {
            continue;
        }
        let sum = json(run.join("summary.json"));
        let pm = json(run.join("premerge.json"));

        let mut outside_files: BTreeSet<String> = BTreeSet::new();
        // 1. summary.accept.failureAttribution.outsideRunFiles
        for f in strings(&sum["accept"]["failureAttribution"]["outsideRunFiles"]) {
            if !f.is_empty() {
                outside_files.insert(f);
            }
        }
        // 2. summary.mainlineBlock.blockingFiles
        if b(&sum["mainlineBlock"], "blocked") || s(&sum, "state") == "blocked_mainline" {
            for f in strings(&sum["mainlineBlock"]["blockingFiles"]) {
                if !f.is_empty() {
                    outside_files.insert(f);
                }
            }
        }
        // 3. premerge.json failureAttribution.outsideRunFiles (when status == failed)
        if s(&pm, "status") == "failed" {
            for f in strings(&pm["failureAttribution"]["outsideRunFiles"]) {
                if !f.is_empty() {
                    outside_files.insert(f);
                }
            }
            for f in strings(&pm["accept"]["failureAttribution"]["outsideRunFiles"]) {
                if !f.is_empty() {
                    outside_files.insert(f);
                }
            }
        }

        for file in outside_files {
            by_repo_file
                .entry((repo.clone(), file))
                .or_default()
                .insert(rid.clone());
        }
    }

    let mut out: HashMap<String, Vec<MainlineBlockCluster>> = HashMap::new();
    for ((repo, file), runs_set) in by_repo_file {
        if runs_set.len() >= 2 {
            out.entry(repo).or_default().push(MainlineBlockCluster {
                file,
                runs: runs_set.into_iter().collect(),
            });
        }
    }
    for clusters in out.values_mut() {
        clusters.sort_by(|a, b| b.runs.len().cmp(&a.runs.len()).then_with(|| a.file.cmp(&b.file)));
    }
    out
}

// ---------------------------------------------------------------------------
// Helpers for inspecting run files without locks
// ---------------------------------------------------------------------------

fn run_id_from(run: &Path, meta: &Value) -> String {
    let r = s(meta, "run");
    if !r.is_empty() {
        return r.to_string();
    }
    run.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string()
}

fn repo_from_meta(meta: &Value) -> String {
    for candidate in [
        s(&meta["worktree"], "source"),
        s(meta, "top"),
        s(meta, "workdir"),
    ] {
        if !candidate.is_empty() {
            return candidate.to_string();
        }
    }
    String::new()
}

fn is_memory_reason(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    reason.contains("内存") || lower.contains("memory") || lower.contains("min_available_mb")
}

struct LaneWaitingInfo {
    label: String,
    since_ts: Option<i64>,
    ahead_hint: Option<usize>,
}

fn detect_lane_waiting(run: &Path) -> Option<LaneWaitingInfo> {
    let entries = fs::read_dir(run).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if !p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .starts_with("lane-waiting-")
        {
            continue;
        }
        let Ok(f) = OpenOptions::new().read(true).open(&p) else {
            continue;
        };
        // Non-blocking shared flock fails iff the waiter process holds LOCK_EX.
        let held = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) != 0 };
        if held {
            let v = json(&p);
            let label = {
                let l = s(&v, "label");
                if l.is_empty() { "lane".to_string() } else { l.to_string() }
            };
            let since_ts = v["epoch"]
                .as_f64()
                .map(|e| e as i64)
                .or_else(|| iso_epoch(s(&v, "since")));
            let ahead_hint = last_queue_ahead_from_run_events(run);
            return Some(LaneWaitingInfo {
                label,
                since_ts,
                ahead_hint,
            });
        }
    }
    None
}

fn last_queue_ahead_from_run_events(run: &Path) -> Option<usize> {
    let evs = runs::events(run);
    for e in evs.iter().rev() {
        if s(e, "e") == "queue" {
            let a = n(e, "ahead");
            if a > 0 {
                return Some(a as usize);
            }
        }
        if matches!(s(e, "e"), "settled" | "result" | "bash_done") {
            break;
        }
    }
    None
}

fn accept_log_running_since(run: &Path) -> Option<i64> {
    let path = run.join("accept.log");
    if !path.is_file() {
        return None;
    }
    let text = read(&path);
    if text.contains("\n[exit ") || text.starts_with("[exit ") {
        return None;
    }
    fs::metadata(&path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

fn state_label_zh(state: &str) -> &'static str {
    match state {
        "rejected" => "验收未过",
        "failed" => "失败",
        "timeout" => "超时",
        "stalled" => "卡住",
        "killed" => "已中止",
        "crashed" => "异常退出",
        "malformed" => "格式异常",
        "needs-redispatch" => "需重派",
        _ => "待处理",
    }
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

// ---------------------------------------------------------------------------
// Single-run ETA estimation
// ---------------------------------------------------------------------------

/// Estimate remaining time for a single run, with stop-clock blocker attribution
/// and real-time lane/capacity queue adjustment.
pub fn estimate_run_in(run: &Path, model: &EtaModel, ctx: &LiveContext) -> RunEta {
    let pre = ctx.precomputed_statuses.get(run);
    estimate_run_inner(run, pre, model, ctx, false)
}

/// Estimate remaining time for a single run, reusing an already-computed `status` JSON object
/// to avoid redundant disk reads on hot read paths (`status`, `work show`, `snapshot --json`, `hook_check`).
pub fn estimate_run_with_status(
    run: &Path,
    status: &Value,
    model: &EtaModel,
    ctx: &LiveContext,
) -> RunEta {
    estimate_run_inner(run, Some(status), model, ctx, false)
}

/// Overlay a structured `eta` object onto a `status` JSON map (skipping `cleaned`,
/// `stopped`, and `Stage::Done` runs). Strictly zero-lock and zero-token.
pub fn overlay_status_eta(
    status: &mut Value,
    run: &Path,
    model: &EtaModel,
    ctx: &LiveContext,
) {
    if !status.is_object() || b(status, "cleaned") || s(status, "state") == "stopped" {
        return;
    }
    let re = estimate_run_with_status(run, status, model, ctx);
    if re.stage != Stage::Done {
        status["eta"] = re.to_json();
    }
}

/// Lightweight zero-git, zero-lock fallback ETA for a live run known only via an
/// index status object (e.g., when `<run>/meta.json` is unavailable on disk).
pub fn estimate_live_status_fallback(
    status: &Value,
    model: &EtaModel,
    now_ts: i64,
) -> Option<RunEta> {
    let state = s(status, "state").to_string();
    if !runs::active(&state) || status["stallWarning"].is_object() {
        return None;
    }
    let raw_mode = {
        let m = s(status, "mode");
        if m.is_empty() { "write" } else { m }
    };
    let raw_class = s(status, "class");
    let raw_tier = s(status, "tier");
    let (norm_mode, norm_class) = speed::normalize_mode_class(raw_mode, raw_class, raw_tier);
    let is_write = norm_mode == "write";
    let repo = {
        let r = s(status, "repo");
        if !r.is_empty() {
            r.to_string()
        } else {
            s(status, "top").to_string()
        }
    };
    let elapsed = status["elapsedSeconds"]
        .as_f64()
        .or_else(|| status["ageSeconds"].as_f64())
        .unwrap_or(0.0)
        .max(0.0);
    let resolved_bucket = model.resolve_bucket(&repo, &norm_mode, &norm_class);
    let work_d = model.resolve_dist(&repo, &norm_mode, &norm_class, |b| &b.agent_work);
    let (w50, w75, anom) = work_d.dist.conditional_residual(elapsed).unwrap_or(if is_write {
        (30.0 * 60.0, 45.0 * 60.0, false)
    } else {
        (5.0 * 60.0, 10.0 * 60.0, false)
    });
    let (rem50, rem75) = if is_write {
        let suf = model.resolve_dist(&repo, &norm_mode, &norm_class, |b| &b.suffix_after_work);
        let s50 = suf.dist.p50_sec.unwrap_or(0.0);
        let s75 = suf.dist.p75_sec.unwrap_or(s50).max(s50);
        combine_p50_p75(&[(w50, w75), (s50, s75)])
    } else {
        (w50, w75)
    };
    let to_delivered = EtaRange::from_remaining(now_ts, rem50, rem75, anom);
    let target_milestone = if is_write { "delivered" } else { "answered" };
    let verb = if is_write { "交付" } else { "答复" };
    let compact_badge = format_compact_clock(to_delivered.rounded_p50_at, now_ts, to_delivered.p50_sec);
    let short_text = format!(
        "预计 {} {verb}（p75 {}）",
        format_clock(to_delivered.rounded_p50_at, now_ts),
        format_clock(to_delivered.rounded_p75_at, now_ts),
    );
    let run_id = s(status, "run").to_string();
    let name = {
        let nm = s(status, "name");
        if nm.is_empty() { run_id.clone() } else { nm.to_string() }
    };
    Some(RunEta {
        run: run_id,
        name,
        repo,
        mode: norm_mode,
        class: norm_class,
        state,
        stage: Stage::AgentWork,
        stage_label: Stage::AgentWork.label_zh().to_string(),
        stage_elapsed_sec: Some(elapsed),
        round: status["chainRound"].as_u64().unwrap_or(1).max(1) as usize,
        target_milestone,
        to_delivered: Some(to_delivered),
        to_applied: None,
        blocker: None,
        lane_wait: None,
        capacity_wait: None,
        fallback_level: work_d.level,
        bucket_key: work_d.key,
        is_prior: work_d.level.is_prior(),
        rework_rate: resolved_bucket.bucket.auto_rework_rate,
        short_text: short_text.clone(),
        compact_badge,
        detail_text: short_text,
        now_ts,
    })
}

/// Convenience wrapper loading `<state>/eta-model.json` (lazily rebuilding if stale)
/// and capturing live lane/machine context once.
#[allow(dead_code)] // Consumed by Chain 3/4 callers.
pub fn estimate_run(run: &Path) -> RunEta {
    let state = state_dir();
    let now_ts = epoch() as i64;
    let model = speed::load_or_refresh_eta_model_in(&state, now_ts, true);
    let visible = vec![run.to_path_buf()];
    let ctx = LiveContext::capture(&state, now_ts, &visible);
    estimate_run_in(run, &model, &ctx)
}

fn estimate_run_inner(
    run: &Path,
    precomputed_status: Option<&Value>,
    model: &EtaModel,
    ctx: &LiveContext,
    in_capacity_recursion: bool,
) -> RunEta {
    let now_ts = ctx.now_ts;
    let meta = json(run.join("meta.json"));
    let owned_status;
    let status: &Value = if let Some(st) = precomputed_status {
        st
    } else if let Some(st) = ctx.precomputed_statuses.get(run) {
        st
    } else {
        owned_status = runs::status(run);
        &owned_status
    };
    let run_id = run_id_from(run, &meta);
    let name = {
        let nm = s(&meta, "name");
        if nm.is_empty() { run_id.clone() } else { nm.to_string() }
    };
    let repo = repo_from_meta(&meta);
    let raw_mode = {
        let m = s(&meta, "mode");
        if m.is_empty() { "write" } else { m }
    };
    let raw_class = s(&meta, "class");
    let raw_tier = s(&meta, "tier");
    let (norm_mode, norm_class) = speed::normalize_mode_class(raw_mode, raw_class, raw_tier);
    let is_write = norm_mode == "write";
    let state = s(status, "state").to_string();

    let resolved_bucket = model.resolve_bucket(&repo, &norm_mode, &norm_class);
    let rework_rate = resolved_bucket.bucket.auto_rework_rate;
    let target_milestone = if is_write { "delivered" } else { "answered" };

    // Chain round count (reply continuation or auto-rework).
    let mut round = status["chainRound"].as_u64().unwrap_or(1).max(1) as usize;
    let rev_round = status["review"]["round"].as_u64().unwrap_or(0) as usize;
    let rw_rounds = status["review"]["reworkRounds"].as_u64().unwrap_or(0) as usize;
    round = round.max(rev_round).max(rw_rounds + 1);

    let make_stopped = |stage: Stage, blocker: BlockerInfo, already_delivered: bool| -> RunEta {
        let short_text = blocker.label.clone();
        let compact_badge = blocker.kind.compact_badge().to_string();
        let detail_text = format!("停表：{}", blocker.label);
        let to_delivered = if already_delivered {
            Some(EtaRange::from_remaining(now_ts, 0.0, 0.0, false))
        } else {
            None
        };
        RunEta {
            run: run_id.clone(),
            name: name.clone(),
            repo: repo.clone(),
            mode: norm_mode.clone(),
            class: norm_class.clone(),
            state: state.clone(),
            stage,
            stage_label: stage.label_zh().to_string(),
            stage_elapsed_sec: blocker.since_sec.map(|s| s as f64),
            round,
            target_milestone,
            to_delivered,
            to_applied: None,
            blocker: Some(blocker),
            lane_wait: None,
            capacity_wait: None,
            fallback_level: resolved_bucket.level,
            bucket_key: resolved_bucket.key.clone(),
            is_prior: resolved_bucket.level.is_prior(),
            rework_rate,
            short_text,
            compact_badge,
            detail_text,
            now_ts,
        }
    };

    // 0. Done check: applied write run, stopped/cleaned run, or answered/delivered read-only run (without question).
    let applied = b(status, "applied") || run.join(".applied").exists();
    let is_stopped_or_cleaned =
        matches!(state.as_str(), "stopped" | "cleaned") || b(status, "cleaned");
    if applied
        || is_stopped_or_cleaned
        || (!is_write && matches!(state.as_str(), "answered" | "delivered") && !b(status, "question"))
    {
        let done_label = if applied {
            "已合入"
        } else if state == "stopped" {
            "已停止"
        } else if is_stopped_or_cleaned {
            "已清理"
        } else {
            "已答复"
        };
        return RunEta {
            run: run_id,
            name,
            repo,
            mode: norm_mode,
            class: norm_class,
            state,
            stage: Stage::Done,
            stage_label: Stage::Done.label_zh().to_string(),
            stage_elapsed_sec: None,
            round,
            target_milestone: "none",
            to_delivered: Some(EtaRange::from_remaining(now_ts, 0.0, 0.0, false)),
            to_applied: if applied {
                Some(EtaRange::from_remaining(now_ts, 0.0, 0.0, false))
            } else {
                None
            },
            blocker: None,
            lane_wait: None,
            capacity_wait: None,
            fallback_level: resolved_bucket.level,
            bucket_key: resolved_bucket.key,
            is_prior: resolved_bucket.level.is_prior(),
            rework_rate,
            short_text: done_label.into(),
            compact_badge: "完成".into(),
            detail_text: done_label.into(),
            now_ts,
        };
    }

    let is_active = runs::active(&state);
    let finished_ts = iso_epoch(s(status, "finishedAt"));
    let stopped_age = finished_ts.map(|t| (now_ts - t).max(0));

    // 1. Stop-clock checks for active runs: stallWarning (cached or live file) or memory capacity wait.
    let live_stall = if is_active && !status["stallWarning"].is_object() {
        let p = run.join("stall-warning.json");
        if p.is_file() {
            let v = json(p);
            v.is_object().then_some(v)
        } else {
            None
        }
    } else {
        None
    };
    let stall_obj = if status["stallWarning"].is_object() {
        Some(&status["stallWarning"])
    } else {
        live_stall.as_ref()
    };
    if is_active {
        if let Some(sw) = stall_obj {
            let stall_sec = sw["elapsedSeconds"]
                .as_i64()
                .or_else(|| sw["idleSeconds"].as_i64());
            return make_stopped(
                Stage::Blocked,
                BlockerInfo {
                    kind: BlockerKind::StallWarning,
                    who: "疑似卡住".into(),
                    reason: "停表".into(),
                    since_sec: stall_sec,
                    target: None,
                    affected_count: None,
                    unlock_at: None,
                    after_unlock_p50_sec: None,
                    after_unlock_p75_sec: None,
                    label: "⚠ 疑似卡住·停表".into(),
                },
                false,
            );
        }
    }

    // 2. Non-active runs: either S7 premerge running, or stopped-clock attributed to someone.
    if !is_active {
        if b(status, "question") || state == "question" {
            let age_str = stopped_age.map(|a| format!(" {}", format_age_short(a))).unwrap_or_default();
            return make_stopped(
                Stage::Blocked,
                BlockerInfo {
                    kind: BlockerKind::ControllerQuestion,
                    who: "主控".into(),
                    reason: "提问".into(),
                    since_sec: stopped_age,
                    target: None,
                    affected_count: None,
                    unlock_at: None,
                    after_unlock_p50_sec: None,
                    after_unlock_p75_sec: None,
                    label: format!("卡在主控·提问{age_str}"),
                },
                false,
            );
        }

        // Mainline blocked (explicit state or shared >=2 repo cluster)
        let cluster = ctx.mainline_cluster_for_run(&repo, &run_id);
        if state == "blocked_mainline" || b(&status["mainlineBlock"], "blocked") {
            let n_aff = cluster.map(|c| c.runs.len()).unwrap_or(1);
            let target_file = cluster
                .map(|c| c.file.clone())
                .or_else(|| strings(&status["mainlineBlock"]["blockingFiles"]).into_iter().next());
            let label = if n_aff >= 2 {
                format!("⚠ 卡在主干阻塞（{n_aff} 条受阻）")
            } else {
                "⚠ 卡在主干阻塞".to_string()
            };
            return make_stopped(
                Stage::Blocked,
                BlockerInfo {
                    kind: BlockerKind::MainlineBlocked,
                    who: "主干阻塞".into(),
                    reason: target_file.clone().unwrap_or_else(|| "主干基线失败".into()),
                    since_sec: stopped_age,
                    target: target_file,
                    affected_count: Some(n_aff),
                    unlock_at: None,
                    after_unlock_p50_sec: None,
                    after_unlock_p75_sec: None,
                    label,
                },
                false,
            );
        }

        let pm = &status["premerge"];
        let pm_status = s(pm, "status");

        if matches!(state.as_str(), "delivered" | "answered") && is_write {
            if pm_status == "conflict" {
                return make_stopped(
                    Stage::Blocked,
                    BlockerInfo {
                        kind: BlockerKind::PremergeConflict,
                        who: "预合并冲突".into(),
                        reason: "需解冲突".into(),
                        since_sec: stopped_age,
                        target: None,
                        affected_count: None,
                        unlock_at: None,
                        after_unlock_p50_sec: None,
                        after_unlock_p75_sec: None,
                        label: "卡在预合并冲突·需解冲突".into(),
                    },
                    true,
                );
            }
            if pm_status == "failed" {
                if let Some(cl) = cluster {
                    return make_stopped(
                        Stage::Blocked,
                        BlockerInfo {
                            kind: BlockerKind::MainlineBlocked,
                            who: "主干阻塞".into(),
                            reason: cl.file.clone(),
                            since_sec: stopped_age,
                            target: Some(cl.file.clone()),
                            affected_count: Some(cl.runs.len()),
                            unlock_at: None,
                            after_unlock_p50_sec: None,
                            after_unlock_p75_sec: None,
                            label: format!("⚠ 卡在主干阻塞（{} 条受阻）", cl.runs.len()),
                        },
                        true,
                    );
                }
                return make_stopped(
                    Stage::Blocked,
                    BlockerInfo {
                        kind: BlockerKind::PremergeFailed,
                        who: "预合并失败".into(),
                        reason: "预合并验收失败".into(),
                        since_sec: stopped_age,
                        target: None,
                        affected_count: None,
                        unlock_at: None,
                        after_unlock_p50_sec: None,
                        after_unlock_p75_sec: None,
                        label: "卡在预合并失败".into(),
                    },
                    true,
                );
            }
            if pm_status == "running" {
                // S7 Premerge running in background!
                let pm_doc = json(run.join("premerge.json"));
                let pm_started = iso_epoch(s(&pm_doc, "startedAt")).unwrap_or(now_ts);
                let pm_elapsed = (now_ts - pm_started).max(0) as f64;
                let pm_dist = resolve_premerge_dist(model, &repo, pm_elapsed <= 15.0);
                let (pm50, pm75, anom) = pm_dist
                    .dist
                    .conditional_residual(pm_elapsed)
                    .unwrap_or((60.0, 180.0, false));
                let mw = model.resolve_dist(&repo, "write", &norm_class, |b| &b.merge_wait);
                let mw50 = mw.dist.p50_sec.unwrap_or(540.0);
                let mw75 = mw.dist.p75_sec.unwrap_or(mw50).max(mw50);
                let (ap50, ap75) = combine_p50_p75(&[(pm50, pm75), (mw50, mw75)]);
                let to_del = EtaRange::from_remaining(now_ts, 0.0, 0.0, false);
                let to_app = EtaRange::from_remaining(now_ts, ap50, ap75, anom);
                let is_prior = pm_dist.level.is_prior();
                let prior_tag = if is_prior { "(初估)" } else { "" };
                let short_text = format!(
                    "预合并中·预计 {} 合入（p75 {}）{prior_tag}",
                    format_clock(to_app.rounded_p50_at, now_ts),
                    format_clock(to_app.rounded_p75_at, now_ts),
                );
                let compact_badge = format_compact_clock(to_app.rounded_p50_at, now_ts, to_app.p50_sec);
                let detail_text = format!(
                    "预估：已交付，预合并中（{}，剩 ~{}），预计 {} 合入（p75 {}）{prior_tag}",
                    format_age_short(pm_elapsed as i64),
                    format_eta_duration(pm50),
                    format_clock(to_app.rounded_p50_at, now_ts),
                    format_clock(to_app.rounded_p75_at, now_ts),
                );
                return RunEta {
                    run: run_id,
                    name,
                    repo,
                    mode: norm_mode,
                    class: norm_class,
                    state,
                    stage: Stage::Premerge,
                    stage_label: Stage::Premerge.label_zh().to_string(),
                    stage_elapsed_sec: Some(pm_elapsed),
                    round,
                    target_milestone: "applied",
                    to_delivered: Some(to_del),
                    to_applied: Some(to_app),
                    blocker: None,
                    lane_wait: None,
                    capacity_wait: None,
                    fallback_level: pm_dist.level,
                    bucket_key: pm_dist.key,
                    is_prior,
                    rework_rate,
                    short_text,
                    compact_badge,
                    detail_text,
                    now_ts,
                };
            }
            if s(&status["review"], "verdict") == "NEEDS_CONTROLLER" {
                let age_str = stopped_age.map(|a| format!(" {}", format_age_short(a))).unwrap_or_default();
                return make_stopped(
                    Stage::Blocked,
                    BlockerInfo {
                        kind: BlockerKind::ControllerReview,
                        who: "主控".into(),
                        reason: "待主控审查".into(),
                        since_sec: stopped_age,
                        target: None,
                        affected_count: None,
                        unlock_at: None,
                        after_unlock_p50_sec: None,
                        after_unlock_p75_sec: None,
                        label: format!("卡在主控·待审查{age_str}"),
                    },
                    true,
                );
            }
            if b(&status["sign"], "required") && !b(&status["sign"], "valid") {
                let age_str = stopped_age.map(|a| format!(" {}", format_age_short(a))).unwrap_or_default();
                return make_stopped(
                    Stage::Blocked,
                    BlockerInfo {
                        kind: BlockerKind::ControllerSign,
                        who: "主控".into(),
                        reason: "待签字".into(),
                        since_sec: stopped_age,
                        target: None,
                        affected_count: None,
                        unlock_at: None,
                        after_unlock_p50_sec: None,
                        after_unlock_p75_sec: None,
                        label: format!("卡在主控·待签字{age_str}"),
                    },
                    true,
                );
            }
            // Delivered & premerge ready (or absent) -> waiting for controller merge
            let age_str = stopped_age.map(|a| format!(" {}", format_age_short(a))).unwrap_or_default();
            return make_stopped(
                Stage::MergeWait,
                BlockerInfo {
                    kind: BlockerKind::ControllerMerge,
                    who: "主控".into(),
                    reason: "待合入".into(),
                    since_sec: stopped_age,
                    target: None,
                    affected_count: None,
                    unlock_at: None,
                    after_unlock_p50_sec: None,
                    after_unlock_p75_sec: None,
                    label: format!("卡在主控·待合入{age_str}"),
                },
                true,
            );
        }

        // Other terminal non-delivered states (rejected, failed, timeout, etc.)
        if let Some(cl) = cluster {
            return make_stopped(
                Stage::Blocked,
                BlockerInfo {
                    kind: BlockerKind::MainlineBlocked,
                    who: "主干阻塞".into(),
                    reason: cl.file.clone(),
                    since_sec: stopped_age,
                    target: Some(cl.file.clone()),
                    affected_count: Some(cl.runs.len()),
                    unlock_at: None,
                    after_unlock_p50_sec: None,
                    after_unlock_p75_sec: None,
                    label: format!("⚠ 卡在主干阻塞（{} 条受阻）", cl.runs.len()),
                },
                false,
            );
        }
        if s(&status["review"], "verdict") == "NEEDS_CONTROLLER" {
            let age_str = stopped_age.map(|a| format!(" {}", format_age_short(a))).unwrap_or_default();
            return make_stopped(
                Stage::Blocked,
                BlockerInfo {
                    kind: BlockerKind::ControllerReview,
                    who: "主控".into(),
                    reason: "待主控审查".into(),
                    since_sec: stopped_age,
                    target: None,
                    affected_count: None,
                    unlock_at: None,
                    after_unlock_p50_sec: None,
                    after_unlock_p75_sec: None,
                    label: format!("卡在主控·待审查{age_str}"),
                },
                false,
            );
        }
        let st_zh = state_label_zh(&state);
        let age_str = stopped_age.map(|a| format!(" {}", format_age_short(a))).unwrap_or_default();
        return make_stopped(
            Stage::Blocked,
            BlockerInfo {
                kind: BlockerKind::ControllerAttention,
                who: "主控".into(),
                reason: st_zh.into(),
                since_sec: stopped_age,
                target: None,
                affected_count: None,
                unlock_at: None,
                after_unlock_p50_sec: None,
                after_unlock_p75_sec: None,
                label: format!("卡在主控·{st_zh}{age_str}"),
            },
            false,
        );
    }

    // 3. Active runs: classify into S1 / S3 / S5 / S6 / S4 / S2 (design.md §3.1 order).
    let is_remote = crate::remote::enabled(&meta["remote"]);
    let rev_status = s(&status["review"], "status");

    let (
        stage,
        stage_label,
        stage_elapsed_sec,
        rem_p50,
        rem_p75,
        anomaly,
        lane_wait_est,
        cap_wait_est,
        used_level,
        used_key,
    ) = if state == "waiting" || run.join(".waiting").exists() {
        // S1 Capacity queue
        let q = queue::status(run).unwrap_or_else(|| json(run.join("queue.json")));
        let reason = s(&q, "reason").to_string();
        if is_memory_reason(&reason) {
            return make_stopped(
                Stage::Blocked,
                BlockerInfo {
                    kind: BlockerKind::MemoryWait,
                    who: "内存".into(),
                    reason: if reason.is_empty() { "等内存释放".into() } else { reason },
                    since_sec: iso_epoch(s(&q, "since")).map(|t| (now_ts - t).max(0)),
                    target: None,
                    affected_count: None,
                    unlock_at: None,
                    after_unlock_p50_sec: None,
                    after_unlock_p75_sec: None,
                    label: "等内存释放".into(),
                },
                false,
            );
        }
        let ahead = q["ahead"].as_u64().unwrap_or(0) as usize;
        let q_elapsed = iso_epoch(s(&q, "since"))
            .map(|t| (now_ts - t).max(0) as f64)
            .unwrap_or(0.0);
        let (w50, w75) = estimate_capacity_wait(
            &reason,
            ahead,
            &repo,
            s(&meta, "model"),
            model,
            ctx,
            in_capacity_recursion,
        );
        let e2e = model.resolve_dist(&repo, &norm_mode, &norm_class, |b| &b.run_to_delivered);
        let d50 = e2e.dist.p50_sec.unwrap_or(if is_write { 54.0 * 60.0 } else { 5.0 * 60.0 });
        let d75 = e2e.dist.p75_sec.unwrap_or(d50).max(d50);
        let (tot50, tot75) = combine_p50_p75(&[(w50, w75), (d50, d75)]);
        let cw = CapacityWaitEstimate {
            reason: if reason.is_empty() { "容量排队".into() } else { reason },
            ahead,
            wait_p50_sec: w50,
            wait_p75_sec: w75,
        };
        let lbl = if ahead > 0 {
            format!("排队(前{ahead})")
        } else {
            "排队中".to_string()
        };
        (
            Stage::CapacityQueue,
            lbl,
            Some(q_elapsed),
            tot50,
            tot75,
            false,
            None,
            Some(cw),
            e2e.level,
            e2e.key,
        )
    } else if let Some(lw) = detect_lane_waiting(run).or_else(|| {
        ctx.lane
            .queued_ticket_for_run(run, is_remote)
            .map(|(q_pos, t)| LaneWaitingInfo {
                label: t.label.clone(),
                since_ts: t.since_ts,
                ahead_hint: Some(q_pos),
            })
    }) {
        // S3 Lane queue (real-time holder + ahead adjustment)
        let lane_held = model.resolve_lane_held(&repo);
        let (max_heavy, holders_elapsed, ahead) =
            ctx.lane
                .holders_and_queue_pos(is_remote, &lw.label, lw.ahead_hint, now_ts);
        let (w50, w75) = compute_lane_wait(lane_held.dist, &holders_elapsed, max_heavy, ahead);
        let acc = resolve_run_accept_dist(run, &meta, &repo, model);
        let a50 = acc.dist.p50_sec.unwrap_or(180.0);
        let a75 = acc.dist.p75_sec.unwrap_or(a50).max(a50);
        let suf = model.resolve_dist(&repo, &norm_mode, &norm_class, |b| &b.suffix_after_accept);
        let s50 = if is_write { suf.dist.p50_sec.unwrap_or(0.0) } else { 0.0 };
        let s75 = if is_write { suf.dist.p75_sec.unwrap_or(s50).max(s50) } else { 0.0 };
        let (tot50, tot75) = combine_p50_p75(&[(w50, w75), (a50, a75), (s50, s75)]);
        let lw_est = LaneWaitEstimate {
            ahead,
            holders: holders_elapsed.len(),
            max_heavy,
            wait_p50_sec: w50,
            wait_p75_sec: w75,
        };
        let elapsed = lw.since_ts.map(|t| (now_ts - t).max(0) as f64);
        (
            Stage::LaneQueue,
            format!("等 lane(前{ahead})"),
            elapsed,
            tot50,
            tot75,
            false,
            Some(lw_est),
            None,
            acc.level,
            acc.key,
        )
    } else if rev_status == "reviewing" {
        // S5 Parallel slow-accept + review
        let rev_doc = json(run.join("review.json"));
        let rev_start = iso_epoch(s(&rev_doc, "startedAt"))
            .or_else(|| iso_epoch(s(&rev_doc, "updatedAt")))
            .unwrap_or(now_ts);
        let elapsed = (now_ts - rev_start).max(0) as f64;
        let par = model.resolve_dist(&repo, &norm_mode, &norm_class, |b| &b.accept_review_parallel);
        let (r50, r75, anom) = par
            .dist
            .conditional_residual(elapsed)
            .unwrap_or((300.0, 540.0, false));
        let suf = model.resolve_dist(&repo, &norm_mode, &norm_class, |b| &b.suffix_after_review);
        let s50 = suf.dist.p50_sec.unwrap_or(0.0);
        let s75 = suf.dist.p75_sec.unwrap_or(s50).max(s50);
        let (tot50, tot75) = combine_p50_p75(&[(r50, r75), (s50, s75)]);
        (
            Stage::ReviewParallel,
            Stage::ReviewParallel.label_zh().to_string(),
            Some(elapsed),
            tot50,
            tot75,
            anom,
            None,
            None,
            par.level,
            par.key,
        )
    } else if rev_status == "reworking" {
        // S6 Rework round k
        let rev_doc = json(run.join("review.json"));
        let rw_start = iso_epoch(s(&rev_doc, "reworkStartedAt"))
            .or_else(|| iso_epoch(s(&rev_doc, "updatedAt")))
            .unwrap_or(now_ts);
        let elapsed = (now_ts - rw_start).max(0) as f64;
        let rw = model.resolve_dist(&repo, &norm_mode, &norm_class, |b| &b.suffix_after_rework);
        let (tot50, tot75, anom) = rw
            .dist
            .conditional_residual(elapsed)
            .unwrap_or((15.0 * 60.0, 25.0 * 60.0, false));
        let k = round.max(2);
        (
            Stage::Rework,
            format!("返工第{k}轮"),
            Some(elapsed),
            tot50,
            tot75,
            anom,
            None,
            None,
            rw.level,
            rw.key,
        )
    } else if let Some(acc_started) = ctx
        .lane
        .running_ticket_for_run(run, is_remote)
        .and_then(|t| t.since_ts)
        .or_else(|| accept_log_running_since(run))
    {
        // S4 Accepting (holding lane or running accept.log)
        let elapsed = (now_ts - acc_started).max(0) as f64;
        let acc = resolve_run_accept_dist(run, &meta, &repo, model);
        let (a50, a75, anom) = acc
            .dist
            .conditional_residual(elapsed)
            .unwrap_or((180.0, 360.0, false));
        let suf = model.resolve_dist(&repo, &norm_mode, &norm_class, |b| &b.suffix_after_accept);
        let s50 = if is_write { suf.dist.p50_sec.unwrap_or(0.0) } else { 0.0 };
        let s75 = if is_write { suf.dist.p75_sec.unwrap_or(s50).max(s50) } else { 0.0 };
        let (tot50, tot75) = combine_p50_p75(&[(a50, a75), (s50, s75)]);
        (
            Stage::Accept,
            Stage::Accept.label_zh().to_string(),
            Some(elapsed),
            tot50,
            tot75,
            anom,
            None,
            None,
            acc.level,
            acc.key,
        )
    } else {
        // S2 Model working
        let started_ts = meta["startedEpoch"]
            .as_f64()
            .or_else(|| iso_epoch(s(&meta, "startedAt")).map(|t| t as f64))
            .unwrap_or(now_ts as f64);
        let elapsed = (now_ts as f64 - started_ts).max(0.0);
        let work_d = model.resolve_dist(&repo, &norm_mode, &norm_class, |b| &b.agent_work);
        let (w50, w75, anom) = work_d.dist.conditional_residual(elapsed).unwrap_or(if is_write {
            (30.0 * 60.0, 45.0 * 60.0, false)
        } else {
            (5.0 * 60.0, 10.0 * 60.0, false)
        });
        let (tot50, tot75) = if is_write {
            let suf = model.resolve_dist(&repo, &norm_mode, &norm_class, |b| &b.suffix_after_work);
            let s50 = suf.dist.p50_sec.unwrap_or(0.0);
            let s75 = suf.dist.p75_sec.unwrap_or(s50).max(s50);
            combine_p50_p75(&[(w50, w75), (s50, s75)])
        } else {
            (w50, w75)
        };
        (
            Stage::AgentWork,
            Stage::AgentWork.label_zh().to_string(),
            Some(elapsed),
            tot50,
            tot75,
            anom,
            None,
            None,
            work_d.level,
            work_d.key,
        )
    };

    let to_delivered = EtaRange::from_remaining(now_ts, rem_p50, rem_p75, anomaly);
    let to_applied = if is_write {
        let pm = resolve_premerge_dist(model, &repo, true);
        let pm50 = pm.dist.p50_sec.unwrap_or(10.0);
        let pm75 = pm.dist.p75_sec.unwrap_or(pm50).max(pm50);
        let mw = model.resolve_dist(&repo, "write", &norm_class, |b| &b.merge_wait);
        let mw50 = mw.dist.p50_sec.unwrap_or(540.0);
        let mw75 = mw.dist.p75_sec.unwrap_or(mw50).max(mw50);
        let (ap50, ap75) = combine_p50_p75(&[(rem_p50, rem_p75), (pm50, pm75), (mw50, mw75)]);
        Some(EtaRange::from_remaining(now_ts, ap50, ap75, anomaly))
    } else {
        None
    };

    let is_prior = used_level.is_prior();
    let prior_tag = if is_prior { "(初估)" } else { "" };
    let anom_tag = if anomaly { "·超常" } else { "" };
    let verb = if is_write { "交付" } else { "答复" };

    let short_text = if !is_write {
        if to_delivered.p50_sec < 300.0 {
            format!("预计 <5m {verb}{anom_tag}{prior_tag}")
        } else {
            format!(
                "预计 {} {verb}{anom_tag}{prior_tag}",
                format_clock(to_delivered.rounded_p50_at, now_ts)
            )
        }
    } else if stage == Stage::LaneQueue {
        format!(
            "{stage_label}·预计 {} {verb}（p75 {}）{anom_tag}{prior_tag}",
            format_clock(to_delivered.rounded_p50_at, now_ts),
            format_clock(to_delivered.rounded_p75_at, now_ts),
        )
    } else if to_delivered.p50_sec < 300.0 && to_delivered.p75_sec < 600.0 {
        format!("预计 <5m {verb}{anom_tag}{prior_tag}")
    } else {
        format!(
            "预计 {} {verb}（p75 {}）{anom_tag}{prior_tag}",
            format_clock(to_delivered.rounded_p50_at, now_ts),
            format_clock(to_delivered.rounded_p75_at, now_ts),
        )
    };

    let compact_badge = format_compact_clock(to_delivered.rounded_p50_at, now_ts, to_delivered.p50_sec);

    let elapsed_part = stage_elapsed_sec
        .map(|e| format!(" {}", format_age_short(e as i64)))
        .unwrap_or_default();
    let rework_part = if is_write {
        rework_rate
            .map(|r| format!("；返工率 {:.0}%", r * 100.0))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let applied_part = to_applied
        .as_ref()
        .map(|a| format!("；合入约 {}", format_clock(a.rounded_p50_at, now_ts)))
        .unwrap_or_default();
    let detail_text = format!(
        "预估：{} {verb}（p75 {}）〔{stage_label}{elapsed_part}，剩 ~{}{rework_part}{applied_part}〕{anom_tag}{prior_tag}",
        format_clock(to_delivered.rounded_p50_at, now_ts),
        format_clock(to_delivered.rounded_p75_at, now_ts),
        format_eta_duration(to_delivered.p50_sec),
    );

    RunEta {
        run: run_id,
        name,
        repo,
        mode: norm_mode,
        class: norm_class,
        state,
        stage,
        stage_label,
        stage_elapsed_sec,
        round,
        target_milestone,
        to_delivered: Some(to_delivered),
        to_applied,
        blocker: None,
        lane_wait: lane_wait_est,
        capacity_wait: cap_wait_est,
        fallback_level: used_level,
        bucket_key: used_key,
        is_prior,
        rework_rate,
        short_text,
        compact_badge,
        detail_text,
        now_ts,
    }
}

fn resolve_premerge_dist<'a>(model: &'a EtaModel, repo: &str, reused: bool) -> speed::ResolvedDist<'a> {
    if let Some(rm) = model.repo_model(repo) {
        let d = if reused { &rm.premerge_reused } else { &rm.premerge_rerun };
        if d.count >= speed::MIN_BUCKET_N {
            return speed::ResolvedDist {
                level: FallbackLevel::RepoMode,
                key: format!("{}:premerge", rm.repo_name),
                dist: d,
            };
        }
    }
    let gd = if reused {
        &model.global.premerge_reused
    } else {
        &model.global.premerge_rerun
    };
    if gd.count >= speed::MIN_GLOBAL_MODE_N {
        return speed::ResolvedDist {
            level: FallbackLevel::GlobalMode,
            key: "all:premerge".into(),
            dist: gd,
        };
    }
    speed::ResolvedDist {
        level: FallbackLevel::Prior,
        key: "prior:premerge".into(),
        dist: if reused {
            &model.priors.write.premerge_reused
        } else {
            &model.priors.write.premerge_rerun
        },
    }
}

fn resolve_run_accept_dist<'a>(
    run: &Path,
    meta: &Value,
    repo: &str,
    model: &'a EtaModel,
) -> speed::ResolvedDist<'a> {
    let acc_doc = json(run.join("accept.json"));
    if acc_doc.is_object() {
        let tiered = b(&acc_doc, "tiered");
        let fail_closed = b(&acc_doc, "failClosed");
        let ran_layers = strings(&acc_doc["ranLayers"]);
        return model.resolve_accept_dist(repo, tiered, fail_closed, &ran_layers);
    }
    let tiered = meta["acceptRules"]
        .as_array()
        .is_some_and(|a| !a.is_empty());
    if tiered {
        return model.resolve_accept_dist(repo, true, false, &[]);
    }
    model.resolve_dist(repo, "write", s(meta, "class"), |b| &b.accept)
}

fn estimate_capacity_wait(
    reason: &str,
    ahead: usize,
    repo: &str,
    model_id: &str,
    model: &EtaModel,
    ctx: &LiveContext,
    in_recursion: bool,
) -> (f64, f64) {
    if !in_recursion {
        let lower = reason.to_ascii_lowercase();
        let scope_model = reason.contains("模型") || lower.contains("model");
        let scope_repo = reason.contains("仓库") || lower.contains("repo");

        let mut active_rems: Vec<(f64, f64)> = Vec::new();
        for other in &ctx.active_machine_runs {
            if other.join(".waiting").exists() {
                continue;
            }
            let ometa = json(other.join("meta.json"));
            if !ometa.is_object() {
                continue;
            }
            if scope_model && !model_id.is_empty() && s(&ometa, "model") != model_id {
                continue;
            }
            if scope_repo && !repo.is_empty() && repo_from_meta(&ometa) != repo {
                continue;
            }
            let oeta = estimate_run_inner(other, ctx.precomputed_statuses.get(other), model, ctx, true);
            if let Some(d) = oeta.to_delivered {
                active_rems.push((d.p50_sec, d.p75_sec));
            }
        }
        if !active_rems.is_empty() {
            active_rems.sort_by(|a, b| a.0.total_cmp(&b.0));
            let slots = active_rems.len();
            let slot_idx = ahead % slots;
            let extra_waves = ahead / slots;
            let (base50, base75) = active_rems[slot_idx];
            if extra_waves == 0 {
                return (base50, base75);
            }
            let wave_dist = model.resolve_dist(repo, "write", "", |b| &b.run_to_delivered);
            let w50 = wave_dist.dist.p50_sec.unwrap_or(54.0 * 60.0);
            let w75 = wave_dist.dist.p75_sec.unwrap_or(w50).max(w50);
            let mut segs = vec![(base50, base75)];
            for _ in 0..extra_waves {
                segs.push((w50, w75));
            }
            return combine_p50_p75(&segs);
        }
    }

    // Fallback to historical capacity queue distribution by reason -> global -> prior.
    for (k, dist) in &model.global.capacity_queue_by_reason {
        if !k.is_empty() && reason.contains(k) && dist.count >= speed::MIN_GLOBAL_MODE_N {
            let p50 = dist.p50_sec.unwrap_or(300.0);
            let p75 = dist.p75_sec.unwrap_or(p50).max(p50);
            return (p50, p75);
        }
    }
    if model.global.capacity_queue.count >= speed::MIN_GLOBAL_MODE_N {
        let p50 = model.global.capacity_queue.p50_sec.unwrap_or(300.0);
        let p75 = model.global.capacity_queue.p75_sec.unwrap_or(p50).max(p50);
        return (p50, p75);
    }
    let p50 = model.priors.write.capacity_queue.p50_sec.unwrap_or(300.0);
    let p75 = model
        .priors
        .write
        .capacity_queue
        .p75_sec
        .unwrap_or(720.0)
        .max(p50);
    (p50, p75)
}

// ---------------------------------------------------------------------------
// Work-item DAG topological critical path & T_dev estimation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ParsedWaitOn {
    work_deps: Vec<(String, bool)>, // (dep_id, needs_applied)
    run_deps: Vec<(String, bool)>,  // (run_ref, needs_applied)
    owner_refs: Vec<String>,
    after_time_max: Option<i64>,
    defer_until_ts: Option<i64>,
}

fn parse_work_wait_on(item: &WorkItem, all_by_id: &HashMap<String, &WorkItem>, now_ts: i64) -> ParsedWaitOn {
    let mut work_deps = Vec::new();
    let mut run_deps = Vec::new();
    let mut owner_refs = Vec::new();
    let mut after_time_max: Option<i64> = None;
    let mut defer_until_ts: Option<i64> = None;

    for cond in &item.wait_on {
        if let Some(wid_raw) = cond.strip_prefix("work_applied:") {
            let wid = work::normalize_work_id(wid_raw).unwrap_or_else(|| wid_raw.to_string());
            let satisfied = all_by_id
                .get(&wid)
                .is_some_and(|w| w.applied || (w.state == "done" && w.closed_kind != "dropped"));
            if !satisfied {
                work_deps.push((wid, true));
            }
        } else if let Some(wid_raw) = cond.strip_prefix("work_closed:") {
            let wid = work::normalize_work_id(wid_raw).unwrap_or_else(|| wid_raw.to_string());
            let satisfied = all_by_id
                .get(&wid)
                .is_some_and(|w| w.state == "done" && w.closed_kind != "dropped");
            if !satisfied {
                work_deps.push((wid, true));
            }
        } else if let Some(run_ref_raw) = cond.strip_prefix("run_applied:") {
            let run_ref = run_ref_raw.trim();
            if !run_ref.is_empty() {
                let satisfied = all_by_id
                    .values()
                    .any(|w| w.applied && w.matches_run(run_ref));
                if !satisfied {
                    run_deps.push((run_ref.to_string(), true));
                }
            }
        } else if let Some(run_ref_raw) = cond.strip_prefix("run_delivered:") {
            let run_ref = run_ref_raw.trim();
            if !run_ref.is_empty() {
                run_deps.push((run_ref.to_string(), false));
            }
        } else if let Some(oref_raw) = cond.strip_prefix("owner:") {
            let oref = oref_raw.trim();
            let norm = work::normalize_work_id(oref).unwrap_or_else(|| oref.to_string());
            let satisfied = all_by_id
                .get(&norm)
                .is_some_and(|w| w.state == "done" && w.closed_kind != "dropped");
            if !satisfied {
                owner_refs.push(norm);
            }
        } else if let Some(ts_str) = cond.strip_prefix("after_time:") {
            if let Some(ts) = parse_iso_or_date_epoch(ts_str.trim()) {
                if ts > now_ts {
                    after_time_max = Some(after_time_max.map_or(ts, |prev| prev.max(ts)));
                }
            }
        }
    }

    // Respect active defer_until as an external wait EST when in the future.
    if !item.defer_until.is_empty() {
        if let Some(ts) = parse_iso_or_date_epoch(&item.defer_until) {
            if ts > now_ts {
                defer_until_ts = Some(ts);
                after_time_max = Some(after_time_max.map_or(ts, |prev| prev.max(ts)));
            }
        }
    }

    ParsedWaitOn {
        work_deps,
        run_deps,
        owner_refs,
        after_time_max,
        defer_until_ts,
    }
}

fn format_upstream_run_wait_badge(
    up_re: &RunEta,
    needs_applied: bool,
    now_ts: i64,
) -> (String, String) {
    let raw_name = if up_re.name.is_empty() {
        up_re.run.as_str()
    } else {
        up_re.name.as_str()
    };
    let disp_name = notes::strip_reply_prefixes(raw_name).0;
    let disp_name = if disp_name.is_empty() { raw_name } else { disp_name };

    if let Some(b) = &up_re.blocker {
        let badge = format!("等 {disp_name}（{}）", b.kind.compact_badge());
        return (badge.clone(), badge);
    }

    let fmt_rng = |r: &EtaRange| -> String {
        if r.p50_sec < 300.0 {
            "<5m".to_string()
        } else {
            format!("~{}", format_clock(r.rounded_p50_at, now_ts))
        }
    };

    let del_clk = up_re.to_delivered.as_ref().map(fmt_rng);
    let app_clk = up_re.to_applied.as_ref().map(fmt_rng);

    let text = if needs_applied {
        match (del_clk, app_clk) {
            (Some(d), Some(a)) if d != a => format!("等 {disp_name}（交付 {d}，合入 {a}）"),
            (_, Some(a)) => format!("等 {disp_name}（合入 {a}）"),
            (Some(d), None) => format!("等 {disp_name}（交付 {d}）"),
            (None, None) => format!("等 {disp_name} 合入"),
        }
    } else {
        match del_clk {
            Some(d) => format!("等 {disp_name}（交付 {d}）"),
            None => format!("等 {disp_name} 交付"),
        }
    };
    (text.clone(), text)
}

fn work_class_to_eta_class(work_class: &str) -> &'static str {
    match work_class {
        "hard" => "hard",
        "design" => "design",
        "feat" | "unblock" | "infra" | "build" => "build",
        _ => "other",
    }
}

/// Estimate open work items as a DAG (design.md §1, §2.2, §3.2, §3.3, §4).
/// - Active items use live remaining `to_applied`.
/// - `after_time:` shifts clock EST without inflating `dev_wall`.
/// - `owner:Qxx` (and items transitively waiting on `owner:Qxx`) are stripped into `owner_blocked`
///   with `after_unlock` range, never counted in `dev_wall`.
/// - Recent 2h `lane_queue_share > 15%` adds lane wait median to unstarted write nodes.
pub fn estimate_work_dag_in(
    all_items: &[WorkItem],
    repo_filter: Option<&str>,
    model: &EtaModel,
    ctx: &LiveContext,
) -> DagEtaSummary {
    let now_ts = ctx.now_ts;
    let all_by_id: HashMap<String, &WorkItem> = all_items.iter().map(|w| (w.id.clone(), w)).collect();

    // Select open non-owner work items matching repo_filter.
    let open_items: Vec<&WorkItem> = all_items
        .iter()
        .filter(|w| {
            w.owner.is_none()
                && w.state != "done"
                && repo_filter.is_none_or(|rf| {
                    let rname = speed::short_repo_name(&w.repo);
                    speed::repo_matches_filter(&w.repo, &rname, rf)
                })
        })
        .collect();

    let lane_congested = model.global.recent_lane_2h.queue_share > fill::STOP_LANE_QUEUE_SHARE;
    let lane_extra_sec = if lane_congested {
        model
            .global
            .recent_lane_2h
            .wait_nonzero_p50_sec
            .or(model.global.recent_lane_2h.wait_p50_sec)
            .unwrap_or(0.0)
    } else {
        0.0
    };

    // Per-item parsed wait_on + live run ETA + base node duration.
    let mut parsed_map: HashMap<String, ParsedWaitOn> = HashMap::new();
    let mut node_run_eta: HashMap<String, RunEta> = HashMap::new();
    let mut node_up_run: HashMap<String, (String, bool, RunEta)> = HashMap::new();
    let mut up_run_cache: HashMap<PathBuf, RunEta> = HashMap::new();
    let mut node_dev_del: HashMap<String, (f64, f64, bool)> = HashMap::new(); // to_delivered (p50, p75, is_prior)
    let mut node_dev_app: HashMap<String, (f64, f64, bool)> = HashMap::new(); // to_applied (p50, p75, is_prior)

    let mut active_write_count = 0usize;
    let mut primary_repo = String::new();
    let mut any_prior = false;

    for w in &open_items {
        if primary_repo.is_empty() && !w.repo.is_empty() {
            primary_repo = w.repo.clone();
        }
        let pw = parse_work_wait_on(w, &all_by_id, now_ts);
        // Resolve first unsatisfied upstream live run (`run_applied:` / `run_delivered:`) once per run dir.
        for (run_ref, needs_applied) in &pw.run_deps {
            if let Some(rdir) = ctx.resolve_live_run(run_ref, &w.run_dirs) {
                let up_re = up_run_cache
                    .entry(rdir.clone())
                    .or_insert_with(|| estimate_run_in(&rdir, model, ctx))
                    .clone();
                let done = if *needs_applied {
                    up_re.stage == Stage::Done
                } else {
                    up_re.stage == Stage::Done
                        || up_re.to_delivered.as_ref().is_some_and(|r| r.p50_sec == 0.0)
                };
                if !done {
                    node_up_run.insert(w.id.clone(), (run_ref.clone(), *needs_applied, up_re));
                    break;
                }
            }
        }
        parsed_map.insert(w.id.clone(), pw);

        // Check latest live run attached to this work item (skip for deferred items so stale runs
        // from earlier attempts never pollute live DAG totals or blockers).
        let mut live_eta: Option<RunEta> = None;
        if w.state != "deferred" {
            for rid in w.runs.iter().rev() {
                if let Some(rdir) = ctx.resolve_live_run(rid, &w.run_dirs) {
                    let re = estimate_run_in(&rdir, model, ctx);
                    // Ignore read-only runs and already-stopped/cleaned runs for write-chain estimation.
                    if re.mode == "write" && re.stage != Stage::Done {
                        live_eta = Some(re);
                        break;
                    }
                }
            }
        }

        let eta_cls = work_class_to_eta_class(&w.class);
        if w.state == "verify" || w.applied {
            node_dev_del.insert(w.id.clone(), (0.0, 0.0, false));
            node_dev_app.insert(w.id.clone(), (0.0, 0.0, false));
        } else if let Some(re) = live_eta {
            if runs::active(&re.state) {
                active_write_count += 1;
            }
            if re.is_prior {
                any_prior = true;
            }
            let (d50, d75) = re
                .to_delivered
                .as_ref()
                .map(|r| (r.p50_sec, r.p75_sec))
                .unwrap_or((0.0, 0.0));
            let (a50, a75) = re
                .to_applied
                .as_ref()
                .map(|r| (r.p50_sec, r.p75_sec))
                .unwrap_or_else(|| {
                    if re.blocker.is_some() {
                        // Stopped-clock item: estimate post-unlock work using bucket baseline if not delivered.
                        if re.to_delivered.as_ref().is_some_and(|r| r.p50_sec == 0.0) {
                            let mw = model.resolve_dist(&w.repo, "write", eta_cls, |b| &b.merge_wait);
                            let m50 = mw.dist.p50_sec.unwrap_or(540.0);
                            let m75 = mw.dist.p75_sec.unwrap_or(m50).max(m50);
                            (m50, m75)
                        } else {
                            let rw = model.resolve_dist(&w.repo, "write", eta_cls, |b| &b.rework_round);
                            let r50 = rw.dist.p50_sec.unwrap_or(15.0 * 60.0);
                            let r75 = rw.dist.p75_sec.unwrap_or(25.0 * 60.0).max(r50);
                            (r50, r75)
                        }
                    } else {
                        (d50, d75)
                    }
                });
            node_dev_del.insert(w.id.clone(), (d50, d75, re.is_prior));
            node_dev_app.insert(w.id.clone(), (a50, a75, re.is_prior));
            node_run_eta.insert(w.id.clone(), re);
        } else {
            // Unstarted write node: lookup end-to-end empirical quantiles + optional lane congestion surcharge.
            let del_d = model.resolve_dist(&w.repo, "write", eta_cls, |b| &b.chain_to_delivered);
            let app_d = model.resolve_dist(&w.repo, "write", eta_cls, |b| &b.chain_to_applied);
            let is_p = del_d.level.is_prior() || app_d.level.is_prior();
            if is_p && w.state != "deferred" {
                any_prior = true;
            }
            let d50 = del_d.dist.p50_sec.unwrap_or(54.0 * 60.0) + lane_extra_sec;
            let d75 = del_d.dist.p75_sec.unwrap_or(78.0 * 60.0).max(d50 - lane_extra_sec) + lane_extra_sec;
            let a50 = app_d.dist.p50_sec.unwrap_or(63.0 * 60.0) + lane_extra_sec;
            let a75 = app_d.dist.p75_sec.unwrap_or(87.0 * 60.0).max(a50 - lane_extra_sec) + lane_extra_sec;
            node_dev_del.insert(w.id.clone(), (d50, d75, is_p));
            node_dev_app.insert(w.id.clone(), (a50, a75, is_p));
        }
    }

    // Topological sort (Kahn's algorithm) over open_items.
    let open_ids: HashSet<String> = open_items.iter().map(|w| w.id.clone()).collect();
    let mut in_deg: HashMap<String, usize> = HashMap::new();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for w in &open_items {
        in_deg.entry(w.id.clone()).or_insert(0);
        if let Some(pw) = parsed_map.get(&w.id) {
            for (dep, _) in &pw.work_deps {
                if open_ids.contains(dep) && dep != &w.id {
                    children.entry(dep.clone()).or_default().push(w.id.clone());
                    *in_deg.entry(w.id.clone()).or_insert(0) += 1;
                }
            }
        }
    }
    let mut queue_topo: VecDeque<String> = in_deg
        .iter()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut topo_order: Vec<String> = Vec::with_capacity(open_items.len());
    while let Some(u) = queue_topo.pop_front() {
        topo_order.push(u.clone());
        if let Some(ch_list) = children.get(&u) {
            for v in ch_list {
                if let Some(d) = in_deg.get_mut(v) {
                    *d = d.saturating_sub(1);
                    if *d == 0 {
                        queue_topo.push_back(v.clone());
                    }
                }
            }
        }
    }
    // Append any remaining nodes (in case of unexpected cycle) deterministically.
    for w in &open_items {
        if !topo_order.contains(&w.id) {
            topo_order.push(w.id.clone());
        }
    }

    // Propagate owner blockers transitively so owner-blocked subgraphs are separated from T_dev.
    let mut owner_blocker_ref: HashMap<String, String> = HashMap::new();
    for id in &topo_order {
        if let Some(pw) = parsed_map.get(id) {
            if let Some(oref) = pw.owner_refs.first() {
                owner_blocker_ref.insert(id.clone(), oref.clone());
                continue;
            }
            for (dep, _) in &pw.work_deps {
                if let Some(oref) = owner_blocker_ref.get(dep) {
                    owner_blocker_ref.insert(id.clone(), oref.clone());
                    break;
                }
            }
        }
    }

    // Propagate schedule holds (deferred items and `after_time:` date-waiting items) transitively
    // so non-advancing items never distort the active DAG total ETA.
    let mut schedule_hold: HashSet<String> = HashSet::new();
    for id in &topo_order {
        let is_def = all_by_id.get(id).is_some_and(|w| w.state == "deferred");
        let has_future_time = parsed_map
            .get(id)
            .and_then(|p| p.after_time_max)
            .is_some_and(|t| t > now_ts);
        let dep_held = parsed_map.get(id).is_some_and(|pw| {
            pw.work_deps
                .iter()
                .any(|(dep, _)| schedule_hold.contains(dep))
        });
        if is_def || has_future_time || dep_held {
            schedule_hold.insert(id.clone());
        }
    }

    // DP for dev critical path (excluding after_time) and clock finish (including after_time EST).
    let mut cp_depth: HashMap<String, usize> = HashMap::new();
    let mut cp_segs: HashMap<String, Vec<(f64, f64)>> = HashMap::new();
    let mut cp_p50: HashMap<String, f64> = HashMap::new();
    let mut cp_p75: HashMap<String, f64> = HashMap::new();
    let mut cp_path: HashMap<String, Vec<String>> = HashMap::new();
    let mut ef_p50: HashMap<String, i64> = HashMap::new();
    let mut ef_p75: HashMap<String, i64> = HashMap::new();

    for id in &topo_order {
        let (a50, a75, _) = node_dev_app.get(id).copied().unwrap_or((0.0, 0.0, false));
        let is_work_node = a50 > 0.0 || a75 > 0.0;
        let pw = parsed_map.get(id);

        let mut best_dep: Option<String> = None;
        let mut best_dep_p50 = 0.0_f64;
        let mut best_dep_depth = 0usize;
        let mut best_ef_p50 = now_ts;
        let mut best_ef_p75 = now_ts;
        let mut up_run_seg: Option<(f64, f64)> = None;

        if let Some(pw) = pw {
            for (dep, _) in &pw.work_deps {
                if !open_ids.contains(dep) || dep == id {
                    continue;
                }
                let dp50 = cp_p50.get(dep).copied().unwrap_or(0.0);
                let ddepth = cp_depth.get(dep).copied().unwrap_or(0);
                if dp50 > best_dep_p50 || (dp50 == best_dep_p50 && ddepth > best_dep_depth) {
                    best_dep_p50 = dp50;
                    best_dep_depth = ddepth;
                    best_dep = Some(dep.clone());
                }
                best_ef_p50 = best_ef_p50.max(ef_p50.get(dep).copied().unwrap_or(now_ts));
                best_ef_p75 = best_ef_p75.max(ef_p75.get(dep).copied().unwrap_or(now_ts));
            }
        }

        // Factor upstream live run (`run_applied:` / `run_delivered:`) into start time & critical path.
        if let Some((_, needs_applied, up_re)) = node_up_run.get(id) {
            let up_rng = if *needs_applied {
                up_re.to_applied.as_ref().or(up_re.to_delivered.as_ref())
            } else {
                up_re.to_delivered.as_ref()
            };
            if let Some(r) = up_rng {
                best_ef_p50 = best_ef_p50.max(now_ts + r.p50_sec.round() as i64);
                best_ef_p75 = best_ef_p75.max(now_ts + r.p75_sec.round() as i64);
                if r.p50_sec > best_dep_p50 {
                    up_run_seg = Some((r.p50_sec, r.p75_sec));
                }
            }
        }

        let mut segs = if let Some(ur) = up_run_seg {
            vec![ur]
        } else {
            best_dep
                .as_ref()
                .and_then(|d| cp_segs.get(d))
                .cloned()
                .unwrap_or_default()
        };
        let mut path = if up_run_seg.is_some() {
            Vec::new()
        } else {
            best_dep
                .as_ref()
                .and_then(|d| cp_path.get(d))
                .cloned()
                .unwrap_or_default()
        };
        if is_work_node {
            segs.push((a50, a75));
        }
        path.push(id.clone());
        let (tot50, tot75) = combine_p50_p75(&segs);
        let depth_here = if up_run_seg.is_some() {
            usize::from(is_work_node)
        } else {
            best_dep_depth + usize::from(is_work_node)
        };

        // External EST from after_time / defer_until: shifts per-item clock without entering dev_wall!
        let est = pw
            .and_then(|p| p.after_time_max)
            .unwrap_or(now_ts)
            .max(now_ts);
        let start_p50 = best_ef_p50.max(est);
        let start_p75 = best_ef_p75.max(est);
        let finish_p50 = start_p50 + a50.round() as i64;
        // Combine spread along path when anchored at now_ts, or from EST if delayed.
        let finish_p75 = if est > best_ef_p75 {
            est + a75.round() as i64
        } else {
            let spread = (tot75 - tot50).max(a75 - a50);
            (start_p50 + a50.round() as i64 + spread.round() as i64)
                .max(start_p75 + a50.round() as i64)
                .max(finish_p50)
        };

        cp_depth.insert(id.clone(), depth_here);
        cp_segs.insert(id.clone(), segs);
        cp_p50.insert(id.clone(), tot50);
        cp_p75.insert(id.clone(), tot75);
        cp_path.insert(id.clone(), path);
        ef_p50.insert(id.clone(), finish_p50);
        ef_p75.insert(id.clone(), finish_p75);
    }

    // Build per-item WorkItemEta and aggregate DAG totals (excluding owner_blocked and
    // deferred/date-wait items from active DAG ETA).
    let mut item_etas: Vec<WorkItemEta> = Vec::with_capacity(open_items.len());
    let mut owner_blocked: Vec<BlockerItem> = Vec::new();
    let mut blockers: Vec<BlockerItem> = Vec::new();
    let mut deferred_count = 0usize;
    let mut earliest_deferred_at: Option<i64> = None;
    let mut date_wait_count = 0usize;
    let mut earliest_date_wait_at: Option<i64> = None;

    let mut computable_write_chains = 0usize;
    let mut max_cp_depth = 0usize;
    let mut max_cp_p50 = 0.0_f64;
    let mut max_cp_p75 = 0.0_f64;
    let mut critical_path: Vec<String> = Vec::new();
    let mut sum_del_p50 = 0.0_f64;
    let mut sum_del_spread_sq = 0.0_f64;
    let mut max_clock_p50: Option<i64> = None;
    let mut max_clock_p75: Option<i64> = None;

    for w in &open_items {
        let pw = parsed_map.get(&w.id);
        let (a50, a75, is_p) = node_dev_app.get(&w.id).copied().unwrap_or((0.0, 0.0, false));
        let (d50, d75, _) = node_dev_del.get(&w.id).copied().unwrap_or((0.0, 0.0, false));
        let tot50 = cp_p50.get(&w.id).copied().unwrap_or(a50);
        let tot75 = cp_p75.get(&w.id).copied().unwrap_or(a75);
        let path = cp_path.get(&w.id).cloned().unwrap_or_else(|| vec![w.id.clone()]);
        let est = pw.and_then(|p| p.after_time_max);
        let run_eta = node_run_eta.get(&w.id);
        let is_deferred = w.state == "deferred";
        let is_held = schedule_hold.contains(&w.id);

        let mut item_blocker: Option<BlockerInfo> = None;

        // 1. Owner blocker? (Strict separation per design.md底线 2)
        if let Some(oref) = owner_blocker_ref.get(&w.id) {
            let range_str = format_duration_range(tot50, tot75);
            let b = BlockerInfo {
                kind: BlockerKind::Owner,
                who: "所有者".into(),
                reason: oref.clone(),
                since_sec: iso_epoch(&w.state_since).map(|t| (now_ts - t).max(0)),
                target: Some(oref.clone()),
                affected_count: None,
                unlock_at: None,
                after_unlock_p50_sec: Some(tot50),
                after_unlock_p75_sec: Some(tot75),
                label: format!("卡在所有者 [{oref}]·解锁后约 {range_str}"),
            };
            owner_blocked.push(BlockerItem {
                id: w.id.clone(),
                title: w.title.clone(),
                blocker: b.clone(),
            });
            item_blocker = Some(b);
        } else if is_held {
            // Deferred or date-waiting items are tracked separately and excluded from active DAG totals.
            if is_deferred {
                deferred_count += 1;
                if let Some(dt) = pw.and_then(|p| p.defer_until_ts).or(est) {
                    earliest_deferred_at = Some(earliest_deferred_at.map_or(dt, |prev| prev.min(dt)));
                }
            } else {
                date_wait_count += 1;
                if let Some(wt) = est {
                    earliest_date_wait_at = Some(earliest_date_wait_at.map_or(wt, |prev| prev.min(wt)));
                }
            }
            if let Some(est_ts) = est {
                let est_clock = format_local_short(est_ts, now_ts);
                let after_str = format_eta_duration(a50);
                item_blocker = Some(BlockerInfo {
                    kind: BlockerKind::ExternalTime,
                    who: if is_deferred { "暂缓定时" } else { "外部定时" }.into(),
                    reason: est_clock.clone(),
                    since_sec: None,
                    target: Some(est_clock.clone()),
                    affected_count: None,
                    unlock_at: Some(est_ts),
                    after_unlock_p50_sec: Some(a50),
                    after_unlock_p75_sec: Some(a75),
                    label: format!("外部等待至 {est_clock}·之后约 {after_str}"),
                });
            }
        } else {
            // Check path ① mainline block: depends on an open `class == "unblock"` work item!
            if let Some(pw) = pw {
                for (dep, _) in &pw.work_deps {
                    if let Some(dep_w) = all_by_id.get(dep) {
                        if dep_w.class == "unblock" && dep_w.state != "done" {
                            let unlock_at = ef_p50.get(dep).copied();
                            let unlock_str = unlock_at
                                .map(|u| format!("（预计 {} 解锁）", format_clock(round_eta_epoch(now_ts, u), now_ts)))
                                .unwrap_or_default();
                            let b = BlockerInfo {
                                kind: BlockerKind::MainlineBlocked,
                                who: "主干修复".into(),
                                reason: dep.clone(),
                                since_sec: iso_epoch(&w.state_since).map(|t| (now_ts - t).max(0)),
                                target: Some(dep.clone()),
                                affected_count: None,
                                unlock_at,
                                after_unlock_p50_sec: Some(a50),
                                after_unlock_p75_sec: Some(a75),
                                label: format!("卡在主干修复 [{dep}]{unlock_str}"),
                            };
                            blockers.push(BlockerItem {
                                id: w.id.clone(),
                                title: w.title.clone(),
                                blocker: b.clone(),
                            });
                            item_blocker = Some(b);
                            break;
                        }
                    }
                }
            }

            // Check live run blocker (stopped clock on controller / conflict / mainline / stall)
            if item_blocker.is_none() {
                if let Some(re) = run_eta {
                    if let Some(rb) = &re.blocker {
                        blockers.push(BlockerItem {
                            id: w.id.clone(),
                            title: w.title.clone(),
                            blocker: rb.clone(),
                        });
                        item_blocker = Some(rb.clone());
                    }
                }
            }

            // Accumulate into computable DAG (T_dev & clock finish) — advancing items only!
            if a50 > 0.0 || a75 > 0.0 {
                computable_write_chains += 1;
                sum_del_p50 += d50;
                let sp = (d75 - d50).max(0.0);
                sum_del_spread_sq += sp * sp;

                if tot50 > max_cp_p50
                    || (tot50 == max_cp_p50 && cp_depth.get(&w.id).copied().unwrap_or(0) > max_cp_depth)
                {
                    max_cp_p50 = tot50;
                    max_cp_p75 = tot75;
                    max_cp_depth = cp_depth.get(&w.id).copied().unwrap_or(0);
                    critical_path = path.clone();
                }

                let f50 = ef_p50.get(&w.id).copied().unwrap_or(now_ts + a50.round() as i64);
                let f75 = ef_p75.get(&w.id).copied().unwrap_or(now_ts + a75.round() as i64);
                max_clock_p50 = Some(max_clock_p50.map_or(f50, |prev| prev.max(f50)));
                max_clock_p75 = Some(max_clock_p75.map_or(f75, |prev| prev.max(f75)));
            }
        }

        let remaining = if owner_blocker_ref.contains_key(&w.id) {
            None
        } else {
            let f50 = ef_p50.get(&w.id).copied().unwrap_or(now_ts);
            let f75 = ef_p75.get(&w.id).copied().unwrap_or(f50).max(f50);
            Some(EtaRange::from_remaining(
                now_ts,
                (f50 - now_ts).max(0) as f64,
                (f75 - now_ts).max(0) as f64,
                run_eta.and_then(|r| r.to_delivered.as_ref()).is_some_and(|d| d.anomaly),
            ))
        };

        let verify_note = if w.needs_verify { " + 待人工验证" } else { "" };
        let (short_text, focus_badge) = if let Some(b) = &item_blocker {
            let badge = match b.kind {
                BlockerKind::Owner => format!("卡所有者{}", b.target.as_deref().unwrap_or("")),
                BlockerKind::MainlineBlocked => {
                    if let Some(t) = &b.target {
                        if let Some(u) = b.unlock_at {
                            format!("等{t}(~{})", format_clock(round_eta_epoch(now_ts, u), now_ts))
                        } else {
                            format!("卡主干({t})")
                        }
                    } else {
                        "卡主干".into()
                    }
                }
                BlockerKind::ExternalTime => format!("等{}", b.target.as_deref().unwrap_or("定时")),
                _ => b.kind.compact_badge().to_string(),
            };
            (format!("{}{verify_note}", b.label), badge)
        } else if let Some(rem) = &remaining {
            if w.state == "verify" {
                (format!("待验证{verify_note}"), "待验".into())
            } else if let Some((_, needs_applied, up_re)) = node_up_run.get(&w.id) {
                let (up_short, up_badge) =
                    format_upstream_run_wait_badge(up_re, *needs_applied, now_ts);
                let c50 = format_clock(rem.rounded_p50_at, now_ts);
                let c75 = format_clock(rem.rounded_p75_at, now_ts);
                (
                    format!("{up_short}·本项预计 {c50} 合入（p75 {c75}）{verify_note}"),
                    up_badge,
                )
            } else if let Some((dep, _)) = pw.and_then(|p| p.work_deps.first()) {
                let c50 = format_clock(rem.rounded_p50_at, now_ts);
                let c75 = format_clock(rem.rounded_p75_at, now_ts);
                (
                    format!("等 {dep}·预计 {c50} 合入（p75 {c75}）{verify_note}"),
                    format!("等{dep}(~{c50})"),
                )
            } else if w.state == "ready" && run_eta.is_none() {
                let dur = format_duration_range(a50, a75);
                let c50 = format_clock(rem.rounded_p50_at, now_ts);
                let c75 = format_clock(rem.rounded_p75_at, now_ts);
                (
                    format!("预计耗时 {dur}（现派约 {c50} 合入，p75 {c75}）{verify_note}"),
                    format!("预计耗时 {dur}"),
                )
            } else {
                let c50 = format_clock(rem.rounded_p50_at, now_ts);
                let c75 = format_clock(rem.rounded_p75_at, now_ts);
                let badge = if rem.p50_sec < 300.0 {
                    "<5m".into()
                } else {
                    format!("~{c50}")
                };
                (
                    format!("预计 {c50} 合入（p75 {c75}）{verify_note}"),
                    badge,
                )
            }
        } else {
            ("-".into(), "-".into())
        };

        let compact_badge = if let Some(b) = &item_blocker {
            b.kind.compact_badge().to_string()
        } else if let Some((_, needs_applied, up_re)) = node_up_run.get(&w.id) {
            let up_rng = if *needs_applied {
                up_re.to_applied.as_ref().or(up_re.to_delivered.as_ref())
            } else {
                up_re.to_delivered.as_ref()
            };
            if let Some(b) = &up_re.blocker {
                b.kind.compact_badge().to_string()
            } else if let Some(r) = up_rng {
                format_compact_clock(r.rounded_p50_at, now_ts, r.p50_sec)
            } else {
                "-".into()
            }
        } else if let Some(rem) = &remaining {
            if w.state == "verify" {
                "待验".into()
            } else if w.state == "ready" && run_eta.is_none() {
                format!("~{}", format_eta_duration(a50))
            } else {
                format_compact_clock(rem.rounded_p50_at, now_ts, rem.p50_sec)
            }
        } else {
            "-".into()
        };

        item_etas.push(WorkItemEta {
            id: w.id.clone(),
            title: w.title.clone(),
            repo: w.repo.clone(),
            state: w.state.clone(),
            class: w.class.clone(),
            active_run: run_eta.map(|r| r.run.clone()),
            needs_verify: w.needs_verify,
            external_est_at: est,
            blocker: item_blocker,
            remaining,
            dev_p50_sec: a50,
            dev_p75_sec: a75,
            upstream_path: path,
            short_text,
            focus_badge,
            compact_badge,
            is_prior: is_p,
            now_ts,
        });
    }

    let external_wait_count = deferred_count + date_wait_count;

    // Compute throughput bound with effective concurrency correction (design.md §3.2).
    let formula_res = model.resolve_formula(&primary_repo);
    if formula_res.level.is_prior() && computable_write_chains > 0 {
        any_prior = true;
    }
    let fill_target = ctx.fill_target_for_repo(&primary_repo);
    let p_eff_hist = formula_res.params.p_eff_mean.unwrap_or(2.0);
    let p_eff_now = effective_concurrency(active_write_count, formula_res.params, fill_target);

    let t_merge_50 = formula_res.params.t_merge_p50_sec.unwrap_or(540.0);
    let t_merge_75 = formula_res.params.t_merge_p75_sec.unwrap_or(t_merge_50).max(t_merge_50);

    let (dev_wall_p50, dev_wall_p75, finish_p50_at, finish_p75_at) = if computable_write_chains == 0 {
        (0.0, 0.0, None, None)
    } else {
        let tp_50 = sum_del_p50 / p_eff_now + t_merge_50;
        let tp_spread = (sum_del_spread_sq.sqrt() / p_eff_now).hypot((t_merge_75 - t_merge_50).max(0.0));
        let tp_75 = tp_50 + tp_spread;

        let wall_p50 = max_cp_p50.max(tp_50);
        let wall_p75 = max_cp_p75.max(tp_75).max(wall_p50);

        let f50 = max_clock_p50
            .unwrap_or(now_ts)
            .max(now_ts + tp_50.round() as i64);
        let f75 = max_clock_p75
            .unwrap_or(f50)
            .max(now_ts + tp_75.round() as i64)
            .max(f50);
        (wall_p50, wall_p75, Some(f50), Some(f75))
    };

    let headline = format_work_dag_headline(
        computable_write_chains,
        max_cp_depth,
        dev_wall_p50,
        dev_wall_p75,
        finish_p50_at,
        finish_p75_at,
        deferred_count,
        earliest_deferred_at,
        date_wait_count,
        earliest_date_wait_at,
        owner_blocked.len(),
        any_prior,
        now_ts,
    );

    DagEtaSummary {
        scope: "work".into(),
        write_chains: computable_write_chains,
        depth: max_cp_depth,
        active_count: active_write_count,
        p_eff_now,
        p_eff_live: active_write_count,
        p_eff_hist,
        fill_target,
        lane_congested,
        lane_extra_sec,
        dev_wall_p50_sec: speed::round2(dev_wall_p50),
        dev_wall_p75_sec: speed::round2(dev_wall_p75),
        earliest_finish_p50_at: finish_p50_at,
        earliest_finish_p75_at: finish_p75_at,
        critical_path,
        external_wait_count,
        deferred_count,
        earliest_deferred_at,
        date_wait_count,
        earliest_date_wait_at,
        owner_blocked,
        blockers,
        is_prior: any_prior,
        headline,
        items: item_etas,
        now_ts,
    }
}

#[allow(clippy::too_many_arguments)]
fn format_work_dag_headline(
    write_chains: usize,
    depth: usize,
    dev_wall_p50: f64,
    dev_wall_p75: f64,
    finish_p50_at: Option<i64>,
    finish_p75_at: Option<i64>,
    deferred_count: usize,
    earliest_deferred_at: Option<i64>,
    date_wait_count: usize,
    earliest_date_wait_at: Option<i64>,
    owner_blocked_count: usize,
    is_prior: bool,
    now_ts: i64,
) -> String {
    let mut deferred_seg = String::new();
    if deferred_count > 0 {
        if let Some(dt) = earliest_deferred_at {
            deferred_seg = format!(
                "另有 {deferred_count} 项暂缓（最早 {} 恢复）",
                format_local_short(dt, now_ts)
            );
        } else {
            deferred_seg = format!("另有 {deferred_count} 项暂缓");
        }
    }
    let mut date_wait_seg = String::new();
    if date_wait_count > 0 {
        if let Some(wt) = earliest_date_wait_at {
            date_wait_seg = format!(
                "外部等待 {date_wait_count}（最早 {}）",
                format_local_short(wt, now_ts)
            );
        } else {
            date_wait_seg = format!("外部等待 {date_wait_count}");
        }
    }

    if write_chains == 0 {
        let mut extras = Vec::new();
        if owner_blocked_count > 0 {
            extras.push(format!("卡所有者 {owner_blocked_count}"));
        }
        if !date_wait_seg.is_empty() {
            extras.push(date_wait_seg);
        }
        if !deferred_seg.is_empty() {
            extras.push(deferred_seg);
        }
        if extras.is_empty() {
            return "无待推进写链".into();
        }
        return extras.join(" · ");
    }
    let range = format_duration_range(dev_wall_p50, dev_wall_p75);
    let clock_part = match (finish_p50_at, finish_p75_at) {
        (Some(a), Some(b)) => format!(
            "，p50 {} / p75 {}",
            format_clock(round_eta_epoch(now_ts, a), now_ts),
            format_clock(round_eta_epoch(now_ts, b), now_ts),
        ),
        _ => String::new(),
    };
    let prior_tag = if is_prior { "(初估)" } else { "" };
    let mut line = format!("推进 {range}（{write_chains}写链/深{depth}{clock_part}）{prior_tag}");
    if !date_wait_seg.is_empty() {
        line.push_str(&format!(" + {date_wait_seg}"));
    }
    if !deferred_seg.is_empty() {
        line.push_str(&format!(" · {deferred_seg}"));
    }
    if owner_blocked_count > 0 {
        line.push_str(&format!(" + 卡所有者 {owner_blocked_count}"));
    }
    line
}

/// Estimate a single work item (including its recursive upstream dependency chain).
pub fn estimate_work_item_in(
    target_id: &str,
    all_items: &[WorkItem],
    model: &EtaModel,
    ctx: &LiveContext,
) -> Option<WorkItemEta> {
    let norm = work::normalize_work_id(target_id).unwrap_or_else(|| target_id.to_string());
    let dag = estimate_work_dag_in(all_items, None, model, ctx);
    dag.items.into_iter().find(|it| it.id == norm)
}

// ---------------------------------------------------------------------------
// `dag-queue.json` (`delegate queue`) topological critical path estimation
// ---------------------------------------------------------------------------

pub fn estimate_queue_dag_in(
    queue_items: &[Value],
    repo_filter: Option<&str>,
    model: &EtaModel,
    ctx: &LiveContext,
) -> DagEtaSummary {
    let now_ts = ctx.now_ts;
    let lane_congested = model.global.recent_lane_2h.queue_share > fill::STOP_LANE_QUEUE_SHARE;
    let lane_extra_sec = if lane_congested {
        model
            .global
            .recent_lane_2h
            .wait_nonzero_p50_sec
            .or(model.global.recent_lane_2h.wait_p50_sec)
            .unwrap_or(0.0)
    } else {
        0.0
    };

    let active_q: Vec<&Value> = queue_items
        .iter()
        .filter(|it| {
            let st = s(it, "status");
            st != "cancelled"
                && repo_filter.is_none_or(|rf| {
                    let rp = s(it, "repo");
                    let rn = speed::short_repo_name(rp);
                    speed::repo_matches_filter(rp, &rn, rf)
                })
        })
        .collect();

    let by_name: HashMap<String, &Value> = active_q
        .iter()
        .map(|it| (s(it, "name").to_string(), *it))
        .collect();

    let mut primary_repo = String::new();
    let mut active_count = 0usize;
    let mut any_prior = false;

    // Evaluate each queue node's remaining time (to_delivered & to_applied).
    let mut node_del: HashMap<String, (f64, f64)> = HashMap::new();
    let mut node_app: HashMap<String, (f64, f64)> = HashMap::new();
    let mut node_blocker: HashMap<String, BlockerInfo> = HashMap::new();
    let mut node_run: HashMap<String, String> = HashMap::new();
    let mut node_done: HashSet<String> = HashSet::new();

    for it in &active_q {
        let name = s(it, "name").to_string();
        let repo = s(it, "repo").to_string();
        if primary_repo.is_empty() && !repo.is_empty() {
            primary_repo = repo.clone();
        }
        let st = s(it, "status");
        if st == "paused" {
            let reason = {
                let pr = s(it, "pauseReason");
                if pr.is_empty() { "上游异常暂停".into() } else { pr.to_string() }
            };
            let up = s(it, "upstreamRun");
            node_blocker.insert(
                name.clone(),
                BlockerInfo {
                    kind: BlockerKind::QueuePaused,
                    who: "队列暂停".into(),
                    reason: reason.clone(),
                    since_sec: iso_epoch(s(it, "pausedAt")).map(|t| (now_ts - t).max(0)),
                    target: (!up.is_empty()).then(|| up.to_string()),
                    affected_count: None,
                    unlock_at: None,
                    after_unlock_p50_sec: None,
                    after_unlock_p75_sec: None,
                    label: format!("卡在队列暂停·{reason}"),
                },
            );
            let app_d = model.resolve_dist(&repo, "write", "", |b| &b.chain_to_applied);
            let a50 = app_d.dist.p50_sec.unwrap_or(63.0 * 60.0);
            let a75 = app_d.dist.p75_sec.unwrap_or(87.0 * 60.0).max(a50);
            node_del.insert(name.clone(), (a50, a75));
            node_app.insert(name.clone(), (a50, a75));
            continue;
        }
        if st == "dispatched" {
            let rdir_str = s(it, "runDir");
            let rid = s(it, "runId");
            let rdir_opt = if !rdir_str.is_empty() && Path::new(rdir_str).join("meta.json").is_file() {
                Some(PathBuf::from(rdir_str))
            } else if !rid.is_empty() {
                ctx.resolve_live_run(rid, &BTreeMap::new())
            } else {
                None
            };
            if let Some(rdir) = rdir_opt {
                let re = estimate_run_in(&rdir, model, ctx);
                node_run.insert(name.clone(), re.run.clone());
                if re.stage == Stage::Done {
                    node_done.insert(name.clone());
                    node_del.insert(name.clone(), (0.0, 0.0));
                    node_app.insert(name.clone(), (0.0, 0.0));
                } else {
                    if runs::active(&re.state) {
                        active_count += 1;
                    }
                    if re.is_prior {
                        any_prior = true;
                    }
                    let (d50, d75) = re
                        .to_delivered
                        .as_ref()
                        .map(|r| (r.p50_sec, r.p75_sec))
                        .unwrap_or((0.0, 0.0));
                    let (a50, a75) = re
                        .to_applied
                        .as_ref()
                        .map(|r| (r.p50_sec, r.p75_sec))
                        .unwrap_or((d50, d75));
                    if let Some(b) = re.blocker {
                        node_blocker.insert(name.clone(), b);
                    }
                    node_del.insert(name.clone(), (d50, d75));
                    node_app.insert(name.clone(), (a50, a75));
                }
                continue;
            }
        }
        // pending / dispatching: unstarted write node
        let del_d = model.resolve_dist(&repo, "write", "", |b| &b.chain_to_delivered);
        let app_d = model.resolve_dist(&repo, "write", "", |b| &b.chain_to_applied);
        if del_d.level.is_prior() || app_d.level.is_prior() {
            any_prior = true;
        }
        let d50 = del_d.dist.p50_sec.unwrap_or(54.0 * 60.0) + lane_extra_sec;
        let d75 = del_d.dist.p75_sec.unwrap_or(78.0 * 60.0).max(d50 - lane_extra_sec) + lane_extra_sec;
        let a50 = app_d.dist.p50_sec.unwrap_or(63.0 * 60.0) + lane_extra_sec;
        let a75 = app_d.dist.p75_sec.unwrap_or(87.0 * 60.0).max(a50 - lane_extra_sec) + lane_extra_sec;
        node_del.insert(name.clone(), (d50, d75));
        node_app.insert(name.clone(), (a50, a75));
    }

    // Topological order among queue items.
    let mut in_deg: HashMap<String, usize> = HashMap::new();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for it in &active_q {
        let nm = s(it, "name").to_string();
        in_deg.entry(nm.clone()).or_insert(0);
        for dep in strings(&it["after"]).into_iter().chain(strings(&it["afterApplied"])) {
            if by_name.contains_key(&dep) && dep != nm {
                children.entry(dep).or_default().push(nm.clone());
                *in_deg.entry(nm.clone()).or_insert(0) += 1;
            }
        }
    }
    let mut q_topo: VecDeque<String> = in_deg
        .iter()
        .filter(|&(_, &d)| d == 0)
        .map(|(k, _)| k.clone())
        .collect();
    let mut topo = Vec::with_capacity(active_q.len());
    while let Some(u) = q_topo.pop_front() {
        topo.push(u.clone());
        if let Some(ch) = children.get(&u) {
            for v in ch {
                if let Some(d) = in_deg.get_mut(v) {
                    *d = d.saturating_sub(1);
                    if *d == 0 {
                        q_topo.push_back(v.clone());
                    }
                }
            }
        }
    }
    for it in &active_q {
        let nm = s(it, "name").to_string();
        if !topo.contains(&nm) {
            topo.push(nm);
        }
    }

    // Helper: estimate external upstream run referenced in after/afterApplied (not in dag-queue).
    let ext_dep_rem = |it: &Value, dep: &str, need_applied: bool| -> (f64, f64, bool) {
        let dir_opt = it["afterDirs"]
            .get(dep)
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.join("meta.json").is_file())
            .or_else(|| ctx.resolve_live_run(dep, &BTreeMap::new()));
        let Some(rdir) = dir_opt else {
            return (0.0, 0.0, false);
        };
        let re = estimate_run_in(&rdir, model, ctx);
        if re.stage == Stage::Done {
            return (0.0, 0.0, false);
        }
        let rng = if need_applied {
            re.to_applied.as_ref().or(re.to_delivered.as_ref())
        } else {
            re.to_delivered.as_ref()
        };
        match rng {
            Some(r) if r.p50_sec > 0.0 || r.p75_sec > 0.0 => (r.p50_sec, r.p75_sec, true),
            _ => (0.0, 0.0, false),
        }
    };

    let mut cp_depth: HashMap<String, usize> = HashMap::new();
    let mut cp_segs_del: HashMap<String, Vec<(f64, f64)>> = HashMap::new();
    let mut cp_segs_app: HashMap<String, Vec<(f64, f64)>> = HashMap::new();
    let mut cp_p50_app: HashMap<String, f64> = HashMap::new();
    let mut cp_p75_app: HashMap<String, f64> = HashMap::new();
    let mut cp_path: HashMap<String, Vec<String>> = HashMap::new();

    for nm in &topo {
        let it = by_name[nm];
        let done = node_done.contains(nm);
        let (d50, d75) = node_del.get(nm).copied().unwrap_or((0.0, 0.0));
        let (a50, a75) = node_app.get(nm).copied().unwrap_or((0.0, 0.0));

        let mut best_segs: Vec<(f64, f64)> = Vec::new();
        let mut best_path: Vec<String> = Vec::new();
        let mut best_depth = 0usize;
        let mut best_p50 = 0.0_f64;

        let check_dep = |dep: &str, need_applied: bool, best_p50: &mut f64, best_depth: &mut usize, best_segs: &mut Vec<(f64, f64)>, best_path: &mut Vec<String>| {
            if by_name.contains_key(dep) && dep != nm {
                let segs = if need_applied {
                    cp_segs_app.get(dep).cloned().unwrap_or_default()
                } else {
                    cp_segs_del.get(dep).cloned().unwrap_or_default()
                };
                let (p50, _) = combine_p50_p75(&segs);
                let dd = cp_depth.get(dep).copied().unwrap_or(0);
                if p50 > *best_p50 || (p50 == *best_p50 && dd > *best_depth) {
                    *best_p50 = p50;
                    *best_depth = dd;
                    *best_segs = segs;
                    *best_path = cp_path.get(dep).cloned().unwrap_or_default();
                }
            } else {
                let (ep50, ep75, active_ext) = ext_dep_rem(it, dep, need_applied);
                if active_ext && ep50 > *best_p50 {
                    *best_p50 = ep50;
                    *best_depth = 1;
                    *best_segs = vec![(ep50, ep75)];
                    *best_path = vec![dep.to_string()];
                }
            }
        };

        for dep in strings(&it["after"]) {
            check_dep(&dep, false, &mut best_p50, &mut best_depth, &mut best_segs, &mut best_path);
        }
        for dep in strings(&it["afterApplied"]) {
            check_dep(&dep, true, &mut best_p50, &mut best_depth, &mut best_segs, &mut best_path);
        }

        let mut segs_del = best_segs.clone();
        let mut segs_app = best_segs;
        if !done {
            segs_del.push((d50, d75));
            segs_app.push((a50, a75));
        }
        let (tot50, tot75) = combine_p50_p75(&segs_app);
        let depth_here = best_depth + usize::from(!done);
        best_path.push(nm.clone());

        cp_depth.insert(nm.clone(), depth_here);
        cp_segs_del.insert(nm.clone(), segs_del);
        cp_segs_app.insert(nm.clone(), segs_app);
        cp_p50_app.insert(nm.clone(), tot50);
        cp_p75_app.insert(nm.clone(), tot75);
        cp_path.insert(nm.clone(), best_path);
    }

    let mut write_chains = 0usize;
    let mut max_depth = 0usize;
    let mut max_cp_p50 = 0.0_f64;
    let mut max_cp_p75 = 0.0_f64;
    let mut critical_path = Vec::new();
    let mut sum_del_p50 = 0.0_f64;
    let mut sum_del_spread_sq = 0.0_f64;
    let mut blockers = Vec::new();
    let mut items_out = Vec::new();

    for it in &active_q {
        let nm = s(it, "name").to_string();
        let repo = s(it, "repo").to_string();
        let st = s(it, "status").to_string();
        let done = node_done.contains(&nm);
        let (d50, d75) = node_del.get(&nm).copied().unwrap_or((0.0, 0.0));
        let (a50, a75) = node_app.get(&nm).copied().unwrap_or((0.0, 0.0));
        let tot50 = cp_p50_app.get(&nm).copied().unwrap_or(a50);
        let tot75 = cp_p75_app.get(&nm).copied().unwrap_or(a75);
        let blk = node_blocker.get(&nm).cloned();

        if let Some(b) = &blk {
            blockers.push(BlockerItem {
                id: nm.clone(),
                title: nm.clone(),
                blocker: b.clone(),
            });
        }

        if !done {
            write_chains += 1;
            sum_del_p50 += d50;
            let sp = (d75 - d50).max(0.0);
            sum_del_spread_sq += sp * sp;
            let dd = cp_depth.get(&nm).copied().unwrap_or(1);
            if tot50 > max_cp_p50 || (tot50 == max_cp_p50 && dd > max_depth) {
                max_cp_p50 = tot50;
                max_cp_p75 = tot75;
                max_depth = dd;
                critical_path = cp_path.get(&nm).cloned().unwrap_or_default();
            }
        }

        let rem = (!done && blk.is_none()).then(|| EtaRange::from_remaining(now_ts, tot50, tot75, false));
        let (short_text, focus_badge) = if let Some(b) = &blk {
            (b.label.clone(), b.kind.compact_badge().to_string())
        } else if let Some(r) = &rem {
            let c50 = format_clock(r.rounded_p50_at, now_ts);
            let c75 = format_clock(r.rounded_p75_at, now_ts);
            (format!("约 {c50} 合入（p75 {c75}）"), format!("~{c50}"))
        } else {
            ("已完成".into(), "完成".into())
        };

        let compact_badge = if let Some(b) = &blk {
            b.kind.compact_badge().to_string()
        } else if let Some(r) = &rem {
            format_compact_clock(r.rounded_p50_at, now_ts, r.p50_sec)
        } else {
            "完成".into()
        };

        items_out.push(WorkItemEta {
            id: nm.clone(),
            title: nm.clone(),
            repo,
            state: st,
            class: String::new(),
            active_run: node_run.get(&nm).cloned(),
            needs_verify: false,
            external_est_at: None,
            blocker: blk,
            remaining: rem,
            dev_p50_sec: a50,
            dev_p75_sec: a75,
            upstream_path: cp_path.get(&nm).cloned().unwrap_or_default(),
            short_text,
            focus_badge,
            compact_badge,
            is_prior: any_prior,
            now_ts,
        });
    }

    let formula_res = model.resolve_formula(&primary_repo);
    if formula_res.level.is_prior() && write_chains > 0 {
        any_prior = true;
    }
    let fill_target = ctx.fill_target_for_repo(&primary_repo);
    let p_eff_hist = formula_res.params.p_eff_mean.unwrap_or(2.0);
    let p_eff_now = effective_concurrency(active_count, formula_res.params, fill_target);
    let t_merge_50 = formula_res.params.t_merge_p50_sec.unwrap_or(540.0);
    let t_merge_75 = formula_res.params.t_merge_p75_sec.unwrap_or(t_merge_50).max(t_merge_50);

    let (dev_wall_p50, dev_wall_p75, finish_p50_at, finish_p75_at) = if write_chains == 0 {
        (0.0, 0.0, None, None)
    } else {
        let tp_50 = sum_del_p50 / p_eff_now + t_merge_50;
        let tp_spread = (sum_del_spread_sq.sqrt() / p_eff_now).hypot((t_merge_75 - t_merge_50).max(0.0));
        let tp_75 = tp_50 + tp_spread;
        let w50 = max_cp_p50.max(tp_50);
        let w75 = max_cp_p75.max(tp_75).max(w50);
        (
            w50,
            w75,
            Some(now_ts + w50.round() as i64),
            Some(now_ts + w75.round() as i64),
        )
    };

    let headline = if write_chains == 0 {
        "DAG 队列无待推进步骤".into()
    } else if !blockers.is_empty() {
        format!(
            "DAG 关键路径：剩 {max_depth} 步，停表（{} 项阻塞：{}）",
            blockers.len(),
            blockers[0].blocker.label
        )
    } else {
        let prior_tag = if any_prior { "(初估)" } else { "" };
        format!(
            "DAG 关键路径：剩 {max_depth} 步，约 {} 全部合入（p75 {}）{prior_tag}",
            format_clock(round_eta_epoch(now_ts, finish_p50_at.unwrap_or(now_ts)), now_ts),
            format_clock(round_eta_epoch(now_ts, finish_p75_at.unwrap_or(now_ts)), now_ts),
        )
    };

    DagEtaSummary {
        scope: "queue".into(),
        write_chains,
        depth: max_depth,
        active_count,
        p_eff_now,
        p_eff_live: active_count,
        p_eff_hist,
        fill_target,
        lane_congested,
        lane_extra_sec,
        dev_wall_p50_sec: speed::round2(dev_wall_p50),
        dev_wall_p75_sec: speed::round2(dev_wall_p75),
        earliest_finish_p50_at: finish_p50_at,
        earliest_finish_p75_at: finish_p75_at,
        critical_path,
        external_wait_count: 0,
        deferred_count: 0,
        earliest_deferred_at: None,
        date_wait_count: 0,
        earliest_date_wait_at: None,
        owner_blocked: Vec::new(),
        blockers,
        is_prior: any_prior,
        headline,
        items: items_out,
        now_ts,
    }
}

/// Format a single tail ETA line for `delegate wait` / `delegate wait --since` (design.md §4):
/// e.g. `其余 4 条在跑：最早 02:55（feat-a），本波约 03:40 交付；链 [W12→W15] 剩 2 步，约 06:30 合入`.
/// Returns `None` when no other runs are active and no open work DAG chains remain.
pub fn format_wait_tail_eta(
    active_runs: &[PathBuf],
    settled_ids: &HashSet<String>,
    items: &[WorkItem],
    repo_filter: Option<&str>,
    model: &EtaModel,
    ctx: &LiveContext,
) -> Option<String> {
    let now_ts = ctx.now_ts;
    let mut seen_run: HashSet<String> = HashSet::new();
    let mut active_etas: Vec<RunEta> = Vec::new();
    for rdir in active_runs {
        let meta = json(rdir.join("meta.json"));
        let rid = run_id_from(rdir, &meta);
        let fname = rdir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if rid.is_empty()
            || settled_ids.contains(&rid)
            || (!fname.is_empty() && settled_ids.contains(&fname))
            || !seen_run.insert(rid)
        {
            continue;
        }
        if !runs::active(&runs::state(rdir)) {
            continue;
        }
        let re = estimate_run_in(rdir, model, ctx);
        if re.stage != Stage::Done {
            active_etas.push(re);
        }
    }

    let mut segs: Vec<String> = Vec::new();

    if !active_etas.is_empty() {
        let moving: Vec<&RunEta> = active_etas
            .iter()
            .filter(|r| r.blocker.is_none() && r.to_delivered.is_some())
            .collect();
        let stopped_n = active_etas.len().saturating_sub(moving.len());
        if active_etas.len() == 1 {
            if let Some(r) = moving.first() {
                let d = r.to_delivered.as_ref().unwrap();
                let verb = if r.mode == "write" { "交付" } else { "答复" };
                let clk = if d.p50_sec < 300.0 {
                    "<5m".to_string()
                } else {
                    format_clock(d.rounded_p50_at, now_ts)
                };
                segs.push(format!(
                    "其余 1 条在跑：{} 预计 {} {}",
                    clip(&r.name, 20),
                    clk,
                    verb
                ));
            } else {
                let r = &active_etas[0];
                segs.push(format!(
                    "其余 1 条在跑：{}（{}）",
                    clip(&r.name, 20),
                    r.short_text
                ));
            }
        } else if !moving.is_empty() {
            let earliest = moving
                .iter()
                .min_by(|a, b| {
                    let da = a.to_delivered.as_ref().unwrap();
                    let db = b.to_delivered.as_ref().unwrap();
                    da.rounded_p50_at
                        .cmp(&db.rounded_p50_at)
                        .then_with(|| da.p50_sec.total_cmp(&db.p50_sec))
                        .then_with(|| a.name.cmp(&b.name))
                })
                .unwrap();
            let wave_end = moving
                .iter()
                .max_by(|a, b| {
                    let da = a.to_delivered.as_ref().unwrap();
                    let db = b.to_delivered.as_ref().unwrap();
                    da.rounded_p50_at
                        .cmp(&db.rounded_p50_at)
                        .then_with(|| da.p50_sec.total_cmp(&db.p50_sec))
                })
                .unwrap();
            let ed = earliest.to_delivered.as_ref().unwrap();
            let wd = wave_end.to_delivered.as_ref().unwrap();
            let e_clk = if ed.p50_sec < 300.0 {
                "<5m".to_string()
            } else {
                format_clock(ed.rounded_p50_at, now_ts)
            };
            let w_clk = if wd.p50_sec < 300.0 {
                "<5m".to_string()
            } else {
                format_clock(wd.rounded_p50_at, now_ts)
            };
            let stopped_note = if stopped_n > 0 {
                format!("（{stopped_n} 条停表）")
            } else {
                String::new()
            };
            segs.push(format!(
                "其余 {} 条在跑：最早 {}（{}），本波约 {} 交付{}",
                active_etas.len(),
                e_clk,
                clip(&earliest.name, 20),
                w_clk,
                stopped_note
            ));
        } else {
            let first = &active_etas[0];
            segs.push(format!(
                "其余 {} 条在跑均停表：{}（{}）",
                active_etas.len(),
                clip(&first.name, 20),
                first.short_text
            ));
        }
    }

    if !items.is_empty() {
        let effective_repo = repo_filter.filter(|rf| {
            items.iter().any(|w| {
                w.owner.is_none()
                    && w.state != "done"
                    && speed::repo_matches_filter(&w.repo, &speed::short_repo_name(&w.repo), rf)
            })
        });
        let dag = estimate_work_dag_in(items, effective_repo, model, ctx);
        if dag.write_chains > 0 && !dag.critical_path.is_empty() {
            let chain_str = if dag.critical_path.len() <= 3 {
                dag.critical_path.join("→")
            } else {
                format!(
                    "{}→…→{}",
                    dag.critical_path.first().unwrap(),
                    dag.critical_path.last().unwrap()
                )
            };
            let steps = dag.depth.max(1);
            // Use the critical chain's own tail ETA (matching `delegate eta <tail>`),
            // never the global DAG max across unrelated/deferred chains.
            let chain_p50_at = dag
                .critical_path
                .last()
                .and_then(|tail_id| dag.items.iter().find(|it| &it.id == tail_id))
                .and_then(|it| it.remaining.as_ref().map(|r| r.rounded_p50_at))
                .or_else(|| dag.earliest_finish_p50_at.map(|t| round_eta_epoch(now_ts, t)));
            if let Some(p50_at) = chain_p50_at {
                let c50 = format_clock(p50_at, now_ts);
                let prior = if dag.is_prior { "(初估)" } else { "" };
                segs.push(format!("链 [{chain_str}] 剩 {steps} 步，约 {c50} 合入{prior}"));
            }
        } else if !dag.owner_blocked.is_empty() {
            segs.push(format!("工作表停表：{} 项卡所有者", dag.owner_blocked.len()));
        }
    }

    if segs.is_empty() {
        None
    } else {
        Some(segs.join("；"))
    }
}

// ---------------------------------------------------------------------------
// CLI entrypoint: `delegate eta [<run>|<Wxx>] [--work] [--queue] [--repo PATH] [--all] [--json]`
// ---------------------------------------------------------------------------

pub fn command(args: &[String]) -> Res<i32> {
    let (pos, flags, kv) = parse_simple(
        "eta",
        args,
        &["--work", "--queue", "--all", "--json"],
        &["--repo"],
    )?;
    if pos.len() > 1 {
        return Err(format!("eta takes at most one target (<run> or <Wxx>), got {}", pos.join(" ")));
    }
    let want_work = has(&flags, "--work");
    let want_queue = has(&flags, "--queue");
    let want_all = has(&flags, "--all");
    let want_json = has(&flags, "--json") || crate::output::json_enabled();
    let repo_opt = value(&kv, "--repo");

    let state = state_dir();
    let now_ts = epoch() as i64;
    let model = speed::load_or_refresh_eta_model_in(&state, now_ts, true);

    let default_repo = if want_all {
        None
    } else if let Some(r) = repo_opt {
        Some(r.to_string())
    } else {
        let cur = work::current_repo_str();
        (!cur.is_empty()).then_some(cur)
    };

    // Collect visible runs (current repo runs + machine-active runs) without locks.
    let mut visible_runs = runs::all_runs();
    for mrun in launch::machine_runs(&state) {
        if !visible_runs.contains(&mrun) {
            visible_runs.push(mrun);
        }
    }
    let ctx = LiveContext::capture(&state, now_ts, &visible_runs);

    // 1. Explicit target: <Wxx> or <run>
    if let Some(target) = pos.first() {
        if let Some(wid) = work::normalize_work_id(target) {
            let items = work::load_readonly_in(&state);
            let Some(item_eta) = estimate_work_item_in(&wid, &items, &model, &ctx) else {
                return Err(format!(
                    "work item {wid} not found or already closed (try: delegate work show {wid})"
                ));
            };
            if want_json {
                let doc = json!({
                    "at": format_epoch(now_ts, false, "%Y-%m-%dT%H:%M:%SZ"),
                    "kind": "work_item",
                    "workItem": item_eta.to_json(),
                });
                println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
            } else {
                println!(
                    "{} [{}] {} — {}",
                    item_eta.id,
                    item_eta.state,
                    clip(&item_eta.title, 48),
                    item_eta.short_text
                );
                if item_eta.upstream_path.len() > 1 {
                    println!("  关键路径：{}", item_eta.upstream_path.join(" → "));
                }
                if let Some(b) = &item_eta.blocker {
                    println!("  卡点：{}", b.label);
                }
                println!("  详情：delegate work show {}", item_eta.id);
            }
            return Ok(0);
        }

        let run_dir = runs::resolve_head(target)?;
        let re = estimate_run_in(&run_dir, &model, &ctx);
        if want_json {
            let doc = json!({
                "at": format_epoch(now_ts, false, "%Y-%m-%dT%H:%M:%SZ"),
                "kind": "run",
                "run": re.to_json(),
            });
            println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        } else {
            println!("{} ({}) — {}", re.name, re.run, re.short_text);
            println!("  {}", re.detail_text);
            if let Some(lw) = &re.lane_wait {
                println!(
                    "  lane 排队：前方 {} 个，当前 {} 个占用（上限 {}），预计等 {}",
                    lw.ahead,
                    lw.holders,
                    lw.max_heavy,
                    format_eta_duration(lw.wait_p50_sec)
                );
            }
            if let Some(cw) = &re.capacity_wait {
                println!(
                    "  容量排队：{}（前方 {} 个），预计等 {}",
                    cw.reason,
                    cw.ahead,
                    format_eta_duration(cw.wait_p50_sec)
                );
            }
            println!(
                "  分桶：{}（{}） · 详情：delegate status {}",
                re.bucket_key,
                re.fallback_level.as_str(),
                re.run
            );
        }
        return Ok(0);
    }

    // 2. --work only
    if want_work && !want_queue {
        let items = work::load_readonly_in(&state);
        let dag = estimate_work_dag_in(&items, default_repo.as_deref(), &model, &ctx);
        if want_json {
            let doc = json!({
                "at": format_epoch(now_ts, false, "%Y-%m-%dT%H:%M:%SZ"),
                "kind": "work_dag",
                "work": dag.to_json(),
            });
            println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        } else {
            print!("{}", format_dag_human(&dag));
        }
        return Ok(0);
    }

    // 3. --queue only
    if want_queue && !want_work {
        let q_items = dag_queue::load_items(&state);
        let dag = estimate_queue_dag_in(&q_items, default_repo.as_deref(), &model, &ctx);
        if want_json {
            let doc = json!({
                "at": format_epoch(now_ts, false, "%Y-%m-%dT%H:%M:%SZ"),
                "kind": "queue_dag",
                "queue": dag.to_json(),
            });
            println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        } else {
            print!("{}", format_dag_human(&dag));
        }
        return Ok(0);
    }

    // 4. Unified overview: active/blocked runs + work DAG + queue DAG
    let mut run_etas: Vec<RunEta> = Vec::new();
    for rdir in &visible_runs {
        let meta = json(rdir.join("meta.json"));
        let rrepo = repo_from_meta(&meta);
        if let Some(rf) = default_repo.as_deref() {
            let rn = speed::short_repo_name(&rrepo);
            if !speed::repo_matches_filter(&rrepo, &rn, rf) {
                continue;
            }
        }
        let re = estimate_run_in(rdir, &model, &ctx);
        if re.stage != Stage::Done {
            run_etas.push(re);
        }
    }
    let items = work::load_readonly_in(&state);
    let work_dag = estimate_work_dag_in(&items, default_repo.as_deref(), &model, &ctx);
    let q_items = dag_queue::load_items(&state);
    let queue_dag = estimate_queue_dag_in(&q_items, default_repo.as_deref(), &model, &ctx);

    if want_json {
        let doc = json!({
            "at": format_epoch(now_ts, false, "%Y-%m-%dT%H:%M:%SZ"),
            "kind": "overview",
            "model": {
                "generatedAt": model.generated_at,
                "eventsSeq": model.events_seq,
                "windowDays": model.window_days,
                "recentLaneQueueShare": (model.global.recent_lane_2h.queue_share * 10000.0).round() / 10000.0,
            },
            "runs": run_etas.iter().map(RunEta::to_json).collect::<Vec<_>>(),
            "work": work_dag.to_json(),
            "queue": queue_dag.to_json(),
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        if run_etas.is_empty() && work_dag.write_chains == 0 && work_dag.owner_blocked.is_empty() && queue_dag.write_chains == 0 {
            println!("delegate eta：当前无在跑或待推进任务");
            println!("校准回测：delegate speed --backtest");
            return Ok(0);
        }
        if !run_etas.is_empty() {
            println!("任务 ETA（{} 条未完成）：", run_etas.len());
            for r in &run_etas {
                println!("  {:<22} {:<10} {}", clip(&r.name, 22), r.stage_label, r.short_text);
            }
        }
        if work_dag.write_chains > 0 || !work_dag.owner_blocked.is_empty() || work_dag.external_wait_count > 0 {
            println!("工作表：{}", work_dag.headline);
            for ob in &work_dag.owner_blocked {
                println!("  {} {}", ob.id, ob.blocker.label);
            }
        }
        if queue_dag.write_chains > 0 || !queue_dag.blockers.is_empty() {
            println!("队列：{}", queue_dag.headline);
        }
        println!("详情：delegate eta <run|Wxx> · 工作表：delegate eta --work · 回测：delegate speed --backtest");
    }
    Ok(0)
}

fn format_dag_human(dag: &DagEtaSummary) -> String {
    let mut out = String::new();
    out.push_str(&format!("{}\n", dag.headline));
    if !dag.critical_path.is_empty() {
        out.push_str(&format!(
            "  关键路径：{}（P_eff={:.1}{}）\n",
            dag.critical_path.join(" → "),
            dag.p_eff_now,
            if dag.lane_congested { "，lane 拥堵已折入" } else { "" },
        ));
    }
    for b in &dag.blockers {
        out.push_str(&format!("  卡点 {}：{} — {}\n", b.id, clip(&b.title, 36), b.blocker.label));
    }
    for ob in &dag.owner_blocked {
        out.push_str(&format!("  外部 {}：{} — {}\n", ob.id, clip(&ob.title, 36), ob.blocker.label));
    }
    for it in &dag.items {
        out.push_str(&format!(
            "  {:<6} {:<8} {:<32} {}\n",
            it.id,
            it.state,
            clip(&it.title, 32),
            it.short_text
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Snapshot wave summary & online milestone calibration
// ---------------------------------------------------------------------------

pub const ETA_PRED_FILE: &str = "eta-pred.json";

/// Summarize the current live wave of runs for `delegate snapshot --json` (TUI Overview card).
pub fn summarize_wave_eta(run_etas: &[RunEta], now_ts: i64) -> Value {
    let mut active_count = 0usize;
    let mut running_count = 0usize;
    let mut queued_count = 0usize;
    let mut blocked_count = 0usize;
    let mut earliest_p50_at: Option<i64> = None;
    let mut wave_p50_at: Option<i64> = None;
    let mut wave_p75_at: Option<i64> = None;
    let mut is_prior = false;
    let mut blockers: Vec<Value> = Vec::new();

    for re in run_etas {
        let is_live = runs::active(&re.state);
        if is_live {
            active_count += 1;
            if re.stage == Stage::CapacityQueue || re.state == "waiting" {
                queued_count += 1;
            } else {
                running_count += 1;
            }
        }
        if let Some(b) = &re.blocker {
            blocked_count += 1;
            blockers.push(json!({
                "run": re.run,
                "name": re.name,
                "kind": b.kind.as_str(),
                "compact": b.kind.compact_badge(),
                "label": b.label,
            }));
        } else if is_live {
            if let Some(d) = &re.to_delivered {
                if re.is_prior {
                    is_prior = true;
                }
                earliest_p50_at = Some(match earliest_p50_at {
                    Some(cur) => cur.min(d.rounded_p50_at),
                    None => d.rounded_p50_at,
                });
                wave_p50_at = Some(match wave_p50_at {
                    Some(cur) => cur.max(d.rounded_p50_at),
                    None => d.rounded_p50_at,
                });
                wave_p75_at = Some(match wave_p75_at {
                    Some(cur) => cur.max(d.rounded_p75_at),
                    None => d.rounded_p75_at,
                });
            }
        }
    }

    let mut obj = json!({
        "activeCount": active_count,
        "runningCount": running_count,
        "queuedCount": queued_count,
        "blockedCount": blocked_count,
        "isPrior": is_prior,
        "blockers": blockers,
    });
    if let Some(t) = earliest_p50_at {
        obj["earliestP50At"] = json!(format_epoch(t, false, "%Y-%m-%dT%H:%M:%SZ"));
        obj["earliestP50Clock"] = json!(format_clock(t, now_ts));
    }
    if let Some(t) = wave_p50_at {
        obj["waveP50At"] = json!(format_epoch(t, false, "%Y-%m-%dT%H:%M:%SZ"));
        obj["waveP50Clock"] = json!(format_clock(t, now_ts));
    }
    if let Some(t) = wave_p75_at {
        obj["waveP75At"] = json!(format_epoch(t, false, "%Y-%m-%dT%H:%M:%SZ"));
        obj["waveP75Clock"] = json!(format_clock(t, now_ts));
    }
    obj
}

fn pred_path(run: &Path) -> PathBuf {
    run.join(ETA_PRED_FILE)
}

/// Record initial ETA prediction into `<run>/eta-pred.json` when a run transitions to `started`.
/// Strictly lock-free, best-effort, never fails the run.
pub fn record_pred_started(run: &Path, meta: &Value) {
    let state = state_dir();
    let now_ts = epoch() as i64;
    record_pred_started_in(&state, run, meta, now_ts);
}

pub fn record_pred_started_in(state: &Path, run: &Path, meta: &Value, now_ts: i64) {
    let path = pred_path(run);
    let mut doc = json(&path);
    if doc.is_object() && doc["started"].is_object() {
        return;
    }

    let run_id = run_id_from(run, meta);
    let repo = repo_from_meta(meta);
    let raw_mode = {
        let m = s(meta, "mode");
        if m.is_empty() { "write" } else { m }
    };
    let (norm_mode, norm_class) =
        speed::normalize_mode_class(raw_mode, s(meta, "class"), s(meta, "tier"));
    let is_write = norm_mode == "write";

    let model = speed::load_or_refresh_eta_model_in(state, now_ts, false);
    let e2e = model.resolve_dist(&repo, &norm_mode, &norm_class, |b| &b.run_to_delivered);
    let (del_p50, del_p75) = match (e2e.dist.p50_sec, e2e.dist.p75_sec) {
        (Some(a), Some(b)) => (a.max(1.0), b.max(a.max(1.0))),
        (Some(a), None) => (a.max(1.0), (a * 1.4).max(1.0)),
        _ if is_write => (35.0 * 60.0, 50.0 * 60.0),
        _ => (5.0 * 60.0, 10.0 * 60.0),
    };

    let (app_p50, app_p75) = if is_write {
        let ap = model.resolve_dist(&repo, "write", &norm_class, |b| &b.chain_to_applied);
        match (ap.dist.p50_sec, ap.dist.p75_sec) {
            (Some(a), Some(b)) => (Some(a.max(del_p50)), Some(b.max(a.max(del_p50)))),
            (Some(a), None) => (Some(a.max(del_p50)), Some((a * 1.38).max(del_p75))),
            _ => (Some((63.0 * 60.0_f64).max(del_p50)), Some((87.0 * 60.0_f64).max(del_p75))),
        }
    } else {
        (None, None)
    };

    let started_epoch = meta["startedEpoch"]
        .as_f64()
        .map(|f| f.round() as i64)
        .or_else(|| iso_epoch(s(meta, "startedAt")))
        .unwrap_or(now_ts);
    let started_at = format_epoch(started_epoch, false, "%Y-%m-%dT%H:%M:%SZ");

    let mut started_obj = json!({
        "at": started_at,
        "epoch": started_epoch,
        "stage": Stage::AgentWork.as_str(),
        "fallbackLevel": e2e.level.as_str(),
        "bucketKey": e2e.key,
        "isPrior": e2e.level.is_prior(),
        "toDeliveredP50Sec": speed::round2(del_p50),
        "toDeliveredP75Sec": speed::round2(del_p75),
    });
    if let (Some(a50), Some(a75)) = (app_p50, app_p75) {
        started_obj["toAppliedP50Sec"] = json!(speed::round2(a50));
        started_obj["toAppliedP75Sec"] = json!(speed::round2(a75));
    }

    if !doc.is_object() {
        doc = json!({
            "version": 1,
            "run": run_id,
            "repo": repo,
            "repoName": speed::short_repo_name(&repo),
            "mode": norm_mode,
            "class": norm_class,
        });
        if !s(meta, "parent").is_empty() {
            doc["parent"] = json!(s(meta, "parent"));
        }
    }
    doc["started"] = started_obj;
    let _ = write_json(&path, &doc);
}

/// Record first-entry `accept`/`review` ETA prediction into `<run>/eta-pred.json`.
/// Strictly lock-free, recorded once per run, best-effort.
pub fn record_pred_accept_review(run: &Path, meta: &Value) {
    let state = state_dir();
    let now_ts = epoch() as i64;
    record_pred_accept_review_in(&state, run, meta, now_ts);
}

pub fn record_pred_accept_review_in(state: &Path, run: &Path, meta: &Value, now_ts: i64) {
    let path = pred_path(run);
    let mut doc = json(&path);
    if doc.is_object() && doc["acceptReview"].is_object() {
        return;
    }

    let run_id = run_id_from(run, meta);
    let repo = repo_from_meta(meta);
    let raw_mode = {
        let m = s(meta, "mode");
        if m.is_empty() { "write" } else { m }
    };
    let (norm_mode, norm_class) =
        speed::normalize_mode_class(raw_mode, s(meta, "class"), s(meta, "tier"));
    let review_enabled = meta["reviewConfig"].is_object() && b(&meta["reviewConfig"], "enabled");

    let model = speed::load_or_refresh_eta_model_in(state, now_ts, false);
    let (p50, p75, level, key, stage_str) = if review_enabled {
        let par = model.resolve_dist(&repo, &norm_mode, &norm_class, |b| &b.accept_review_parallel);
        let a50 = par.dist.p50_sec.unwrap_or(300.0).max(1.0);
        let a75 = par.dist.p75_sec.unwrap_or(a50 * 1.6).max(a50);
        let suf = model.resolve_dist(&repo, &norm_mode, &norm_class, |b| &b.suffix_after_review);
        let s50 = suf.dist.p50_sec.unwrap_or(0.0).max(0.0);
        let s75 = suf.dist.p75_sec.unwrap_or(s50).max(s50);
        let (t50, t75) = combine_p50_p75(&[(a50, a75), (s50, s75)]);
        (t50, t75, par.level, par.key, Stage::ReviewParallel.as_str())
    } else {
        let acc = resolve_run_accept_dist(run, meta, &repo, &model);
        let a50 = acc.dist.p50_sec.unwrap_or(180.0).max(1.0);
        let a75 = acc.dist.p75_sec.unwrap_or(a50 * 1.6).max(a50);
        let suf = model.resolve_dist(&repo, &norm_mode, &norm_class, |b| &b.suffix_after_accept);
        let s50 = suf.dist.p50_sec.unwrap_or(0.0).max(0.0);
        let s75 = suf.dist.p75_sec.unwrap_or(s50).max(s50);
        let (t50, t75) = combine_p50_p75(&[(a50, a75), (s50, s75)]);
        (t50, t75, acc.level, acc.key, Stage::Accept.as_str())
    };

    if !doc.is_object() {
        doc = json!({
            "version": 1,
            "run": run_id,
            "repo": repo,
            "repoName": speed::short_repo_name(&repo),
            "mode": norm_mode,
            "class": norm_class,
        });
        if !s(meta, "parent").is_empty() {
            doc["parent"] = json!(s(meta, "parent"));
        }
    }
    doc["acceptReview"] = json!({
        "at": format_epoch(now_ts, false, "%Y-%m-%dT%H:%M:%SZ"),
        "epoch": now_ts,
        "stage": stage_str,
        "fallbackLevel": level.as_str(),
        "bucketKey": key,
        "isPrior": level.is_prior(),
        "toDeliveredP50Sec": speed::round2(p50),
        "toDeliveredP75Sec": speed::round2(p75),
    });
    let _ = write_json(&path, &doc);
}

#[allow(clippy::too_many_arguments)]
fn append_calibration_record(
    state: &Path,
    at_iso: &str,
    run_id: &str,
    repo: &str,
    mode: &str,
    class: &str,
    checkpoint: &str,
    milestone: &str,
    pred_p50: f64,
    pred_p75: f64,
    actual_sec: f64,
    fallback_level: &str,
    is_prior: bool,
) {
    let actual = actual_sec.max(0.0);
    let p50 = pred_p50.max(0.0);
    let p75 = pred_p75.max(p50);
    let within_p75 = actual <= p75 + 1e-6;
    let rel_bias = if actual > 0.0 {
        Some(((actual - p50) / actual * 10000.0).round() / 10000.0)
    } else {
        None
    };
    let mut rec = json!({
        "at": at_iso,
        "run": run_id,
        "repo": repo,
        "repoName": speed::short_repo_name(repo),
        "mode": mode,
        "class": class,
        "checkpoint": checkpoint,
        "milestone": milestone,
        "predP50Sec": speed::round2(p50),
        "predP75Sec": speed::round2(p75),
        "actualSec": speed::round2(actual),
        "withinP75": within_p75,
        "fallbackLevel": fallback_level,
        "isPrior": is_prior,
    });
    if let Some(rb) = rel_bias {
        rec["relBias"] = json!(rb);
    }
    let cal_path = speed::eta_calibration_path(state);
    if let Some(parent) = cal_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = append_synced(&cal_path, &format!("{rec}\n"));
}

/// Append prediction-vs-actual records to `<state>/eta-calibration.jsonl` when a run delivers.
/// Strictly lock-free, idempotent via `recordedDelivered`, best-effort.
pub fn record_calibration_delivered(run: &Path, meta: &Value, summary: &Value) {
    let state = state_dir();
    let now_ts = iso_epoch(s(summary, "finishedAt")).unwrap_or_else(|| epoch() as i64);
    record_calibration_delivered_in(&state, run, meta, summary, now_ts);
}

pub fn record_calibration_delivered_in(
    state: &Path,
    run: &Path,
    meta: &Value,
    summary: &Value,
    finished_ts: i64,
) {
    let st = s(summary, "state");
    if !matches!(st, "delivered" | "answered")
        || b(summary, "question")
        || b(summary, "policyRefused")
    {
        return;
    }
    let path = pred_path(run);
    let mut doc = json(&path);
    if !doc.is_object() || b(&doc, "recordedDelivered") {
        return;
    }
    let run_id = {
        let r = s(&doc, "run");
        if r.is_empty() { run_id_from(run, meta) } else { r.to_string() }
    };
    let repo = {
        let r = s(&doc, "repo");
        if r.is_empty() { repo_from_meta(meta) } else { r.to_string() }
    };
    let mode = {
        let m = s(&doc, "mode");
        if m.is_empty() { "write" } else { m }
    }
    .to_string();
    let class = {
        let c = s(&doc, "class");
        if c.is_empty() { "other" } else { c }
    }
    .to_string();
    let milestone = if mode == "write" { "delivered" } else { "answered" };
    let at_iso = if !s(summary, "finishedAt").is_empty() {
        s(summary, "finishedAt").to_string()
    } else {
        format_epoch(finished_ts, false, "%Y-%m-%dT%H:%M:%SZ")
    };

    let mut wrote_any = false;
    if let Some(st_pred) = doc.get("started").filter(|v| v.is_object()) {
        if let (Some(p50), Some(p75), Some(t0)) = (
            st_pred["toDeliveredP50Sec"].as_f64(),
            st_pred["toDeliveredP75Sec"].as_f64(),
            st_pred["epoch"].as_i64().or_else(|| iso_epoch(s(st_pred, "at"))),
        ) {
            let actual = (finished_ts - t0).max(0) as f64;
            append_calibration_record(
                state,
                &at_iso,
                &run_id,
                &repo,
                &mode,
                &class,
                "started",
                milestone,
                p50,
                p75,
                actual,
                s(st_pred, "fallbackLevel"),
                b(st_pred, "isPrior"),
            );
            wrote_any = true;
        }
    }
    if let Some(ar_pred) = doc.get("acceptReview").filter(|v| v.is_object()) {
        if let (Some(p50), Some(p75), Some(t0)) = (
            ar_pred["toDeliveredP50Sec"].as_f64(),
            ar_pred["toDeliveredP75Sec"].as_f64(),
            ar_pred["epoch"].as_i64().or_else(|| iso_epoch(s(ar_pred, "at"))),
        ) {
            let actual = (finished_ts - t0).max(0) as f64;
            append_calibration_record(
                state,
                &at_iso,
                &run_id,
                &repo,
                &mode,
                &class,
                "accept_review",
                milestone,
                p50,
                p75,
                actual,
                s(ar_pred, "fallbackLevel"),
                b(ar_pred, "isPrior"),
            );
            wrote_any = true;
        }
    }
    if wrote_any {
        doc["recordedDelivered"] = json!(true);
        let _ = write_json(&path, &doc);
    }
}

/// Append prediction-vs-actual record to `<state>/eta-calibration.jsonl` when a write run is applied.
/// Walks `parent` within the same `runs_dir` to measure full chain wall time from chain root.
/// Strictly lock-free, idempotent via `recordedApplied`, best-effort.
pub fn record_calibration_applied(run: &Path, meta: &Value, applied_at: &str) {
    let state = state_dir();
    let applied_ts = iso_epoch(applied_at).unwrap_or_else(|| epoch() as i64);
    record_calibration_applied_in(&state, run, meta, applied_at, applied_ts);
}

pub fn record_calibration_applied_in(
    state: &Path,
    run: &Path,
    meta: &Value,
    applied_at: &str,
    applied_ts: i64,
) {
    // Walk to chain root within the same runs_dir if the root also has eta-pred.json.
    let mut target_run = run.to_path_buf();
    let mut cur_meta = meta.clone();
    let mut seen: HashSet<String> = HashSet::new();
    if let Some(runs_dir) = run.parent() {
        for _ in 0..32 {
            let parent_id = s(&cur_meta, "parent").to_string();
            if parent_id.is_empty() || !seen.insert(parent_id.clone()) {
                break;
            }
            let pdir = runs_dir.join(&parent_id);
            if pdir.join(ETA_PRED_FILE).is_file() {
                target_run = pdir.clone();
                cur_meta = json(pdir.join("meta.json"));
            } else {
                break;
            }
        }
    }

    let path = pred_path(&target_run);
    let mut doc = json(&path);
    if !doc.is_object() || b(&doc, "recordedApplied") {
        return;
    }
    let Some(st_pred) = doc.get("started").filter(|v| v.is_object()) else {
        return;
    };
    let (Some(p50), Some(p75), Some(t0)) = (
        st_pred["toAppliedP50Sec"].as_f64(),
        st_pred["toAppliedP75Sec"].as_f64(),
        st_pred["epoch"].as_i64().or_else(|| iso_epoch(s(st_pred, "at"))),
    ) else {
        return;
    };

    let run_id = {
        let r = s(&doc, "run");
        if r.is_empty() { run_id_from(&target_run, &cur_meta) } else { r.to_string() }
    };
    let repo = {
        let r = s(&doc, "repo");
        if r.is_empty() { repo_from_meta(&cur_meta) } else { r.to_string() }
    };
    let mode = {
        let m = s(&doc, "mode");
        if m.is_empty() { "write" } else { m }
    }
    .to_string();
    let class = {
        let c = s(&doc, "class");
        if c.is_empty() { "other" } else { c }
    }
    .to_string();
    let at_iso = if !applied_at.is_empty() {
        applied_at.to_string()
    } else {
        format_epoch(applied_ts, false, "%Y-%m-%dT%H:%M:%SZ")
    };
    let actual = (applied_ts - t0).max(0) as f64;
    append_calibration_record(
        state,
        &at_iso,
        &run_id,
        &repo,
        &mode,
        &class,
        "started",
        "applied",
        p50,
        p75,
        actual,
        s(st_pred, "fallbackLevel"),
        b(st_pred, "isPrior"),
    );
    doc["recordedApplied"] = json!(true);
    let _ = write_json(&path, &doc);
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speed::{build_eta_model_from_events, EventRec, ETA_MODEL_WINDOW_DAYS};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(prefix: &str) -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "delegate-eta-test-{prefix}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_ev(ts: i64, run: &str, repo: &str, kind: &str, payload: Value) -> EventRec {
        EventRec {
            seq: 1,
            ts,
            run: run.to_string(),
            repo: repo.to_string(),
            kind: kind.to_string(),
            payload,
        }
    }

    fn populated_model(state: &Path, repo: &str, now_ts: i64) -> EtaModel {
        let mut evs = Vec::new();
        let t0 = now_ts - 3 * 86_400;
        // 12 write:build runs with agent_work = 600..=1700s, accept = 120s, suffix = 120s, merge_wait = 300s
        for i in 0..12 {
            let id = format!("hist-{i}");
            let st = t0 + i * 3600;
            let work = 600 + i * 100;
            let del = st + work + 120;
            let app = del + 300;
            evs.push(make_ev(
                st,
                &id,
                repo,
                "started",
                json!({"mode": "write", "class": "build"}),
            ));
            evs.push(make_ev(
                del - 120,
                &id,
                repo,
                "lane_acquired",
                json!({"queuedSeconds": 0.0}),
            ));
            evs.push(make_ev(
                del,
                &id,
                repo,
                "lane_released",
                json!({"heldSeconds": 120.0}),
            ));
            evs.push(make_ev(
                del,
                &id,
                repo,
                "accept",
                json!({"ok": true, "tiered": false, "elapsedSeconds": 120.0, "queuedSeconds": 0.0}),
            ));
            evs.push(make_ev(del, &id, repo, "delivered", json!({"mode": "write"})));
            evs.push(make_ev(
                del + 5,
                &id,
                repo,
                "premerge_ready",
                json!({"elapsedSeconds": 5.0, "reusedAccept": true}),
            ));
            evs.push(make_ev(app, &id, repo, "applied", json!({})));
        }
        build_eta_model_from_events(state, &evs, now_ts, evs.len() as u64, ETA_MODEL_WINDOW_DAYS)
    }

    #[test]
    fn rss_p75_combination_avoids_naive_sum_inflation() {
        // Two segments each (p50=100, p75=130) -> spread=30 each
        // Naive sum p75 = 260; RSS p75 = 200 + sqrt(900 + 900) = 242.43
        let (p50, p75) = combine_p50_p75(&[(100.0, 130.0), (100.0, 130.0)]);
        assert_eq!(p50, 200.0);
        assert!((p75 - 242.43).abs() < 0.05, "got {p75}");
    }

    #[test]
    fn rounding_and_hysteresis_refresh_rules() {
        let now = 1_800_000_000_i64;
        assert_eq!(format_eta_duration(120.0), "<5m");
        assert_eq!(format_eta_duration(480.0), "8m");
        assert_eq!(format_eta_duration(1900.0), "30m"); // 31.6m -> 30m (5m step)
        assert_eq!(format_eta_duration(7200.0), "2.0h");

        // Hysteresis: small shift within max(5m, 15% * rem) does NOT refresh
        assert!(!should_refresh_shown_eta(
            Some(now + 3600),
            Some("agent_work"),
            false,
            now + 3600 + 400, // 400s < 15% of 4000s (600s)
            "agent_work",
            false,
            now
        ));
        // Large shift > max(5m, 15% * rem) DOES refresh
        assert!(should_refresh_shown_eta(
            Some(now + 3600),
            Some("agent_work"),
            false,
            now + 3600 + 900,
            "agent_work",
            false,
            now
        ));
        // Stage switch DOES refresh
        assert!(should_refresh_shown_eta(
            Some(now + 3600),
            Some("agent_work"),
            false,
            now + 3600,
            "review_parallel",
            false,
            now
        ));
        // Past shownP50At DOES refresh
        assert!(should_refresh_shown_eta(
            Some(now - 60),
            Some("agent_work"),
            false,
            now + 600,
            "agent_work",
            false,
            now
        ));
    }

    #[test]
    fn lane_wait_accounts_for_live_holders_and_queue_ahead() {
        // 6-minute (360s) heavy lane_held distribution
        let dist = SampleDist::from_secs(vec![360.0; 12]);
        // L=1 slot, 1 holder running for 60s (300s left), ahead=2 (1 queued ahead of us -> 360s)
        // Expected p50 = (300 + 360) / 1 = 660s (11m), and with L=2 slots -> halved!
        let (w50_l1, w75_l1) = compute_lane_wait(&dist, &[60.0], 1, 2);
        assert!((w50_l1 - 660.0).abs() < 1.0, "got {w50_l1}");
        assert!(w75_l1 >= w50_l1);

        let (w50_l2, _) = compute_lane_wait(&dist, &[60.0, 60.0], 2, 2);
        // 2 holders each 300s left -> sum/2 = 300s, + (2-1)*360/2 = 180s -> 480s
        assert!((w50_l2 - 480.0).abs() < 1.0, "got {w50_l2}");
    }

    #[test]
    fn effective_concurrency_caps_high_live_parallelism() {
        let params = RepoFormulaParams {
            p_eff_mean: Some(2.2),
            p_eff_p75: Some(3.0),
            ..Default::default()
        };
        // 15 live runs must be capped at P_eff,p75 (3.0), never treated as 15!
        assert_eq!(effective_concurrency(15, &params, Some(6)), 3.0);
        // Tighter fill.target (2) caps further
        assert_eq!(effective_concurrency(15, &params, Some(2)), 2.0);
        // Idle planning (0 live) uses historical mean (2.2)
        assert_eq!(effective_concurrency(0, &params, Some(6)), 2.2);
    }

    #[test]
    fn work_dag_separates_after_time_est_and_owner_blockers_from_dev_wall() {
        let state = temp_dir("work-dag");
        let now = 1_800_000_000_i64;
        let repo = "/work/demo";
        let model = populated_model(&state, repo, now);
        let ctx = LiveContext {
            now_ts: now,
            ..Default::default()
        };

        // W001 (ready) -> W002 (waits on work_applied:W001 + after_time:now+5h)
        // W003 (waits on owner:Q01) -> W004 (waits on work_applied:W003)
        // Q01 (open owner item)
        let after_iso = format_epoch(now + 5 * 3600, false, "%Y-%m-%dT%H:%M:%SZ");
        let items = vec![
            WorkItem {
                id: "W001".into(),
                title: "chain 1".into(),
                repo: repo.into(),
                state: "ready".into(),
                class: "feat".into(),
                ..Default::default()
            },
            WorkItem {
                id: "W002".into(),
                title: "chain 2 timed".into(),
                repo: repo.into(),
                state: "blocked".into(),
                class: "feat".into(),
                wait_on: vec!["work_applied:W001".into(), format!("after_time:{after_iso}")],
                ..Default::default()
            },
            WorkItem {
                id: "Q01".into(),
                title: "owner decision".into(),
                repo: repo.into(),
                state: "ready".into(),
                owner: Some(crate::work::OwnerInfo {
                    kind: "nod".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            WorkItem {
                id: "W003".into(),
                title: "waiting on owner".into(),
                repo: repo.into(),
                state: "blocked".into(),
                class: "feat".into(),
                wait_on: vec!["owner:Q01".into()],
                ..Default::default()
            },
            WorkItem {
                id: "W004".into(),
                title: "downstream of owner".into(),
                repo: repo.into(),
                state: "blocked".into(),
                class: "feat".into(),
                wait_on: vec!["work_applied:W003".into()],
                ..Default::default()
            },
        ];

        let dag = estimate_work_dag_in(&items, None, &model, &ctx);
        // Only advancing item W001 enters active DAG totals (W002 is held by after_time:,
        // and W003 & W004 are stripped into owner_blocked!).
        assert_eq!(dag.write_chains, 1);
        assert_eq!(dag.depth, 1);
        assert_eq!(dag.external_wait_count, 1);
        assert_eq!(dag.date_wait_count, 1);
        assert_eq!(dag.earliest_date_wait_at, Some(now + 5 * 3600));
        assert_eq!(dag.owner_blocked.len(), 2);
        assert!(dag.owner_blocked.iter().any(|b| b.id == "W003" && b.blocker.label.contains("卡在所有者 [Q01]")));
        assert!(dag.owner_blocked.iter().any(|b| b.id == "W004" && b.blocker.label.contains("卡在所有者 [Q01]")));

        // Active DAG wall & clock finish must NOT be dragged out by the 5-hour after_time wait!
        assert!(
            dag.dev_wall_p50_sec < 3.0 * 3600.0,
            "dev_wall should exclude 5h external wait, got {}s",
            dag.dev_wall_p50_sec
        );
        let fin50 = dag.earliest_finish_p50_at.expect("clock finish");
        assert!(
            fin50 < now + 3 * 3600,
            "active DAG clock finish must not be dragged to after_time EST, got delta={}s",
            fin50 - now
        );
        // Individual item W002 still preserves its own post-EST clock finish.
        let w2_eta = dag.items.iter().find(|it| it.id == "W002").expect("W002 item eta");
        let w2_fin50 = w2_eta.remaining.as_ref().expect("W002 remaining").p50_at;
        assert!(
            w2_fin50 >= now + 5 * 3600 + 600,
            "per-item W002 finish must respect after_time EST, got delta={}s",
            w2_fin50 - now
        );

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn mainline_block_detected_for_unblock_dep_and_shared_outside_file() {
        let state = temp_dir("mainline");
        let now = 1_800_000_000_i64;
        let repo = "/work/shared";
        let model = populated_model(&state, repo, now);

        // Path ②: 2 runs failing on the exact same outside file `scripts/size-audit.py`
        let r1 = state.join("run-1");
        let r2 = state.join("run-2");
        for (dir, id) in [(&r1, "run-1"), (&r2, "run-2")] {
            fs::create_dir_all(dir).unwrap();
            write_json(
                dir.join("meta.json"),
                &json!({"run": id, "name": id, "mode": "write", "top": repo}),
            )
            .unwrap();
            write_json(
                dir.join("summary.json"),
                &json!({
                    "state": "rejected",
                    "finishedAt": format_epoch(now - 300, false, "%Y-%m-%dT%H:%M:%SZ"),
                    "accept": {
                        "ok": false,
                        "failureAttribution": {
                            "outsideRunFiles": ["scripts/size-audit.py"]
                        }
                    }
                }),
            )
            .unwrap();
        }
        let ctx = LiveContext::capture(&state, now, &[r1.clone(), r2.clone()]);
        let e1 = estimate_run_in(&r1, &model, &ctx);
        let blk = e1.blocker.expect("mainline blocker");
        assert_eq!(blk.kind, BlockerKind::MainlineBlocked);
        assert_eq!(blk.affected_count, Some(2));
        assert!(blk.label.contains("卡在主干阻塞（2 条受阻）"), "got {}", blk.label);

        // Path ①: Work item depending on an open `class == "unblock"` item
        let items = vec![
            WorkItem {
                id: "W010".into(),
                title: "fix size-audit".into(),
                repo: repo.into(),
                state: "ready".into(),
                class: "unblock".into(),
                ..Default::default()
            },
            WorkItem {
                id: "W011".into(),
                title: "feature blocked by unblock".into(),
                repo: repo.into(),
                state: "blocked".into(),
                class: "feat".into(),
                wait_on: vec!["work_applied:W010".into()],
                ..Default::default()
            },
        ];
        let w11 = estimate_work_item_in("W011", &items, &model, &ctx).expect("W011 eta");
        let wblk = w11.blocker.expect("unblock dep blocker");
        assert_eq!(wblk.kind, BlockerKind::MainlineBlocked);
        assert!(wblk.label.contains("卡在主干修复 [W010]"), "got {}", wblk.label);
        assert!(wblk.unlock_at.is_some());

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn queue_dag_computes_critical_path_and_pauses_stop_clock() {
        let state = temp_dir("qdag");
        let now = 1_800_000_000_i64;
        let repo = "/work/qrepo";
        let model = populated_model(&state, repo, now);
        let ctx = LiveContext {
            now_ts: now,
            ..Default::default()
        };

        let q_items = vec![
            json!({"name": "step-1", "repo": repo, "status": "pending", "after": [], "afterApplied": []}),
            json!({"name": "step-2a", "repo": repo, "status": "pending", "after": [], "afterApplied": ["step-1"]}),
            json!({"name": "step-2b", "repo": repo, "status": "pending", "after": [], "afterApplied": ["step-1"]}),
            json!({"name": "step-3", "repo": repo, "status": "pending", "after": [], "afterApplied": ["step-2a", "step-2b"]}),
        ];
        let dag = estimate_queue_dag_in(&q_items, None, &model, &ctx);
        assert_eq!(dag.write_chains, 4);
        assert_eq!(dag.depth, 3);
        assert!(dag.headline.contains("DAG 关键路径：剩 3 步"), "got {}", dag.headline);

        // Paused item triggers stop-clock headline
        let paused_q = vec![
            json!({"name": "step-1", "repo": repo, "status": "paused", "pauseReason": "上游提问，等待主控决定", "after": [], "afterApplied": []}),
        ];
        let pdag = estimate_queue_dag_in(&paused_q, None, &model, &ctx);
        assert_eq!(pdag.blockers.len(), 1);
        assert!(pdag.headline.contains("停表"), "got {}", pdag.headline);

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn format_compact_clock_respects_six_column_budget() {
        let now = 1_800_000_000_i64;
        assert_eq!(format_compact_clock(now + 120, now, 120.0), "<5m");
        let mid = format_compact_clock(now + 1800, now, 1800.0);
        assert!(mid.starts_with('~') && mid.len() == 6, "got {mid}");
        let day1 = format_compact_clock(now + 24 * 3600, now, 24.0 * 3600.0);
        assert_eq!(day1, "~24h");
        assert!(day1.len() <= 6);
        let day3 = format_compact_clock(now + 72 * 3600, now, 72.0 * 3600.0);
        assert_eq!(day3, "~3d");
        assert!(day3.len() <= 6);
    }

    #[test]
    fn summarize_wave_eta_computes_earliest_wave_and_blockers() {
        let now = 1_800_000_000_i64;
        let active_fast = RunEta {
            run: "r1".into(),
            name: "fast".into(),
            repo: "/repo".into(),
            mode: "write".into(),
            class: "build".into(),
            state: "running".into(),
            stage: Stage::AgentWork,
            stage_label: "模型工作中".into(),
            stage_elapsed_sec: Some(60.0),
            round: 1,
            target_milestone: "delivered",
            to_delivered: Some(EtaRange::from_remaining(now, 600.0, 900.0, false)),
            to_applied: None,
            blocker: None,
            lane_wait: None,
            capacity_wait: None,
            fallback_level: FallbackLevel::RepoModeClass,
            bucket_key: "repo:write:build".into(),
            is_prior: false,
            rework_rate: None,
            short_text: String::new(),
            compact_badge: "~01:10".into(),
            detail_text: String::new(),
            now_ts: now,
        };
        let mut active_slow = active_fast.clone();
        active_slow.run = "r2".into();
        active_slow.name = "slow".into();
        active_slow.state = "waiting".into();
        active_slow.stage = Stage::CapacityQueue;
        active_slow.to_delivered = Some(EtaRange::from_remaining(now, 2400.0, 3600.0, false));

        let mut blocked = active_fast.clone();
        blocked.run = "r3".into();
        blocked.name = "need-sign".into();
        blocked.state = "delivered".into();
        blocked.stage = Stage::Blocked;
        blocked.to_delivered = None;
        blocked.blocker = Some(BlockerInfo {
            kind: BlockerKind::ControllerSign,
            who: "主控".into(),
            reason: "待签字".into(),
            since_sec: Some(600),
            target: None,
            affected_count: None,
            unlock_at: None,
            after_unlock_p50_sec: None,
            after_unlock_p75_sec: None,
            label: "卡在主控·待签字 10m".into(),
        });

        let w = summarize_wave_eta(&[active_fast, active_slow, blocked], now);
        assert_eq!(w["activeCount"], 2);
        assert_eq!(w["runningCount"], 1);
        assert_eq!(w["queuedCount"], 1);
        assert_eq!(w["blockedCount"], 1);
        assert!(w["earliestP50Clock"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(w["waveP50Clock"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(w["waveP75Clock"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(w["blockers"].as_array().unwrap().len(), 1);
        assert_eq!(w["blockers"][0]["compact"], "卡主控");
    }

    #[test]
    fn online_calibration_records_started_accept_delivered_and_applied_idempotently() {
        let state = temp_dir("online-cal");
        let now = 1_800_000_000_i64;
        let repo = "/work/cal-repo";
        let model = populated_model(&state, repo, now);
        speed::write_eta_model_in(&state, &model).unwrap();

        let runs_dir = state.join("runs");
        let run1 = runs_dir.join("run-root");
        let run2 = runs_dir.join("run-reply");
        fs::create_dir_all(&run1).unwrap();
        fs::create_dir_all(&run2).unwrap();

        let meta1 = json!({
            "run": "run-root",
            "mode": "write",
            "class": "build",
            "worktree": {"source": repo},
            "startedEpoch": now as f64,
            "startedAt": format_epoch(now, false, "%Y-%m-%dT%H:%M:%SZ"),
        });
        write_json(run1.join("meta.json"), &meta1).unwrap();

        // 1) Started prediction recorded once; second call is idempotent.
        record_pred_started_in(&state, &run1, &meta1, now);
        let pred_after_start = json(run1.join(ETA_PRED_FILE));
        assert!(pred_after_start["started"]["toDeliveredP50Sec"].as_f64().unwrap() > 0.0);
        assert!(pred_after_start["started"]["toAppliedP50Sec"].as_f64().unwrap() > 0.0);
        let first_started_at = pred_after_start["started"]["at"].clone();
        record_pred_started_in(&state, &run1, &meta1, now + 999);
        assert_eq!(json(run1.join(ETA_PRED_FILE))["started"]["at"], first_started_at);

        // 2) First entry to accept/review recorded once.
        record_pred_accept_review_in(&state, &run1, &meta1, now + 600);
        let pred_after_ar = json(run1.join(ETA_PRED_FILE));
        assert!(pred_after_ar["acceptReview"]["toDeliveredP50Sec"].as_f64().unwrap() > 0.0);
        record_pred_accept_review_in(&state, &run1, &meta1, now + 900);
        assert_eq!(json(run1.join(ETA_PRED_FILE))["acceptReview"]["epoch"], now + 600);

        // 3) Delivered appends both started->delivered and accept_review->delivered, idempotently.
        let summary1 = json!({
            "state": "delivered",
            "finishedAt": format_epoch(now + 850, false, "%Y-%m-%dT%H:%M:%SZ"),
        });
        record_calibration_delivered_in(&state, &run1, &meta1, &summary1, now + 850);
        record_calibration_delivered_in(&state, &run1, &meta1, &summary1, now + 850);

        // 4) Reply run applied walks back to chain root and records started->applied once.
        let meta2 = json!({
            "run": "run-reply",
            "parent": "run-root",
            "mode": "write",
            "class": "build",
            "worktree": {"source": repo},
        });
        write_json(run2.join("meta.json"), &meta2).unwrap();
        let app_iso = format_epoch(now + 1200, false, "%Y-%m-%dT%H:%M:%SZ");
        record_calibration_applied_in(&state, &run2, &meta2, &app_iso, now + 1200);
        record_calibration_applied_in(&state, &run2, &meta2, &app_iso, now + 1200);

        let cal_raw = fs::read_to_string(speed::eta_calibration_path(&state)).unwrap();
        let rows: Vec<Value> = cal_raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 3, "expected started->delivered, accept_review->delivered, started->applied: {rows:?}");
        assert_eq!(rows[0]["checkpoint"], "started");
        assert_eq!(rows[0]["milestone"], "delivered");
        assert_eq!(rows[0]["actualSec"], 850.0);
        assert_eq!(rows[1]["checkpoint"], "accept_review");
        assert_eq!(rows[1]["milestone"], "delivered");
        assert_eq!(rows[1]["actualSec"], 250.0);
        assert_eq!(rows[2]["checkpoint"], "started");
        assert_eq!(rows[2]["milestone"], "applied");
        assert_eq!(rows[2]["run"], "run-root");
        assert_eq!(rows[2]["actualSec"], 1200.0);

        // Deleting derived eta-pred.json never crashes delivered/applied recording.
        let _ = fs::remove_file(run1.join(ETA_PRED_FILE));
        record_calibration_delivered_in(&state, &run1, &meta1, &summary1, now + 900);
        record_calibration_applied_in(&state, &run1, &meta1, &app_iso, now + 1300);

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn wait_tail_eta_combines_active_wave_and_work_chain() {
        let state = temp_dir("wait_tail");
        let now = 1_800_000_000_i64;
        let repo = "/work/wrepo";
        let model = populated_model(&state, repo, now);

        let run_a = state.join("runs/r-a");
        let run_b = state.join("runs/r-b");
        fs::create_dir_all(&run_a).unwrap();
        fs::create_dir_all(&run_b).unwrap();
        let started_a = format_epoch(now - 900, false, "%Y-%m-%dT%H:%M:%SZ");
        let started_b = format_epoch(now - 60, false, "%Y-%m-%dT%H:%M:%SZ");
        write_json(
            run_a.join("meta.json"),
            &json!({"run": "r-a", "name": "feat-a", "mode": "write", "class": "build", "top": repo, "startedAt": started_a}),
        )
        .unwrap();
        write_json(
            run_b.join("meta.json"),
            &json!({"run": "r-b", "name": "feat-b", "mode": "write", "class": "build", "top": repo, "startedAt": started_b}),
        )
        .unwrap();
        fs::write(run_a.join("supervisor.pid"), std::process::id().to_string()).unwrap();
        fs::write(run_b.join("supervisor.pid"), std::process::id().to_string()).unwrap();

        let w1 = WorkItem {
            id: "W012".into(),
            repo: repo.into(),
            title: "链1".into(),
            state: "active".into(),
            class: "feat".into(),
            runs: vec!["r-a".into()],
            run_dirs: BTreeMap::from([("r-a".into(), run_a.to_string_lossy().into_owned())]),
            ..Default::default()
        };
        let w2 = WorkItem {
            id: "W015".into(),
            repo: repo.into(),
            title: "链2".into(),
            state: "blocked".into(),
            class: "feat".into(),
            wait_on: vec!["work_applied:W012".into()],
            ..Default::default()
        };
        let items = vec![w1, w2];

        let mut run_dir_index = HashMap::new();
        run_dir_index.insert("r-a".into(), run_a.clone());
        run_dir_index.insert("r-b".into(), run_b.clone());
        let ctx = LiveContext {
            now_ts: now,
            run_dir_index,
            ..Default::default()
        };

        let line = format_wait_tail_eta(
            &[run_a.clone(), run_b.clone()],
            &HashSet::new(),
            &items,
            Some(repo),
            &model,
            &ctx,
        )
        .expect("should produce tail ETA line");
        assert!(line.contains("其余 2 条在跑：最早 "), "{line}");
        assert!(line.contains("（feat-a）"), "{line}");
        assert!(line.contains("本波约 "), "{line}");
        assert!(line.contains("；链 [W012→W015] 剩 2 步，约 "), "{line}");

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn deferred_items_do_not_drag_dag_eta_and_are_summarized_separately() {
        let state = temp_dir("deferred-eta-sep");
        let now = 1_795_634_100_i64; // Fixed clock: 2026-11-25T19:15:00Z (2026-11-26 03:15+08:00)
        let repo = "/work/demo";
        let model = populated_model(&state, repo, now);
        let ctx = LiveContext {
            now_ts: now,
            ..Default::default()
        };

        let items = vec![
            WorkItem {
                id: "W119".into(),
                title: "推进链第一步".into(),
                repo: repo.into(),
                state: "ready".into(),
                class: "feat".into(),
                ..Default::default()
            },
            WorkItem {
                id: "W120".into(),
                title: "推进链第二步".into(),
                repo: repo.into(),
                state: "blocked".into(),
                class: "feat".into(),
                wait_on: vec!["work_closed:W119".into()],
                ..Default::default()
            },
            WorkItem {
                id: "W006".into(),
                title: "暂缓到五天后".into(),
                repo: repo.into(),
                state: "deferred".into(),
                class: "feat".into(),
                defer_until: format_epoch(now + 5 * 86_400, false, "%Y-%m-%dT%H:%M:%SZ"),
                ..Default::default()
            },
            WorkItem {
                id: "W064".into(),
                title: "暂缓到六小时后".into(),
                repo: repo.into(),
                state: "deferred".into(),
                class: "feat".into(),
                defer_until: format_epoch(now + 6 * 3600, false, "%Y-%m-%dT%H:%M:%SZ"),
                ..Default::default()
            },
        ];

        let dag = estimate_work_dag_in(&items, None, &model, &ctx);
        assert_eq!(dag.write_chains, 2, "only advancing W119+W120 should count");
        assert_eq!(dag.depth, 2);
        assert_eq!(dag.deferred_count, 2);
        assert_eq!(dag.earliest_deferred_at, Some(now + 6 * 3600));
        let fin50 = dag.earliest_finish_p50_at.expect("advancing finish");
        assert!(
            fin50 < now + 6 * 3600,
            "active DAG ETA must not be dragged to deferred date, delta={}s",
            fin50 - now
        );
        let earliest_clk = format_local_short(now + 6 * 3600, now);
        assert!(
            dag.headline.contains(&format!("另有 2 项暂缓（最早 {earliest_clk} 恢复）")),
            "headline missing separate deferred summary: {}",
            dag.headline
        );
        // Individual deferred item still keeps its own post-defer ETA for `delegate eta W006`.
        let w6 = dag.items.iter().find(|it| it.id == "W006").expect("W006 item");
        assert!(w6.remaining.as_ref().expect("W006 rem").p50_at >= now + 5 * 86_400);

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn wait_tail_eta_uses_chain_own_eta_not_global_or_deferred_max() {
        let state = temp_dir("wait-tail-chain-own");
        let now = 1_795_634_100_i64;
        let repo = "/work/demo";
        let model = populated_model(&state, repo, now);
        let ctx = LiveContext {
            now_ts: now,
            ..Default::default()
        };

        // Chain W119 -> W120 plus 8 unrelated ready items (so global throughput bound > W120)
        // plus a deferred item 5 days in the future.
        let mut items = vec![
            WorkItem {
                id: "W119".into(),
                title: "链头".into(),
                repo: repo.into(),
                state: "ready".into(),
                class: "feat".into(),
                ..Default::default()
            },
            WorkItem {
                id: "W120".into(),
                title: "链尾".into(),
                repo: repo.into(),
                state: "blocked".into(),
                class: "feat".into(),
                wait_on: vec!["work_closed:W119".into()],
                ..Default::default()
            },
            WorkItem {
                id: "W006".into(),
                title: "五天后暂缓".into(),
                repo: repo.into(),
                state: "deferred".into(),
                class: "feat".into(),
                defer_until: format_epoch(now + 5 * 86_400, false, "%Y-%m-%dT%H:%M:%SZ"),
                ..Default::default()
            },
        ];
        for i in 1..=8 {
            items.push(WorkItem {
                id: format!("W20{i}"),
                title: format!("并行就绪 {i}"),
                repo: repo.into(),
                state: "ready".into(),
                class: "feat".into(),
                ..Default::default()
            });
        }

        let dag = estimate_work_dag_in(&items, Some(repo), &model, &ctx);
        let w120_eta = dag
            .items
            .iter()
            .find(|it| it.id == "W120")
            .and_then(|it| it.remaining.as_ref())
            .expect("W120 own eta");
        let expected_w120_clock = format_clock(w120_eta.rounded_p50_at, now);

        let tail = format_wait_tail_eta(&[], &HashSet::new(), &items, Some(repo), &model, &ctx)
            .expect("tail line");
        let expected_seg = format!("链 [W119→W120] 剩 2 步，约 {expected_w120_clock} 合入");
        assert!(
            tail.contains(&expected_seg),
            "wait tail must report chain W119->W120's own ETA ({expected_seg}), got: {tail}"
        );

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn downstream_items_waiting_on_same_run_share_upstream_name_and_eta() {
        let state = temp_dir("shared-upstream-run");
        let now = 1_795_634_100_i64;
        let repo_a = "/work/norvane";
        let repo_b = "/work/physics-rl";
        let model = populated_model(&state, repo_a, now);

        let up_id = "20261125-215334-阶段-2---T3.1";
        let up_dir = state.join(up_id);
        fs::create_dir_all(&up_dir).unwrap();
        write_json(
            up_dir.join("meta.json"),
            &json!({
                "run": up_id,
                "name": "阶段 2 · T3.1",
                "mode": "write",
                "class": "build",
                "workdir": repo_b,
                "startedAt": format_epoch(now - 600, false, "%Y-%m-%dT%H:%M:%SZ"),
            }),
        )
        .unwrap();
        fs::write(up_dir.join("supervisor.pid"), std::process::id().to_string()).unwrap();

        let items = vec![
            WorkItem {
                id: "W110".into(),
                title: "上游在 physics-rl，本项在 norvane".into(),
                repo: repo_a.into(),
                state: "blocked".into(),
                class: "hard".into(),
                wait_on: vec![format!("run_applied:{up_id}")],
                ..Default::default()
            },
            WorkItem {
                id: "W123".into(),
                title: "同上游另一项".into(),
                repo: repo_b.into(),
                state: "blocked".into(),
                class: "feat".into(),
                wait_on: vec![format!("run_applied:{up_id}")],
                ..Default::default()
            },
        ];

        let mut run_dir_index = HashMap::new();
        run_dir_index.insert(up_id.to_string(), up_dir.clone());
        let ctx = LiveContext {
            now_ts: now,
            run_dir_index,
            ..Default::default()
        };

        let dag = estimate_work_dag_in(&items, None, &model, &ctx);
        let e110 = dag.items.iter().find(|it| it.id == "W110").unwrap();
        let e123 = dag.items.iter().find(|it| it.id == "W123").unwrap();
        assert_eq!(
            e110.focus_badge, e123.focus_badge,
            "siblings waiting on same upstream run must display identical upstream ETA"
        );
        assert!(e110.focus_badge.contains("等 阶段 2 · T3.1"), "{}", e110.focus_badge);
        assert!(!e110.focus_badge.contains("20261125-215334"), "{}", e110.focus_badge);

        let _ = fs::remove_dir_all(&state);
    }
}
