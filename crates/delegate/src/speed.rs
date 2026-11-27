//! Calibrate wall-clock estimation parameters (`T_dev = max(D·t_crit, N·t_chain/P_eff) + T_merge`)
//! from the machine-wide event stream (`<state_dir>/events.jsonl`), grouped by repository,
//! persisting each snapshot to `<state_dir>/speed/<repo-slug>.jsonl` with ±20% drift detection.
use crate::common::*;
use crate::events;
use crate::review;
use crate::{has, parse_simple, value};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

const MIN_WRITE_SAMPLES: usize = 30;
const EXPAND_WINDOW_DAYS: i64 = 14;
const EXPAND_WINDOW_SECS: i64 = EXPAND_WINDOW_DAYS * 86_400;
const DRIFT_ALERT_PCT: f64 = 20.0;
const MAX_OPEN_RUN_SECS: i64 = 4 * 3600;

pub const ETA_MODEL_VERSION: u32 = 1;
pub const ETA_MODEL_FILE: &str = "eta-model.json";
pub const ETA_CALIBRATION_FILE: &str = "eta-calibration.jsonl";
pub const ETA_MODEL_WINDOW_DAYS: i64 = 14;
#[allow(dead_code)] // Consumed by eta.rs in Chain 2.
pub const ETA_MODEL_STALE_SECS: i64 = 15 * 60;
pub const MIN_BUCKET_N: usize = 10;
pub const MIN_GLOBAL_MODE_N: usize = 5;
const MAX_STORED_SAMPLES: usize = 256;
const RECENT_LANE_WINDOW_SECS: i64 = 2 * 3600;

#[derive(Debug, Clone)]
pub struct EventRec {
    pub seq: u64,
    pub ts: i64,
    pub run: String,
    pub repo: String,
    pub kind: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Default)]
struct RunTrack {
    started_ts: Option<i64>,
    finished_ts: Option<i64>,
    applied_ts: Option<i64>,
}

#[derive(Debug, Clone)]
struct ChainInfo {
    is_write: bool,
    has_started: bool,
    class: String,
    tier: String,
    first_started_ts: Option<i64>,
    first_applied_ts: Option<i64>,
    last_delivered_ts: Option<i64>,
    last_finished_ts: Option<i64>,
    started_runs: HashSet<String>,
    rework_sent_count: usize,
    has_premerge_conflict: bool,
    has_premerge_failed: bool,
    ready_timestamps: Vec<i64>,
    runs: HashMap<String, RunTrack>,
}

impl ChainInfo {
    fn new() -> Self {
        Self {
            is_write: false,
            has_started: false,
            class: String::new(),
            tier: String::new(),
            first_started_ts: None,
            first_applied_ts: None,
            last_delivered_ts: None,
            last_finished_ts: None,
            started_runs: HashSet::new(),
            rework_sent_count: 0,
            has_premerge_conflict: false,
            has_premerge_failed: false,
            ready_timestamps: Vec::new(),
            runs: HashMap::new(),
        }
    }

    fn mode_and_class(&self) -> (String, String) {
        let mode = if self.is_write { "write" } else { "read-only" };
        normalize_mode_class(mode, &self.class, &self.tier)
    }

    fn delivered_secs(&self) -> Option<f64> {
        let start = self.first_started_ts?;
        let del = self.last_delivered_ts?;
        Some((del - start).max(0) as f64)
    }

    fn total_rounds(&self) -> usize {
        self.started_runs.len().max(1) + self.rework_sent_count
    }

    fn is_non_first_pass(&self) -> bool {
        self.started_runs.len() > 1 || self.rework_sent_count > 0
    }

    fn is_three_plus_rounds(&self) -> bool {
        self.total_rounds() >= 3
    }

    fn wall_clock_secs(&self) -> Option<f64> {
        let start = self.first_started_ts?;
        let applied = self.first_applied_ts?;
        Some((applied - start).max(0) as f64)
    }

    fn read_only_secs(&self) -> Option<f64> {
        let start = self.first_started_ts?;
        let fin = self.last_finished_ts?;
        Some((fin - start).max(0) as f64)
    }

    fn merge_wait_secs(&self) -> Option<f64> {
        let applied = self.first_applied_ts?;
        let ready = self
            .ready_timestamps
            .iter()
            .copied()
            .filter(|&ts| ts <= applied)
            .max()?;
        Some((applied - ready).max(0) as f64)
    }

    /// Merged half-open minute intervals `[start_min, end_min_exclusive)` during which at least one
    /// run in this chain was actively executing.
    fn active_minute_intervals(&self, now_ts: i64) -> Vec<(i64, i64)> {
        let mut raw: Vec<(i64, i64)> = Vec::new();
        for rt in self.runs.values() {
            let Some(st) = rt.started_ts else {
                continue;
            };
            let end = rt
                .finished_ts
                .or(rt.applied_ts)
                .unwrap_or_else(|| now_ts.min(st + MAX_OPEN_RUN_SECS))
                .max(st);
            let m0 = st.div_euclid(60);
            let m1 = end.div_euclid(60) + 1;
            raw.push((m0, m1));
        }
        if raw.is_empty() {
            return raw;
        }
        raw.sort_unstable();
        let mut merged: Vec<(i64, i64)> = Vec::with_capacity(raw.len());
        for (a, b) in raw {
            if let Some(last) = merged.last_mut() {
                if a <= last.1 {
                    last.1 = last.1.max(b);
                    continue;
                }
            }
            merged.push((a, b));
        }
        merged
    }
}

#[derive(Debug, Clone, Default)]
pub struct DistStats {
    pub count: usize,
    pub p50_sec: Option<f64>,
    pub p75_sec: Option<f64>,
    pub mean_sec: Option<f64>,
}

impl DistStats {
    fn from_secs(mut vals: Vec<f64>) -> Self {
        if vals.is_empty() {
            return Self::default();
        }
        vals.sort_by(|a, b| a.total_cmp(b));
        let sum: f64 = vals.iter().sum();
        Self {
            count: vals.len(),
            p50_sec: percentile(&vals, 0.50),
            p75_sec: percentile(&vals, 0.75),
            mean_sec: Some(sum / vals.len() as f64),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "count": self.count,
            "p50Min": self.p50_sec.map(sec_to_min),
            "p75Min": self.p75_sec.map(sec_to_min),
            "meanMin": self.mean_sec.map(sec_to_min),
            "p50Sec": self.p50_sec.map(round2),
            "p75Sec": self.p75_sec.map(round2),
            "meanSec": self.mean_sec.map(round2),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConcurrencyStats {
    pub active_minutes: i64,
    pub mean: Option<f64>,
    pub p50: Option<f64>,
    pub p75: Option<f64>,
    pub applied_per_active_hour: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct DriftMetric {
    pub prev: f64,
    pub curr: f64,
    pub pct: f64,
    pub alert: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DriftReport {
    pub previous_at: Option<String>,
    pub metrics: BTreeMap<String, DriftMetric>,
    pub any_alert: bool,
}

#[derive(Debug, Clone)]
pub struct RepoSpeedReport {
    pub at: String,
    pub repo: String,
    pub repo_name: String,
    pub repo_slug: String,
    pub requested_since: String,
    pub effective_since: String,
    pub expanded: bool,
    pub initial_write_chains: Option<usize>,
    pub write_chains_total: usize,
    pub write_wall: DistStats,
    pub t_crit: DistStats,
    pub first_pass: DistStats,
    pub auto_rework_count: usize,
    pub auto_rework_rate: Option<f64>,
    pub non_first_pass_count: usize,
    pub non_first_pass_rate: Option<f64>,
    pub rounds_3plus_count: usize,
    pub rounds_3plus_rate: Option<f64>,
    pub concurrency: ConcurrencyStats,
    pub premerge_conflict_count: usize,
    pub premerge_failed_count: usize,
    pub premerge_issue_count: usize,
    pub premerge_issue_rate: Option<f64>,
    pub merge_wait: DistStats,
    pub read_only: DistStats,
    pub drift: DriftReport,
    pub eta_calibration: Option<RepoCalibrationReport>,
}

impl RepoSpeedReport {
    pub fn t_chain_min(&self) -> Option<f64> {
        self.write_wall.p50_sec.map(sec_to_min)
    }

    pub fn t_crit_min(&self) -> Option<f64> {
        self.t_crit.p50_sec.map(sec_to_min)
    }

    pub fn t_crit_p75_min(&self) -> Option<f64> {
        self.t_crit.p75_sec.map(sec_to_min)
    }

    pub fn p_eff(&self) -> Option<f64> {
        self.concurrency.mean.map(round2)
    }

    pub fn t_merge_wait_p75_min(&self) -> Option<f64> {
        self.merge_wait.p75_sec.map(sec_to_min)
    }

    pub fn to_snapshot_json(&self) -> Value {
        json!({
            "at": self.at,
            "repo": self.repo,
            "repoName": self.repo_name,
            "repoSlug": self.repo_slug,
            "window": {
                "requested": self.requested_since,
                "effective": self.effective_since,
                "expanded": self.expanded,
                "initialWriteChains": self.initial_write_chains,
            },
            "writeChains": {
                "total": self.write_chains_total,
                "applied": self.write_wall.count,
                "p50Min": self.write_wall.p50_sec.map(sec_to_min),
                "p75Min": self.write_wall.p75_sec.map(sec_to_min),
                "meanMin": self.write_wall.mean_sec.map(sec_to_min),
                "p50Sec": self.write_wall.p50_sec.map(round2),
                "p75Sec": self.write_wall.p75_sec.map(round2),
                "meanSec": self.write_wall.mean_sec.map(round2),
            },
            "tCrit": self.t_crit.to_json(),
            "firstPass": self.first_pass.to_json(),
            "rework": {
                "autoReworkCount": self.auto_rework_count,
                "autoReworkRate": self.auto_rework_rate.map(round4),
                "nonFirstPassCount": self.non_first_pass_count,
                "nonFirstPassRate": self.non_first_pass_rate.map(round4),
                "rounds3PlusCount": self.rounds_3plus_count,
                "rounds3PlusRate": self.rounds_3plus_rate.map(round4),
            },
            "concurrency": {
                "activeMinutes": self.concurrency.active_minutes,
                "pEffMean": self.concurrency.mean.map(round2),
                "pEffP50": self.concurrency.p50.map(round2),
                "pEffP75": self.concurrency.p75.map(round2),
                "appliedPerActiveHour": self.concurrency.applied_per_active_hour.map(round2),
            },
            "merge": {
                "premergeConflictCount": self.premerge_conflict_count,
                "premergeFailedCount": self.premerge_failed_count,
                "premergeIssueCount": self.premerge_issue_count,
                "premergeIssueRate": self.premerge_issue_rate.map(round4),
                "wait": self.merge_wait.to_json(),
            },
            "readOnly": self.read_only.to_json(),
            "formula": {
                "tChainMin": self.t_chain_min(),
                "tCritMin": self.t_crit_min(),
                "tCritP75Min": self.t_crit_p75_min(),
                "pEff": self.p_eff(),
                "tMergeWaitP75Min": self.t_merge_wait_p75_min(),
                "tMergeIssueRate": self.premerge_issue_rate.map(round4),
            },
        })
    }

    pub fn to_full_json(&self) -> Value {
        let mut doc = self.to_snapshot_json();
        let mut drift_metrics = serde_json::Map::new();
        for (k, m) in &self.drift.metrics {
            drift_metrics.insert(
                k.clone(),
                json!({
                    "prev": round2(m.prev),
                    "curr": round2(m.curr),
                    "pct": round2(m.pct),
                    "alert": m.alert,
                }),
            );
        }
        doc["drift"] = json!({
            "previousAt": self.drift.previous_at,
            "anyAlert": self.drift.any_alert,
            "metrics": drift_metrics,
        });
        if let Some(cal) = &self.eta_calibration {
            doc["etaCalibration"] = cal.to_json();
        }
        doc
    }
}

pub(crate) fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

pub(crate) fn sec_to_min(sec: f64) -> f64 {
    round2(sec / 60.0)
}

pub(crate) fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    if sorted.len() == 1 {
        return Some(sorted[0]);
    }
    let pos = p.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let frac = pos - lo as f64;
    Some(sorted[lo] + frac * (sorted[hi] - sorted[lo]))
}

fn weighted_percentile(buckets: &[(u32, i64)], p: f64) -> Option<f64> {
    let total: i64 = buckets.iter().map(|(_, w)| *w).sum();
    if total <= 0 {
        return None;
    }
    let mut sorted = buckets.to_vec();
    sorted.sort_unstable_by_key(|(v, _)| *v);
    if total == 1 {
        return Some(sorted[0].0 as f64);
    }
    let pos = p.clamp(0.0, 1.0) * (total - 1) as f64;
    let lo_idx = pos.floor() as i64;
    let hi_idx = pos.ceil() as i64;
    let frac = pos - lo_idx as f64;

    let val_at = |idx: i64| -> f64 {
        let mut acc: i64 = 0;
        for &(val, w) in &sorted {
            acc += w;
            if acc > idx {
                return val as f64;
            }
        }
        sorted.last().map(|(v, _)| *v as f64).unwrap_or(0.0)
    };

    let lo_v = val_at(lo_idx);
    let hi_v = val_at(hi_idx);
    Some(lo_v + frac * (hi_v - lo_v))
}

pub(crate) fn short_repo_name(repo: &str) -> String {
    Path::new(repo)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(repo)
        .to_string()
}

pub(crate) fn repo_matches_filter(repo_path: &str, repo_name: &str, filter: &str) -> bool {
    let f = filter.trim();
    if f.is_empty() {
        return true;
    }
    if repo_name == f || repo_path == f {
        return true;
    }
    if let Ok(canon) = fs::canonicalize(Path::new(f)) {
        if canon.to_string_lossy() == repo_path {
            return true;
        }
    }
    false
}

fn speed_dir(state: &Path) -> PathBuf {
    state.join("speed")
}

/// Resolve `<state>/speed/<repo-slug>.jsonl`: sanitized repo basename when unambiguous,
/// or `<basename>-<8hex>` if an existing file belongs to a different repository path.
fn resolve_speed_file(state: &Path, repo: &str) -> (String, PathBuf) {
    let dir = speed_dir(state);
    let base = slug(&short_repo_name(repo));
    let base = if base.is_empty() {
        "repo".to_string()
    } else {
        base
    };
    let direct = dir.join(format!("{base}.jsonl"));
    if let Some(last) = read_last_json_line(&direct) {
        let existing_repo = s(&last, "repo");
        if !existing_repo.is_empty() && existing_repo != repo {
            let hash = review::sha256_hex(repo.as_bytes());
            let disamb = format!("{base}-{}", &hash[..8]);
            return (disamb.clone(), dir.join(format!("{disamb}.jsonl")));
        }
    }
    (base, direct)
}

fn read_last_json_line(path: &Path) -> Option<Value> {
    let f = OpenOptions::new().read(true).open(path).ok()?;
    let reader = BufReader::new(f);
    let mut last: Option<Value> = None;
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            if v.is_object() {
                last = Some(v);
            }
        }
    }
    last
}

pub fn parse_events(raw: Vec<Value>) -> Vec<EventRec> {
    let mut out = Vec::with_capacity(raw.len());
    for v in raw {
        let Some(ts) = iso_epoch(s(&v, "at")) else {
            continue;
        };
        let repo = s(&v, "repo").trim().to_string();
        let run = s(&v, "run").trim().to_string();
        let kind = s(&v, "kind").trim().to_string();
        if repo.is_empty() || run.is_empty() || kind.is_empty() {
            continue;
        }
        out.push(EventRec {
            seq: v["seq"].as_u64().unwrap_or(0),
            ts,
            run,
            repo,
            kind,
            payload: v["payload"].clone(),
        });
    }
    out
}

fn resolve_root<'a>(run: &'a str, parent_of: &'a HashMap<String, String>) -> &'a str {
    let mut cur = run;
    let mut seen: HashSet<&str> = HashSet::new();
    seen.insert(cur);
    while let Some(p) = parent_of.get(cur) {
        let p_str = p.as_str();
        if p_str.is_empty() || !seen.insert(p_str) {
            break;
        }
        cur = p_str;
    }
    cur
}

fn is_terminal_kind(kind: &str) -> bool {
    matches!(
        kind,
        "delivered" | "rejected" | "failed" | "stopped" | "question"
    )
}

fn build_chains_for_repo(events: &[EventRec]) -> HashMap<String, ChainInfo> {
    // Pass 1: build run -> parent map from all started events.
    let mut parent_of: HashMap<String, String> = HashMap::new();
    for ev in events {
        if ev.kind == "started" {
            let p = s(&ev.payload, "parent").trim();
            if !p.is_empty() && p != ev.run {
                parent_of.insert(ev.run.clone(), p.to_string());
            }
        }
    }

    // Pass 2: aggregate events into root chains.
    let mut chains: HashMap<String, ChainInfo> = HashMap::new();
    for ev in events {
        let root = resolve_root(&ev.run, &parent_of).to_string();
        let chain = chains.entry(root).or_insert_with(ChainInfo::new);
        let rt = chain.runs.entry(ev.run.clone()).or_default();

        match ev.kind.as_str() {
            "started" => {
                chain.has_started = true;
                let mode = s(&ev.payload, "mode");
                if mode == "write" {
                    chain.is_write = true;
                }
                let cls = s(&ev.payload, "class").trim();
                if chain.class.is_empty() && !cls.is_empty() {
                    chain.class = cls.to_string();
                }
                let tier = s(&ev.payload, "tier").trim();
                if chain.tier.is_empty() && !tier.is_empty() {
                    chain.tier = tier.to_string();
                }
                chain.started_runs.insert(ev.run.clone());
                chain.first_started_ts = Some(
                    chain
                        .first_started_ts
                        .map_or(ev.ts, |prev| prev.min(ev.ts)),
                );
                rt.started_ts = Some(rt.started_ts.map_or(ev.ts, |prev| prev.min(ev.ts)));
            }
            "applied" => {
                chain.first_applied_ts = Some(
                    chain
                        .first_applied_ts
                        .map_or(ev.ts, |prev| prev.min(ev.ts)),
                );
                rt.applied_ts = Some(rt.applied_ts.map_or(ev.ts, |prev| prev.min(ev.ts)));
            }
            "rework_sent" => {
                chain.rework_sent_count += 1;
            }
            "premerge_conflict" => {
                chain.has_premerge_conflict = true;
            }
            "premerge_failed" => {
                chain.has_premerge_failed = true;
            }
            "review_verdict" => {
                chain.ready_timestamps.push(ev.ts);
            }
            kind if is_terminal_kind(kind) => {
                if kind == "delivered" {
                    chain.ready_timestamps.push(ev.ts);
                    chain.last_delivered_ts = Some(
                        chain
                            .last_delivered_ts
                            .map_or(ev.ts, |prev| prev.max(ev.ts)),
                    );
                }
                chain.last_finished_ts = Some(
                    chain
                        .last_finished_ts
                        .map_or(ev.ts, |prev| prev.max(ev.ts)),
                );
                rt.finished_ts = Some(rt.finished_ts.map_or(ev.ts, |prev| prev.max(ev.ts)));
            }
            _ => {}
        }
    }

    chains
}

fn compute_concurrency(
    write_chains: &[&ChainInfo],
    applied_write_chains: usize,
    now_ts: i64,
) -> ConcurrencyStats {
    let mut diff: BTreeMap<i64, i32> = BTreeMap::new();
    for ch in write_chains {
        for (m0, m1) in ch.active_minute_intervals(now_ts) {
            if m1 > m0 {
                *diff.entry(m0).or_insert(0) += 1;
                *diff.entry(m1).or_insert(0) -= 1;
            }
        }
    }

    let mut cur: i32 = 0;
    let mut prev_m: Option<i64> = None;
    let mut active_minutes: i64 = 0;
    let mut weighted_sum: f64 = 0.0;
    let mut buckets: Vec<(u32, i64)> = Vec::new();

    for (m, delta) in diff {
        if let Some(pm) = prev_m {
            let span = m - pm;
            if span > 0 && cur > 0 {
                active_minutes += span;
                weighted_sum += (cur as f64) * (span as f64);
                buckets.push((cur as u32, span));
            }
        }
        cur += delta;
        prev_m = Some(m);
    }

    if active_minutes <= 0 {
        return ConcurrencyStats::default();
    }

    ConcurrencyStats {
        active_minutes,
        mean: Some(weighted_sum / active_minutes as f64),
        p50: weighted_percentile(&buckets, 0.50),
        p75: weighted_percentile(&buckets, 0.75),
        applied_per_active_hour: Some(applied_write_chains as f64 / (active_minutes as f64 / 60.0)),
    }
}

struct WindowSpec<'a> {
    cutoff_ts: Option<i64>,
    now_ts: i64,
    at_iso: &'a str,
    requested_since: &'a str,
    effective_since: &'a str,
    expanded: bool,
    initial_write_chains: Option<usize>,
}

fn compute_repo_window(
    repo: &str,
    repo_slug: &str,
    chains: &HashMap<String, ChainInfo>,
    win: &WindowSpec<'_>,
) -> RepoSpeedReport {
    let mut write_chains: Vec<&ChainInfo> = Vec::new();
    let mut read_only_secs: Vec<f64> = Vec::new();

    for ch in chains.values() {
        if !ch.has_started {
            continue;
        }
        let Some(st) = ch.first_started_ts else {
            continue;
        };
        if let Some(cut) = win.cutoff_ts {
            if st < cut {
                continue;
            }
        }
        if ch.is_write {
            write_chains.push(ch);
        } else if let Some(dur) = ch.read_only_secs() {
            read_only_secs.push(dur);
        }
    }

    let write_chains_total = write_chains.len();
    let mut all_write_secs: Vec<f64> = Vec::new();
    let mut non_first_pass_secs: Vec<f64> = Vec::new();
    let mut first_pass_secs: Vec<f64> = Vec::new();
    let mut merge_wait_secs: Vec<f64> = Vec::new();

    let mut auto_rework_count: usize = 0;
    let mut non_first_pass_count: usize = 0;
    let mut rounds_3plus_count: usize = 0;
    let mut premerge_conflict_count: usize = 0;
    let mut premerge_failed_count: usize = 0;
    let mut premerge_issue_count: usize = 0;

    for ch in &write_chains {
        if ch.has_premerge_conflict {
            premerge_conflict_count += 1;
        }
        if ch.has_premerge_failed {
            premerge_failed_count += 1;
        }
        if ch.has_premerge_conflict || ch.has_premerge_failed {
            premerge_issue_count += 1;
        }

        if let Some(wall) = ch.wall_clock_secs() {
            all_write_secs.push(wall);
            if ch.rework_sent_count > 0 {
                auto_rework_count += 1;
            }
            if ch.is_non_first_pass() {
                non_first_pass_count += 1;
                non_first_pass_secs.push(wall);
            } else {
                first_pass_secs.push(wall);
            }
            if ch.is_three_plus_rounds() {
                rounds_3plus_count += 1;
            }
            if let Some(w) = ch.merge_wait_secs() {
                merge_wait_secs.push(w);
            }
        }
    }

    let applied_count = all_write_secs.len();
    let rate = |num: usize, den: usize| -> Option<f64> {
        if den == 0 {
            None
        } else {
            Some(num as f64 / den as f64)
        }
    };

    let concurrency = compute_concurrency(&write_chains, applied_count, win.now_ts);

    RepoSpeedReport {
        at: win.at_iso.to_string(),
        repo: repo.to_string(),
        repo_name: short_repo_name(repo),
        repo_slug: repo_slug.to_string(),
        requested_since: win.requested_since.to_string(),
        effective_since: win.effective_since.to_string(),
        expanded: win.expanded,
        initial_write_chains: win.initial_write_chains,
        write_chains_total,
        write_wall: DistStats::from_secs(all_write_secs),
        t_crit: DistStats::from_secs(non_first_pass_secs),
        first_pass: DistStats::from_secs(first_pass_secs),
        auto_rework_count,
        auto_rework_rate: rate(auto_rework_count, applied_count),
        non_first_pass_count,
        non_first_pass_rate: rate(non_first_pass_count, applied_count),
        rounds_3plus_count,
        rounds_3plus_rate: rate(rounds_3plus_count, applied_count),
        concurrency,
        premerge_conflict_count,
        premerge_failed_count,
        premerge_issue_count,
        premerge_issue_rate: rate(premerge_issue_count, write_chains_total),
        merge_wait: DistStats::from_secs(merge_wait_secs),
        read_only: DistStats::from_secs(read_only_secs),
        drift: DriftReport::default(),
        eta_calibration: None,
    }
}

fn compute_drift(prev_doc: Option<&Value>, report: &RepoSpeedReport) -> DriftReport {
    let Some(prev) = prev_doc else {
        return DriftReport::default();
    };
    let prev_at = {
        let a = s(prev, "at");
        if a.is_empty() {
            None
        } else {
            Some(a.to_string())
        }
    };
    let prev_formula = &prev["formula"];
    let pairs: [(&str, Option<f64>, Option<f64>); 4] = [
        ("t_chain", prev_formula["tChainMin"].as_f64(), report.t_chain_min()),
        ("t_crit", prev_formula["tCritMin"].as_f64(), report.t_crit_min()),
        ("p_eff", prev_formula["pEff"].as_f64(), report.p_eff()),
        (
            "t_merge",
            prev_formula["tMergeWaitP75Min"].as_f64(),
            report.t_merge_wait_p75_min(),
        ),
    ];

    let mut metrics = BTreeMap::new();
    let mut any_alert = false;
    for (name, p_opt, c_opt) in pairs {
        if let (Some(p), Some(c)) = (p_opt, c_opt) {
            if p > 0.0 {
                let pct = (c - p) / p * 100.0;
                let alert = pct.abs() > DRIFT_ALERT_PCT;
                if alert {
                    any_alert = true;
                }
                metrics.insert(
                    name.to_string(),
                    DriftMetric {
                        prev: p,
                        curr: c,
                        pct,
                        alert,
                    },
                );
            }
        }
    }

    DriftReport {
        previous_at: prev_at,
        metrics,
        any_alert,
    }
}

pub fn analyze_in(
    state: &Path,
    now_ts: i64,
    since_str: &str,
    repo_filter: Option<&str>,
    persist: bool,
) -> Res<Vec<RepoSpeedReport>> {
    let (requested_secs, requested_label) = if since_str.eq_ignore_ascii_case("all") {
        (None, "all".to_string())
    } else {
        let secs = seconds(since_str)? as i64;
        if secs <= 0 {
            return Err(format!("--since must be positive, got {since_str:?}"));
        }
        (Some(secs), since_str.to_string())
    };

    let raw = events::read_since_in(state, 0);
    let parsed = parse_events(raw);
    let latest_seq = parsed
        .iter()
        .map(|e| e.seq)
        .max()
        .unwrap_or(0)
        .max(events::latest_seq_in(state));

    if persist {
        let model = build_eta_model_from_events(
            state,
            &parsed,
            now_ts,
            latest_seq,
            ETA_MODEL_WINDOW_DAYS,
        );
        write_eta_model_in(state, &model)?;
    }

    let mut by_repo: BTreeMap<String, Vec<EventRec>> = BTreeMap::new();
    for ev in parsed {
        let name = short_repo_name(&ev.repo);
        if let Some(rf) = repo_filter {
            if !repo_matches_filter(&ev.repo, &name, rf) {
                continue;
            }
        }
        by_repo.entry(ev.repo.clone()).or_default().push(ev);
    }

    let at_iso = format_epoch(now_ts, false, "%Y-%m-%dT%H:%M:%SZ");
    let cal_map = analyze_calibration_by_repo(state, now_ts, requested_secs);
    let mut reports = Vec::new();

    for (repo, repo_events) in by_repo {
        let chains = build_chains_for_repo(&repo_events);
        let (repo_slug, speed_file) = resolve_speed_file(state, &repo);
        let cutoff = requested_secs.map(|d| now_ts - d);

        let mut report = compute_repo_window(
            &repo,
            &repo_slug,
            &chains,
            &WindowSpec {
                cutoff_ts: cutoff,
                now_ts,
                at_iso: &at_iso,
                requested_since: &requested_label,
                effective_since: &requested_label,
                expanded: false,
                initial_write_chains: None,
            },
        );

        // Auto-expand to 14d when requested window < 14d and applied write chains < 30.
        if let Some(req_s) = requested_secs {
            if req_s < EXPAND_WINDOW_SECS && report.write_wall.count < MIN_WRITE_SAMPLES {
                let init_count = report.write_wall.count;
                let expanded_cutoff = now_ts - EXPAND_WINDOW_SECS;
                report = compute_repo_window(
                    &repo,
                    &repo_slug,
                    &chains,
                    &WindowSpec {
                        cutoff_ts: Some(expanded_cutoff),
                        now_ts,
                        at_iso: &at_iso,
                        requested_since: &requested_label,
                        effective_since: "14d",
                        expanded: true,
                        initial_write_chains: Some(init_count),
                    },
                );
            }
        }

        if report.write_chains_total == 0 && report.read_only.count == 0 {
            continue;
        }

        let prev_doc = read_last_json_line(&speed_file);
        report.drift = compute_drift(prev_doc.as_ref(), &report);
        report.eta_calibration = cal_map.get(&repo).cloned();

        if persist {
            fs::create_dir_all(speed_dir(state)).map_err(|e| e.to_string())?;
            let line = format!("{}\n", report.to_snapshot_json());
            append_synced(&speed_file, &line)?;
        }

        reports.push(report);
    }

    reports.sort_by(|a, b| {
        b.write_wall
            .count
            .cmp(&a.write_wall.count)
            .then_with(|| b.write_chains_total.cmp(&a.write_chains_total))
            .then_with(|| a.repo_name.cmp(&b.repo_name))
    });

    Ok(reports)
}

fn fmt_min(v: Option<f64>) -> String {
    match v {
        Some(m) => format!("{m:.1}m"),
        None => "-".to_string(),
    }
}

fn fmt_min_pair(a: Option<f64>, b: Option<f64>) -> String {
    match (a, b) {
        (Some(x), Some(y)) => format!("{x:.1}/{y:.1}m"),
        (Some(x), None) => format!("{x:.1}/-m"),
        _ => "-".to_string(),
    }
}

fn fmt_min_triple(a: Option<f64>, b: Option<f64>, c: Option<f64>) -> String {
    match (a, b, c) {
        (Some(x), Some(y), Some(z)) => format!("{x:.1}/{y:.1}/{z:.1}m"),
        _ => "-".to_string(),
    }
}

fn fmt_pct(v: Option<f64>) -> String {
    match v {
        Some(r) => format!("{:.0}%", r * 100.0),
        None => "-".to_string(),
    }
}

fn fmt_f1(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{x:.1}"),
        None => "-".to_string(),
    }
}

pub fn format_human(reports: &[RepoSpeedReport], requested_since: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "delegate speed (请求窗口 {requested_since}，来源 events.jsonl)\n"
    ));

    if reports.is_empty() {
        out.push_str("\n（窗口内无任务链记录）\n");
        return out;
    }

    out.push('\n');
    out.push_str(&format!(
        "{:<16} {:<5} {:<8} {:<18} {:<8} {:<13} {:<15} {:<15} {:<7} {:<14} {:<14} {}\n",
        "仓库",
        "窗口",
        "写链",
        "t_chain 50/75/均",
        "一次过",
        "t_crit 50/75",
        "返工(自/总/3+)",
        "P_eff 均(50/75)",
        "合入/时",
        "T_merge 75(冲)",
        "只读 50/75(n)",
        "ETA p75覆盖"
    ));

    let mut expanded_notes: Vec<String> = Vec::new();
    for r in reports {
        let win = if r.expanded {
            expanded_notes.push(format!(
                "{}（{} 合入写链 {} < {}，已扩至 14d，共 {} 条）",
                r.repo_name,
                r.requested_since,
                r.initial_write_chains.unwrap_or(0),
                MIN_WRITE_SAMPLES,
                r.write_wall.count
            ));
            format!("{}*", r.effective_since)
        } else {
            r.effective_since.clone()
        };

        let write_cnt = format!("{}/{}", r.write_wall.count, r.write_chains_total);
        let t_chain_str = fmt_min_triple(
            r.write_wall.p50_sec.map(sec_to_min),
            r.write_wall.p75_sec.map(sec_to_min),
            r.write_wall.mean_sec.map(sec_to_min),
        );
        let fp_str = fmt_min(r.first_pass.p50_sec.map(sec_to_min));
        let t_crit_str = fmt_min_pair(r.t_crit_min(), r.t_crit_p75_min());
        let rework_str = if r.write_wall.count > 0 {
            format!(
                "{}/{}/{}",
                fmt_pct(r.auto_rework_rate),
                fmt_pct(r.non_first_pass_rate),
                fmt_pct(r.rounds_3plus_rate)
            )
        } else {
            "-".to_string()
        };
        let peff_str = match (r.concurrency.mean, r.concurrency.p50, r.concurrency.p75) {
            (Some(m), Some(p50), Some(p75)) => format!("{m:.1}({p50:.0}/{p75:.0})"),
            _ => "-".to_string(),
        };
        let rate_hr = fmt_f1(r.concurrency.applied_per_active_hour);
        let merge_str = format!(
            "{}({})",
            fmt_min(r.t_merge_wait_p75_min()),
            fmt_pct(r.premerge_issue_rate)
        );
        let ro_str = if r.read_only.count > 0 {
            format!(
                "{}({})",
                fmt_min_pair(
                    r.read_only.p50_sec.map(sec_to_min),
                    r.read_only.p75_sec.map(sec_to_min)
                ),
                r.read_only.count
            )
        } else {
            "-".to_string()
        };

        let cal_col = r
            .eta_calibration
            .as_ref()
            .and_then(|c| c.primary_metrics())
            .and_then(|m| {
                m.p75_coverage.map(|cov| {
                    let alert = if m.p75_alert { "⚠" } else { "" };
                    format!("{:.0}%{alert}({})", cov * 100.0, m.count)
                })
            })
            .unwrap_or_else(|| "-".to_string());

        out.push_str(&format!(
            "{:<16} {:<5} {:<8} {:<18} {:<8} {:<13} {:<15} {:<15} {:<7} {:<14} {:<14} {}\n",
            clip(&r.repo_name, 16),
            win,
            write_cnt,
            t_chain_str,
            fp_str,
            t_crit_str,
            rework_str,
            peff_str,
            rate_hr,
            merge_str,
            ro_str,
            cal_col,
        ));
    }

    if !expanded_notes.is_empty() {
        out.push_str(&format!("\n* 自动扩窗：{}\n", expanded_notes.join("；")));
    }

    out.push_str("\n公式 T_dev = max(D·t_crit, N·t_chain/P_eff) + T_merge\n");
    for r in reports {
        out.push_str(&format!(
            "  {}: t_chain={}  t_crit={} (p75 {})  P_eff={}  T_merge={}/链 (预合并冲突/失败 {})\n",
            r.repo_name,
            fmt_min(r.t_chain_min()),
            fmt_min(r.t_crit_min()),
            fmt_min(r.t_crit_p75_min()),
            fmt_f1(r.p_eff()),
            fmt_min(r.t_merge_wait_p75_min()),
            fmt_pct(r.premerge_issue_rate),
        ));
        if !r.drift.metrics.is_empty() {
            let prev_date = r
                .drift
                .previous_at
                .as_deref()
                .map(|s| s.get(..10).unwrap_or(s))
                .unwrap_or("-");
            let parts: Vec<String> = r
                .drift
                .metrics
                .iter()
                .map(|(k, m)| {
                    let sign = if m.pct >= 0.0 { "+" } else { "" };
                    let mark = if m.alert { " ⚠" } else { "" };
                    format!("{k} {sign}{:.1}%{mark}", m.pct)
                })
                .collect();
            out.push_str(&format!(
                "    漂移(较 {prev_date}): {}\n",
                parts.join("  ")
            ));
        }
        if let Some(cal) = &r.eta_calibration {
            if let Some(m) = cal.primary_metrics() {
                out.push_str(&format!("    ETA 在线覆盖：{}\n", fmt_bt_metric_brief(m)));
            }
        }
    }

    out
}

/// CLI entrypoint: `delegate speed [--since 7d|14d|all] [--repo PATH] [--backtest] [--json]`
pub fn command(args: &[String]) -> Res<i32> {
    let (pos, flags, kv) =
        parse_simple("speed", args, &["--json", "--backtest"], &["--since", "--repo"])?;
    if !pos.is_empty() {
        return Err(unrecognized_arg(
            &pos.join(" "),
            &["--json", "--backtest", "--since", "--repo"],
            "speed",
        ));
    }
    let json_out = has(&flags, "--json") || crate::output::json_enabled();
    let backtest_mode = has(&flags, "--backtest");
    let since_opt = value(&kv, "--since");
    let repo_filter = value(&kv, "--repo");

    let now_ts = epoch() as i64;
    let state = state_dir();

    if backtest_mode {
        let since_str = since_opt.unwrap_or("14d");
        let bt = backtest_in(&state, now_ts, since_str, repo_filter)?;
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&bt.to_json()).unwrap_or_default()
            );
        } else {
            print!("{}", format_backtest_human(&bt));
        }
        return Ok(0);
    }

    let since_str = since_opt.unwrap_or("7d");
    let reports = analyze_in(&state, now_ts, since_str, repo_filter, true)?;
    let bt = backtest_in(&state, now_ts, "14d", repo_filter).ok();
    let cal = analyze_calibration_in(&state, now_ts, since_str, repo_filter).ok();
    let cmd_out_ab = analyze_cmd_out_ab_in(&state, now_ts, since_str, repo_filter).unwrap_or_default();

    if json_out {
        let repos_json: Vec<Value> = reports.iter().map(|r| r.to_full_json()).collect();
        let mut doc = json!({
            "at": format_epoch(now_ts, false, "%Y-%m-%dT%H:%M:%SZ"),
            "requestedSince": since_str,
            "repos": repos_json,
        });
        if let Some(bt_rep) = bt {
            doc["etaBacktest"] = bt_rep.to_json();
        }
        if let Some(cal_rep) = cal {
            doc["etaCalibration"] = cal_rep.to_json();
        }
        if !cmd_out_ab.is_empty() {
            let ab_map: serde_json::Map<String, Value> = cmd_out_ab
                .iter()
                .map(|(k, v)| (k.clone(), v.to_json()))
                .collect();
            doc["cmdOutAb"] = Value::Object(ab_map);
        }
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        print!("{}", format_human(&reports, since_str));
        if !cmd_out_ab.is_empty() {
            print!("{}", crate::cmd_out::format_ab_summary_human(&cmd_out_ab));
        }
        if let Some(cal_rep) = cal.filter(|c| c.total_records > 0) {
            print!("{}", format_calibration_inline_summary(&cal_rep));
        }
        if let Some(bt_rep) = bt.filter(|b| b.overall_start.count > 0) {
            print!("{}", format_backtest_inline_summary(&bt_rep));
        }
    }
    Ok(0)
}

pub fn analyze_cmd_out_ab_in(
    state: &Path,
    now_ts: i64,
    since_str: &str,
    repo_filter: Option<&str>,
) -> Res<BTreeMap<String, crate::cmd_out::CmdOutGroupMetrics>> {
    let cutoff = if since_str.eq_ignore_ascii_case("all") {
        None
    } else {
        let secs = seconds(since_str)? as i64;
        Some(now_ts - secs)
    };
    let raw = events::read_since_in(state, 0);
    let mut by_run: HashMap<String, Value> = HashMap::new();
    for ev in raw {
        let co = &ev["payload"]["cmdOut"];
        if !co.is_object() {
            continue;
        }
        let ts = iso_epoch(s(&ev, "at")).unwrap_or(0);
        if let Some(cut) = cutoff {
            if ts < cut {
                continue;
            }
        }
        let repo = s(&ev, "repo");
        let name = short_repo_name(repo);
        if let Some(rf) = repo_filter {
            if !repo_matches_filter(repo, &name, rf) {
                continue;
            }
        }
        let run_id = s(&ev, "run");
        if !run_id.is_empty() {
            by_run.insert(run_id.to_string(), co.clone());
        }
    }
    let mut groups: BTreeMap<String, crate::cmd_out::CmdOutGroupMetrics> = BTreeMap::new();
    for co in by_run.values() {
        let group = s(co, "group");
        if group.is_empty() {
            continue;
        }
        let m = groups.entry(group.to_string()).or_default();
        m.runs += 1;
        m.triggers = m
            .triggers
            .saturating_add(co["triggers"].as_u64().unwrap_or(0));
        m.truncated_bytes = m
            .truncated_bytes
            .saturating_add(co["truncatedBytes"].as_u64().unwrap_or(0));
        m.author_steps = m
            .author_steps
            .saturating_add(co["authorSteps"].as_u64().unwrap_or(0));
        m.compile_test_steps = m
            .compile_test_steps
            .saturating_add(co["compileTestSteps"].as_u64().unwrap_or(0));
        m.author_input_tokens = m
            .author_input_tokens
            .saturating_add(n(co, "authorInputTokens"));
        if b(co, "hasAccept") {
            m.accept_total += 1;
            if b(co, "acceptFirstPass") {
                m.accept_first_pass += 1;
            }
        }
    }
    Ok(groups)
}

// ---------------------------------------------------------------------------
// Unified ETA model cache (`<state>/eta-model.json`) & online calibration log
// Built by `speed.rs` from `events.jsonl` and consumed read-only by `eta.rs`.
// ---------------------------------------------------------------------------

pub fn eta_model_path(state: &Path) -> PathBuf {
    state.join(ETA_MODEL_FILE)
}

pub fn eta_calibration_path(state: &Path) -> PathBuf {
    state.join(ETA_CALIBRATION_FILE)
}

#[derive(Debug, Clone, Default)]
pub struct RepoCalibrationReport {
    pub repo: String,
    pub repo_name: String,
    pub started_delivered: BacktestMetrics,
    pub accept_delivered: BacktestMetrics,
    pub started_applied: BacktestMetrics,
}

impl RepoCalibrationReport {
    pub fn primary_metrics(&self) -> Option<&BacktestMetrics> {
        if self.started_delivered.count > 0 {
            Some(&self.started_delivered)
        } else if self.accept_delivered.count > 0 {
            Some(&self.accept_delivered)
        } else if self.started_applied.count > 0 {
            Some(&self.started_applied)
        } else {
            None
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "repo": self.repo,
            "repoName": self.repo_name,
            "startedDelivered": self.started_delivered.to_json(),
            "acceptDelivered": self.accept_delivered.to_json(),
            "startedApplied": self.started_applied.to_json(),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct OnlineCalibrationReport {
    pub at: String,
    pub window: String,
    pub total_records: usize,
    pub overall_started_delivered: BacktestMetrics,
    pub overall_accept_delivered: BacktestMetrics,
    pub overall_started_applied: BacktestMetrics,
    pub repos: Vec<RepoCalibrationReport>,
}

impl OnlineCalibrationReport {
    pub fn to_json(&self) -> Value {
        json!({
            "at": self.at,
            "window": self.window,
            "totalRecords": self.total_records,
            "overall": {
                "startedDelivered": self.overall_started_delivered.to_json(),
                "acceptDelivered": self.overall_accept_delivered.to_json(),
                "startedApplied": self.overall_started_applied.to_json(),
            },
            "repos": self.repos.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
        })
    }
}

fn parse_fallback_level_str(raw: &str) -> FallbackLevel {
    match raw {
        "L1" => FallbackLevel::RepoModeClass,
        "L2" => FallbackLevel::RepoMode,
        "L3" => FallbackLevel::GlobalModeClass,
        "L4" => FallbackLevel::GlobalMode,
        _ => FallbackLevel::Prior,
    }
}

fn read_calibration_records(
    state: &Path,
    cutoff_ts: Option<i64>,
    repo_filter: Option<&str>,
) -> Vec<(String, String, String, BacktestObs)> {
    let path = eta_calibration_path(state);
    let Ok(file) = fs::File::open(&path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(cut) = cutoff_ts {
            if let Some(ts) = iso_epoch(s(&v, "at")) {
                if ts < cut {
                    continue;
                }
            }
        }
        let repo = s(&v, "repo").to_string();
        let rname = short_repo_name(&repo);
        if let Some(rf) = repo_filter {
            if !repo_matches_filter(&repo, &rname, rf) {
                continue;
            }
        }
        let checkpoint = s(&v, "checkpoint").to_string();
        let milestone = s(&v, "milestone").to_string();
        let (Some(actual), Some(p50), Some(p75)) = (
            v["actualSec"].as_f64(),
            v["predP50Sec"].as_f64(),
            v["predP75Sec"].as_f64(),
        ) else {
            continue;
        };
        if !actual.is_finite() || !p50.is_finite() || !p75.is_finite() {
            continue;
        }
        let level = parse_fallback_level_str(s(&v, "fallbackLevel"));
        out.push((
            repo,
            checkpoint,
            milestone,
            BacktestObs {
                actual_sec: actual.max(0.0),
                pred_p50_sec: p50.max(0.0),
                pred_p75_sec: p75.max(p50.max(0.0)),
                level,
            },
        ));
    }
    out
}

fn build_calibration_report(
    records: &[(String, String, String, BacktestObs)],
    now_ts: i64,
    window_label: &str,
) -> OnlineCalibrationReport {
    let mut all_sd = Vec::new();
    let mut all_ad = Vec::new();
    let mut all_sa = Vec::new();
    let mut by_repo_sd: BTreeMap<String, Vec<BacktestObs>> = BTreeMap::new();
    let mut by_repo_ad: BTreeMap<String, Vec<BacktestObs>> = BTreeMap::new();
    let mut by_repo_sa: BTreeMap<String, Vec<BacktestObs>> = BTreeMap::new();
    let mut repo_keys: BTreeMap<String, ()> = BTreeMap::new();

    for (repo, checkpoint, milestone, obs) in records {
        repo_keys.insert(repo.clone(), ());
        match (checkpoint.as_str(), milestone.as_str()) {
            ("started", "delivered" | "answered") => {
                all_sd.push(obs.clone());
                by_repo_sd.entry(repo.clone()).or_default().push(obs.clone());
            }
            ("accept_review", "delivered" | "answered") => {
                all_ad.push(obs.clone());
                by_repo_ad.entry(repo.clone()).or_default().push(obs.clone());
            }
            ("started", "applied") => {
                all_sa.push(obs.clone());
                by_repo_sa.entry(repo.clone()).or_default().push(obs.clone());
            }
            _ => {}
        }
    }

    let mut repos: Vec<RepoCalibrationReport> = repo_keys
        .into_keys()
        .map(|repo| {
            let sd = by_repo_sd.remove(&repo).unwrap_or_default();
            let ad = by_repo_ad.remove(&repo).unwrap_or_default();
            let sa = by_repo_sa.remove(&repo).unwrap_or_default();
            RepoCalibrationReport {
                repo_name: short_repo_name(&repo),
                repo,
                started_delivered: BacktestMetrics::from_obs(&sd),
                accept_delivered: BacktestMetrics::from_obs(&ad),
                started_applied: BacktestMetrics::from_obs(&sa),
            }
        })
        .collect();
    repos.sort_by(|a, b| {
        b.started_delivered
            .count
            .cmp(&a.started_delivered.count)
            .then_with(|| a.repo_name.cmp(&b.repo_name))
    });

    OnlineCalibrationReport {
        at: format_epoch(now_ts, false, "%Y-%m-%dT%H:%M:%SZ"),
        window: window_label.to_string(),
        total_records: records.len(),
        overall_started_delivered: BacktestMetrics::from_obs(&all_sd),
        overall_accept_delivered: BacktestMetrics::from_obs(&all_ad),
        overall_started_applied: BacktestMetrics::from_obs(&all_sa),
        repos,
    }
}

fn analyze_calibration_by_repo(
    state: &Path,
    now_ts: i64,
    requested_secs: Option<i64>,
) -> HashMap<String, RepoCalibrationReport> {
    // Expand calibration lookback to at least 14d when requested window < 14d so sparse
    // repos still show coverage cleanly.
    let cutoff = requested_secs.map(|d| now_ts - d.max(EXPAND_WINDOW_SECS));
    let records = read_calibration_records(state, cutoff, None);
    if records.is_empty() {
        return HashMap::new();
    }
    let rep = build_calibration_report(&records, now_ts, "14d");
    rep.repos
        .into_iter()
        .map(|r| (r.repo.clone(), r))
        .collect()
}

pub fn analyze_calibration_in(
    state: &Path,
    now_ts: i64,
    since_str: &str,
    repo_filter: Option<&str>,
) -> Res<OnlineCalibrationReport> {
    let (cutoff, label) = if since_str.eq_ignore_ascii_case("all") {
        (None, "all".to_string())
    } else {
        let secs = seconds(since_str)?.round() as i64;
        if secs <= 0 {
            return Err(format!("--since must be positive, got {since_str:?}"));
        }
        (Some(now_ts - secs.max(EXPAND_WINDOW_SECS)), since_str.to_string())
    };
    let records = read_calibration_records(state, cutoff, repo_filter);
    Ok(build_calibration_report(&records, now_ts, &label))
}

fn format_calibration_inline_summary(cal: &OnlineCalibrationReport) -> String {
    let mut out = format!(
        "\nETA 在线校准（来源 eta-calibration.jsonl，健康 65%–85%，偏差 ±15% 内，共 {} 条）：\n",
        cal.total_records
    );
    if cal.overall_started_delivered.count > 0 {
        out.push_str(&format!(
            "  起跑→交付：{}\n",
            fmt_bt_metric_brief(&cal.overall_started_delivered)
        ));
    }
    if cal.overall_accept_delivered.count > 0 {
        out.push_str(&format!(
            "  验收→交付：{}\n",
            fmt_bt_metric_brief(&cal.overall_accept_delivered)
        ));
    }
    if cal.overall_started_applied.count > 0 {
        out.push_str(&format!(
            "  起跑→合入：{}\n",
            fmt_bt_metric_brief(&cal.overall_started_applied)
        ));
    }
    out
}

pub fn normalize_mode_class(mode: &str, class: &str, tier: &str) -> (String, String) {
    let m = if mode == "write" { "write" } else { "read-only" };
    let c = if m == "write" {
        match class.trim() {
            "hard" => "hard",
            "build" => "build",
            "design" => "design",
            _ => "other",
        }
    } else {
        match class.trim() {
            "scout" => "scout",
            "review" => "review",
            "design" => "design",
            "" => match tier.trim() {
                "scout" => "scout",
                "review" => "review",
                _ => "other",
            },
            _ => "other",
        }
    };
    (m.to_string(), c.to_string())
}

pub fn mode_class_key(mode: &str, class: &str) -> String {
    format!("{mode}:{class}")
}

pub fn accept_signature(tiered: bool, fail_closed: bool, ran_layers: &[String]) -> String {
    if fail_closed {
        return "failClosed".to_string();
    }
    if !tiered {
        return "default".to_string();
    }
    let mut sorted: Vec<&str> = ran_layers
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if sorted.is_empty() {
        return "tiered:empty".to_string();
    }
    sorted.sort_unstable();
    sorted.dedup();
    sorted.join(",")
}

/// Empirical distribution in seconds, retaining up to `MAX_STORED_SAMPLES` sorted
/// quantile points so `eta.rs` can compute conditional residual life `{x - e | x > e}`
/// without re-reading `events.jsonl`.
#[derive(Debug, Clone, Default)]
pub struct SampleDist {
    pub count: usize,
    pub p50_sec: Option<f64>,
    pub p75_sec: Option<f64>,
    pub p90_sec: Option<f64>,
    pub mean_sec: Option<f64>,
    pub samples_sec: Vec<f64>,
}

impl SampleDist {
    pub fn from_secs(vals: Vec<f64>) -> Self {
        let mut clean: Vec<f64> = vals
            .into_iter()
            .filter(|v| v.is_finite() && *v >= 0.0)
            .collect();
        if clean.is_empty() {
            return Self::default();
        }
        clean.sort_by(|a, b| a.total_cmp(b));
        let count = clean.len();
        let sum: f64 = clean.iter().sum();
        let p50_sec = percentile(&clean, 0.50).map(round2);
        let p75_sec = percentile(&clean, 0.75).map(round2);
        let p90_sec = percentile(&clean, 0.90).map(round2);
        let mean_sec = Some(round2(sum / count as f64));

        let samples_sec = if count <= MAX_STORED_SAMPLES {
            clean.into_iter().map(round2).collect()
        } else {
            (0..MAX_STORED_SAMPLES)
                .map(|i| {
                    let idx = i * (count - 1) / (MAX_STORED_SAMPLES - 1);
                    round2(clean[idx])
                })
                .collect()
        };

        Self {
            count,
            p50_sec,
            p75_sec,
            p90_sec,
            mean_sec,
            samples_sec,
        }
    }

    pub fn prior(p50_sec: f64, p75_sec: f64) -> Self {
        let p75 = p75_sec.max(p50_sec);
        let p90 = round2(p75 + (p75 - p50_sec));
        Self {
            count: 0,
            p50_sec: Some(round2(p50_sec)),
            p75_sec: Some(round2(p75)),
            p90_sec: Some(p90),
            mean_sec: Some(round2(p50_sec)),
            samples_sec: Vec::new(),
        }
    }

    /// Compute empirical conditional residual life `{x_i - elapsed | x_i > elapsed}`:
    /// returns `(p50_rem_sec, p75_rem_sec, anomaly)`.
    /// When `elapsed > p90` or fewer than 5 conditional samples remain, switches to the
    /// conservative tail `max(5m, 0.25 * p50)` with `anomaly = true`.
    #[allow(dead_code)] // Consumed by eta.rs in Chain 2.
    pub fn conditional_residual(&self, elapsed_sec: f64) -> Option<(f64, f64, bool)> {
        let e = elapsed_sec.max(0.0);
        if !self.samples_sec.is_empty() {
            let rem: Vec<f64> = self
                .samples_sec
                .iter()
                .copied()
                .filter(|&x| x > e)
                .map(|x| x - e)
                .collect();
            let p90 = self.p90_sec.unwrap_or(f64::INFINITY);
            if e > p90 || rem.len() < 5 {
                let p50_base = self.p50_sec.unwrap_or(0.0);
                let p75_base = self.p75_sec.unwrap_or(p50_base);
                let tail50 = 300.0_f64.max(0.25 * p50_base);
                let tail75 = tail50.max(0.40 * p75_base);
                return Some((round2(tail50), round2(tail75), true));
            }
            let r50 = percentile(&rem, 0.50)?;
            let r75 = percentile(&rem, 0.75)?.max(r50);
            return Some((round2(r50), round2(r75), false));
        }
        let (p50, p75) = (self.p50_sec?, self.p75_sec.unwrap_or(self.p50_sec?));
        let p90 = self.p90_sec.unwrap_or(p75 + (p75 - p50));
        if e > p90 {
            let tail50 = 300.0_f64.max(0.25 * p50);
            let tail75 = tail50.max(0.40 * p75);
            return Some((round2(tail50), round2(tail75), true));
        }
        let floor = (0.25 * p50).min(60.0);
        let r50 = (p50 - e).max(floor);
        let r75 = (p75 - e).max(r50);
        Some((round2(r50), round2(r75), false))
    }

    pub fn to_json(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("count".into(), json!(self.count));
        obj.insert("p50Sec".into(), json!(self.p50_sec));
        obj.insert("p75Sec".into(), json!(self.p75_sec));
        obj.insert("p90Sec".into(), json!(self.p90_sec));
        obj.insert("meanSec".into(), json!(self.mean_sec));
        obj.insert("p50Min".into(), json!(self.p50_sec.map(sec_to_min)));
        obj.insert("p75Min".into(), json!(self.p75_sec.map(sec_to_min)));
        if !self.samples_sec.is_empty() {
            obj.insert("samplesSec".into(), json!(self.samples_sec));
        }
        Value::Object(obj)
    }

    #[allow(dead_code)] // Consumed by eta.rs in Chain 2.
    pub fn from_json(v: &Value) -> Self {
        let samples_sec = v["samplesSec"]
            .as_array()
            .map(|arr| arr.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_default();
        Self {
            count: v["count"].as_u64().unwrap_or(0) as usize,
            p50_sec: v["p50Sec"].as_f64(),
            p75_sec: v["p75Sec"].as_f64(),
            p90_sec: v["p90Sec"].as_f64(),
            mean_sec: v["meanSec"].as_f64(),
            samples_sec,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BucketStats {
    pub chain_count: usize,
    pub delivered_count: usize,
    pub applied_count: usize,
    pub run_to_delivered: SampleDist,
    pub chain_to_delivered: SampleDist,
    pub chain_to_applied: SampleDist,
    pub t_crit: SampleDist,
    pub first_pass: SampleDist,
    pub capacity_queue: SampleDist,
    pub agent_work: SampleDist,
    pub accept: SampleDist,
    pub review: SampleDist,
    pub accept_review_parallel: SampleDist,
    pub rework_round: SampleDist,
    pub premerge_reused: SampleDist,
    pub premerge_rerun: SampleDist,
    pub merge_wait: SampleDist,
    pub suffix_after_work: SampleDist,
    pub suffix_after_accept: SampleDist,
    pub suffix_after_review: SampleDist,
    pub suffix_after_rework: SampleDist,
    pub auto_rework_rate: Option<f64>,
    pub non_first_pass_rate: Option<f64>,
    pub rounds_3plus_rate: Option<f64>,
    pub premerge_issue_rate: Option<f64>,
}

impl BucketStats {
    #[allow(dead_code)] // Consumed by eta.rs in Chain 2.
    pub fn primary_count(&self) -> usize {
        self.run_to_delivered.count.max(self.chain_to_applied.count)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "chainCount": self.chain_count,
            "deliveredCount": self.delivered_count,
            "appliedCount": self.applied_count,
            "endToEnd": {
                "runToDelivered": self.run_to_delivered.to_json(),
                "chainToDelivered": self.chain_to_delivered.to_json(),
                "chainToApplied": self.chain_to_applied.to_json(),
                "tCrit": self.t_crit.to_json(),
                "firstPass": self.first_pass.to_json(),
            },
            "stages": {
                "capacityQueue": self.capacity_queue.to_json(),
                "agentWork": self.agent_work.to_json(),
                "accept": self.accept.to_json(),
                "review": self.review.to_json(),
                "acceptReviewParallel": self.accept_review_parallel.to_json(),
                "reworkRound": self.rework_round.to_json(),
                "premergeReused": self.premerge_reused.to_json(),
                "premergeRerun": self.premerge_rerun.to_json(),
                "mergeWait": self.merge_wait.to_json(),
            },
            "suffixes": {
                "afterWork": self.suffix_after_work.to_json(),
                "afterAccept": self.suffix_after_accept.to_json(),
                "afterReview": self.suffix_after_review.to_json(),
                "afterRework": self.suffix_after_rework.to_json(),
            },
            "rates": {
                "autoRework": self.auto_rework_rate.map(round4),
                "nonFirstPass": self.non_first_pass_rate.map(round4),
                "rounds3Plus": self.rounds_3plus_rate.map(round4),
                "premergeIssue": self.premerge_issue_rate.map(round4),
            },
        })
    }

    #[allow(dead_code)] // Consumed by eta.rs in Chain 2.
    pub fn from_json(v: &Value) -> Self {
        let e2e = &v["endToEnd"];
        let st = &v["stages"];
        let sf = &v["suffixes"];
        let rt = &v["rates"];
        Self {
            chain_count: v["chainCount"].as_u64().unwrap_or(0) as usize,
            delivered_count: v["deliveredCount"].as_u64().unwrap_or(0) as usize,
            applied_count: v["appliedCount"].as_u64().unwrap_or(0) as usize,
            run_to_delivered: SampleDist::from_json(&e2e["runToDelivered"]),
            chain_to_delivered: SampleDist::from_json(&e2e["chainToDelivered"]),
            chain_to_applied: SampleDist::from_json(&e2e["chainToApplied"]),
            t_crit: SampleDist::from_json(&e2e["tCrit"]),
            first_pass: SampleDist::from_json(&e2e["firstPass"]),
            capacity_queue: SampleDist::from_json(&st["capacityQueue"]),
            agent_work: SampleDist::from_json(&st["agentWork"]),
            accept: SampleDist::from_json(&st["accept"]),
            review: SampleDist::from_json(&st["review"]),
            accept_review_parallel: SampleDist::from_json(&st["acceptReviewParallel"]),
            rework_round: SampleDist::from_json(&st["reworkRound"]),
            premerge_reused: SampleDist::from_json(&st["premergeReused"]),
            premerge_rerun: SampleDist::from_json(&st["premergeRerun"]),
            merge_wait: SampleDist::from_json(&st["mergeWait"]),
            suffix_after_work: SampleDist::from_json(&sf["afterWork"]),
            suffix_after_accept: SampleDist::from_json(&sf["afterAccept"]),
            suffix_after_review: SampleDist::from_json(&sf["afterReview"]),
            suffix_after_rework: SampleDist::from_json(&sf["afterRework"]),
            auto_rework_rate: rt["autoRework"].as_f64(),
            non_first_pass_rate: rt["nonFirstPass"].as_f64(),
            rounds_3plus_rate: rt["rounds3Plus"].as_f64(),
            premerge_issue_rate: rt["premergeIssue"].as_f64(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RepoFormulaParams {
    pub write_applied_count: usize,
    pub t_chain_p50_sec: Option<f64>,
    pub t_chain_p75_sec: Option<f64>,
    pub t_crit_p50_sec: Option<f64>,
    pub t_crit_p75_sec: Option<f64>,
    pub p_eff_mean: Option<f64>,
    pub p_eff_p50: Option<f64>,
    pub p_eff_p75: Option<f64>,
    pub t_merge_p50_sec: Option<f64>,
    pub t_merge_p75_sec: Option<f64>,
    pub auto_rework_rate: Option<f64>,
    pub non_first_pass_rate: Option<f64>,
    pub rounds_3plus_rate: Option<f64>,
    pub premerge_issue_rate: Option<f64>,
}

impl RepoFormulaParams {
    pub fn to_json(&self) -> Value {
        json!({
            "writeAppliedCount": self.write_applied_count,
            "tChainP50Sec": self.t_chain_p50_sec.map(round2),
            "tChainP75Sec": self.t_chain_p75_sec.map(round2),
            "tCritP50Sec": self.t_crit_p50_sec.map(round2),
            "tCritP75Sec": self.t_crit_p75_sec.map(round2),
            "pEffMean": self.p_eff_mean.map(round2),
            "pEffP50": self.p_eff_p50.map(round2),
            "pEffP75": self.p_eff_p75.map(round2),
            "tMergeP50Sec": self.t_merge_p50_sec.map(round2),
            "tMergeP75Sec": self.t_merge_p75_sec.map(round2),
            "autoReworkRate": self.auto_rework_rate.map(round4),
            "nonFirstPassRate": self.non_first_pass_rate.map(round4),
            "rounds3PlusRate": self.rounds_3plus_rate.map(round4),
            "premergeIssueRate": self.premerge_issue_rate.map(round4),
        })
    }

    #[allow(dead_code)] // Consumed by eta.rs in Chain 2.
    pub fn from_json(v: &Value) -> Self {
        Self {
            write_applied_count: v["writeAppliedCount"].as_u64().unwrap_or(0) as usize,
            t_chain_p50_sec: v["tChainP50Sec"].as_f64(),
            t_chain_p75_sec: v["tChainP75Sec"].as_f64(),
            t_crit_p50_sec: v["tCritP50Sec"].as_f64(),
            t_crit_p75_sec: v["tCritP75Sec"].as_f64(),
            p_eff_mean: v["pEffMean"].as_f64(),
            p_eff_p50: v["pEffP50"].as_f64(),
            p_eff_p75: v["pEffP75"].as_f64(),
            t_merge_p50_sec: v["tMergeP50Sec"].as_f64(),
            t_merge_p75_sec: v["tMergeP75Sec"].as_f64(),
            auto_rework_rate: v["autoReworkRate"].as_f64(),
            non_first_pass_rate: v["nonFirstPassRate"].as_f64(),
            rounds_3plus_rate: v["rounds3PlusRate"].as_f64(),
            premerge_issue_rate: v["premergeIssueRate"].as_f64(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RecentLaneStats {
    pub window_secs: i64,
    pub queued_secs: f64,
    pub held_secs: f64,
    pub queue_share: f64,
    pub wait_p50_sec: Option<f64>,
    pub wait_nonzero_p50_sec: Option<f64>,
    pub acquire_count: usize,
}

impl RecentLaneStats {
    pub fn to_json(&self) -> Value {
        json!({
            "windowSecs": self.window_secs,
            "queuedSecs": round2(self.queued_secs),
            "heldSecs": round2(self.held_secs),
            "queueShare": round4(self.queue_share),
            "waitP50Sec": self.wait_p50_sec.map(round2),
            "waitNonZeroP50Sec": self.wait_nonzero_p50_sec.map(round2),
            "acquireCount": self.acquire_count,
        })
    }

    #[allow(dead_code)] // Consumed by eta.rs in Chain 2.
    pub fn from_json(v: &Value) -> Self {
        Self {
            window_secs: v["windowSecs"].as_i64().unwrap_or(RECENT_LANE_WINDOW_SECS),
            queued_secs: v["queuedSecs"].as_f64().unwrap_or(0.0),
            held_secs: v["heldSecs"].as_f64().unwrap_or(0.0),
            queue_share: v["queueShare"].as_f64().unwrap_or(0.0),
            wait_p50_sec: v["waitP50Sec"].as_f64(),
            wait_nonzero_p50_sec: v["waitNonZeroP50Sec"].as_f64(),
            acquire_count: v["acquireCount"].as_u64().unwrap_or(0) as usize,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RepoEtaModel {
    pub repo: String,
    pub repo_name: String,
    pub repo_slug: String,
    pub by_mode_class: BTreeMap<String, BucketStats>,
    pub by_mode: BTreeMap<String, BucketStats>,
    pub formula: RepoFormulaParams,
    pub accept_by_sig: BTreeMap<String, SampleDist>,
    pub accept_layers: BTreeMap<String, SampleDist>,
    pub lane_held: SampleDist,
    pub lane_queue: SampleDist,
    pub premerge_reused: SampleDist,
    pub premerge_rerun: SampleDist,
    pub merge_exec: SampleDist,
}

impl RepoEtaModel {
    pub fn to_json(&self) -> Value {
        json!({
            "repo": self.repo,
            "repoName": self.repo_name,
            "repoSlug": self.repo_slug,
            "byModeClass": map_buckets_to_json(&self.by_mode_class),
            "byMode": map_buckets_to_json(&self.by_mode),
            "formula": self.formula.to_json(),
            "acceptBySig": map_dists_to_json(&self.accept_by_sig),
            "acceptLayers": map_dists_to_json(&self.accept_layers),
            "laneHeld": self.lane_held.to_json(),
            "laneQueue": self.lane_queue.to_json(),
            "premergeReused": self.premerge_reused.to_json(),
            "premergeRerun": self.premerge_rerun.to_json(),
            "mergeExec": self.merge_exec.to_json(),
        })
    }

    #[allow(dead_code)] // Consumed by eta.rs in Chain 2.
    pub fn from_json(v: &Value) -> Self {
        Self {
            repo: s(v, "repo").to_string(),
            repo_name: s(v, "repoName").to_string(),
            repo_slug: s(v, "repoSlug").to_string(),
            by_mode_class: map_buckets_from_json(&v["byModeClass"]),
            by_mode: map_buckets_from_json(&v["byMode"]),
            formula: RepoFormulaParams::from_json(&v["formula"]),
            accept_by_sig: map_dists_from_json(&v["acceptBySig"]),
            accept_layers: map_dists_from_json(&v["acceptLayers"]),
            lane_held: SampleDist::from_json(&v["laneHeld"]),
            lane_queue: SampleDist::from_json(&v["laneQueue"]),
            premerge_reused: SampleDist::from_json(&v["premergeReused"]),
            premerge_rerun: SampleDist::from_json(&v["premergeRerun"]),
            merge_exec: SampleDist::from_json(&v["mergeExec"]),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct GlobalEtaModel {
    pub by_mode_class: BTreeMap<String, BucketStats>,
    pub by_mode: BTreeMap<String, BucketStats>,
    pub formula: RepoFormulaParams,
    pub accept_by_sig: BTreeMap<String, SampleDist>,
    pub accept_layers: BTreeMap<String, SampleDist>,
    pub lane_held: SampleDist,
    pub lane_queue: SampleDist,
    pub capacity_queue: SampleDist,
    pub capacity_queue_by_reason: BTreeMap<String, SampleDist>,
    pub premerge_reused: SampleDist,
    pub premerge_rerun: SampleDist,
    pub merge_exec: SampleDist,
    pub recent_lane_2h: RecentLaneStats,
}

impl GlobalEtaModel {
    pub fn to_json(&self) -> Value {
        json!({
            "byModeClass": map_buckets_to_json(&self.by_mode_class),
            "byMode": map_buckets_to_json(&self.by_mode),
            "formula": self.formula.to_json(),
            "acceptBySig": map_dists_to_json(&self.accept_by_sig),
            "acceptLayers": map_dists_to_json(&self.accept_layers),
            "laneHeld": self.lane_held.to_json(),
            "laneQueue": self.lane_queue.to_json(),
            "capacityQueue": self.capacity_queue.to_json(),
            "capacityQueueByReason": map_dists_to_json(&self.capacity_queue_by_reason),
            "premergeReused": self.premerge_reused.to_json(),
            "premergeRerun": self.premerge_rerun.to_json(),
            "mergeExec": self.merge_exec.to_json(),
            "recentLane2h": self.recent_lane_2h.to_json(),
        })
    }

    #[allow(dead_code)] // Consumed by eta.rs in Chain 2.
    pub fn from_json(v: &Value) -> Self {
        Self {
            by_mode_class: map_buckets_from_json(&v["byModeClass"]),
            by_mode: map_buckets_from_json(&v["byMode"]),
            formula: RepoFormulaParams::from_json(&v["formula"]),
            accept_by_sig: map_dists_from_json(&v["acceptBySig"]),
            accept_layers: map_dists_from_json(&v["acceptLayers"]),
            lane_held: SampleDist::from_json(&v["laneHeld"]),
            lane_queue: SampleDist::from_json(&v["laneQueue"]),
            capacity_queue: SampleDist::from_json(&v["capacityQueue"]),
            capacity_queue_by_reason: map_dists_from_json(&v["capacityQueueByReason"]),
            premerge_reused: SampleDist::from_json(&v["premergeReused"]),
            premerge_rerun: SampleDist::from_json(&v["premergeRerun"]),
            merge_exec: SampleDist::from_json(&v["mergeExec"]),
            recent_lane_2h: RecentLaneStats::from_json(&v["recentLane2h"]),
        }
    }
}

/// Built-in L5 cold-start priors (design.md §2.3: 11-24 controller calibration baseline).
#[derive(Debug, Clone)]
pub struct ColdStartPriors {
    pub write: BucketStats,
    pub read_only: BucketStats,
    pub formula: RepoFormulaParams,
    pub lane_held: SampleDist,
}

impl Default for ColdStartPriors {
    fn default() -> Self {
        let write = BucketStats {
            chain_count: 0,
            delivered_count: 0,
            applied_count: 0,
            run_to_delivered: SampleDist::prior(54.0 * 60.0, 78.0 * 60.0),
            chain_to_delivered: SampleDist::prior(54.0 * 60.0, 78.0 * 60.0),
            chain_to_applied: SampleDist::prior(63.0 * 60.0, 87.0 * 60.0),
            t_crit: SampleDist::prior(75.0 * 60.0, 102.0 * 60.0),
            first_pass: SampleDist::prior(45.0 * 60.0, 65.0 * 60.0),
            capacity_queue: SampleDist::prior(5.0 * 60.0, 12.0 * 60.0),
            agent_work: SampleDist::prior(30.0 * 60.0, 45.0 * 60.0),
            accept: SampleDist::prior(3.0 * 60.0, 6.0 * 60.0),
            review: SampleDist::prior(4.0 * 60.0, 8.0 * 60.0),
            accept_review_parallel: SampleDist::prior(5.0 * 60.0, 9.0 * 60.0),
            rework_round: SampleDist::prior(15.0 * 60.0, 25.0 * 60.0),
            premerge_reused: SampleDist::prior(5.0, 15.0),
            premerge_rerun: SampleDist::prior(3.0 * 60.0, 6.0 * 60.0),
            merge_wait: SampleDist::prior(9.0 * 60.0, 9.0 * 60.0),
            suffix_after_work: SampleDist::prior(18.0 * 60.0, 33.0 * 60.0),
            suffix_after_accept: SampleDist::prior(10.0 * 60.0, 22.0 * 60.0),
            suffix_after_review: SampleDist::prior(5.0 * 60.0, 18.0 * 60.0),
            suffix_after_rework: SampleDist::prior(15.0 * 60.0, 25.0 * 60.0),
            auto_rework_rate: Some(0.25),
            non_first_pass_rate: Some(0.35),
            rounds_3plus_rate: Some(0.10),
            premerge_issue_rate: Some(0.15),
        };
        let read_only = BucketStats {
            run_to_delivered: SampleDist::prior(5.0 * 60.0, 10.0 * 60.0),
            chain_to_delivered: SampleDist::prior(5.0 * 60.0, 10.0 * 60.0),
            agent_work: SampleDist::prior(5.0 * 60.0, 10.0 * 60.0),
            suffix_after_work: SampleDist::prior(0.0, 0.0),
            ..BucketStats::default()
        };
        let formula = RepoFormulaParams {
            write_applied_count: 0,
            t_chain_p50_sec: Some(63.0 * 60.0),
            t_chain_p75_sec: Some(87.0 * 60.0),
            t_crit_p50_sec: Some(75.0 * 60.0),
            t_crit_p75_sec: Some(102.0 * 60.0),
            p_eff_mean: Some(2.0),
            p_eff_p50: Some(2.0),
            p_eff_p75: Some(2.0),
            t_merge_p50_sec: Some(9.0 * 60.0),
            t_merge_p75_sec: Some(9.0 * 60.0),
            auto_rework_rate: Some(0.25),
            non_first_pass_rate: Some(0.35),
            rounds_3plus_rate: Some(0.10),
            premerge_issue_rate: Some(0.15),
        };
        Self {
            write,
            read_only,
            formula,
            lane_held: SampleDist::prior(2.0 * 60.0, 5.0 * 60.0),
        }
    }
}

impl ColdStartPriors {
    pub fn bucket_for_mode(&self, mode: &str) -> &BucketStats {
        if mode == "write" {
            &self.write
        } else {
            &self.read_only
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "write": self.write.to_json(),
            "readOnly": self.read_only.to_json(),
            "formula": self.formula.to_json(),
            "laneHeld": self.lane_held.to_json(),
        })
    }

    #[allow(dead_code)] // Consumed by eta.rs in Chain 2.
    pub fn from_json(v: &Value) -> Self {
        if !v.is_object() {
            return Self::default();
        }
        Self {
            write: if v["write"].is_object() {
                BucketStats::from_json(&v["write"])
            } else {
                Self::default().write
            },
            read_only: if v["readOnly"].is_object() {
                BucketStats::from_json(&v["readOnly"])
            } else {
                Self::default().read_only
            },
            formula: if v["formula"].is_object() {
                RepoFormulaParams::from_json(&v["formula"])
            } else {
                Self::default().formula
            },
            lane_held: if v["laneHeld"].is_object() {
                SampleDist::from_json(&v["laneHeld"])
            } else {
                Self::default().lane_held
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Consumed by eta.rs in Chain 2.
pub enum FallbackLevel {
    /// L1: `(repo, mode, class)` with `n >= 10`
    RepoModeClass,
    /// L2: `(repo, mode)` with `n >= 10`
    RepoMode,
    /// L3: `(all_repos, mode, class)` with `n >= 10`
    GlobalModeClass,
    /// L4: `(all_repos, mode)` with `n >= 5`
    GlobalMode,
    /// L5: Built-in cold-start prior (marked `(初估)`)
    Prior,
}

#[allow(dead_code)] // Consumed by eta.rs in Chain 2.
impl FallbackLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RepoModeClass => "L1",
            Self::RepoMode => "L2",
            Self::GlobalModeClass => "L3",
            Self::GlobalMode => "L4",
            Self::Prior => "L5",
        }
    }

    pub fn is_prior(&self) -> bool {
        *self == Self::Prior
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // Consumed by eta.rs in Chain 2.
pub struct ResolvedBucket<'a> {
    pub level: FallbackLevel,
    pub key: String,
    pub bucket: &'a BucketStats,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // Consumed by eta.rs in Chain 2.
pub struct ResolvedDist<'a> {
    pub level: FallbackLevel,
    pub key: String,
    pub dist: &'a SampleDist,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // Consumed by eta.rs in Chain 2.
pub struct ResolvedFormula<'a> {
    pub level: FallbackLevel,
    pub params: &'a RepoFormulaParams,
}

#[derive(Debug, Clone)]
pub struct EtaModel {
    pub version: u32,
    pub generated_at: String,
    pub generated_epoch: i64,
    pub events_seq: u64,
    pub window_days: i64,
    pub repos: BTreeMap<String, RepoEtaModel>,
    pub global: GlobalEtaModel,
    pub priors: ColdStartPriors,
}

impl EtaModel {
    pub fn to_json(&self) -> Value {
        let mut repos_map = serde_json::Map::new();
        for (k, r) in &self.repos {
            repos_map.insert(k.clone(), r.to_json());
        }
        json!({
            "version": self.version,
            "generatedAt": self.generated_at,
            "generatedEpoch": self.generated_epoch,
            "eventsSeq": self.events_seq,
            "windowDays": self.window_days,
            "minBucketN": MIN_BUCKET_N,
            "minGlobalModeN": MIN_GLOBAL_MODE_N,
            "repos": Value::Object(repos_map),
            "global": self.global.to_json(),
            "priors": self.priors.to_json(),
        })
    }

    #[allow(dead_code)] // Consumed by eta.rs in Chain 2.
    pub fn from_json(v: &Value) -> Option<Self> {
        if !v.is_object() {
            return None;
        }
        let version = v["version"].as_u64()? as u32;
        if version != ETA_MODEL_VERSION {
            return None;
        }
        let mut repos = BTreeMap::new();
        if let Some(obj) = v["repos"].as_object() {
            for (k, rv) in obj {
                repos.insert(k.clone(), RepoEtaModel::from_json(rv));
            }
        }
        Some(Self {
            version,
            generated_at: s(v, "generatedAt").to_string(),
            generated_epoch: v["generatedEpoch"].as_i64().unwrap_or(0),
            events_seq: v["eventsSeq"].as_u64().unwrap_or(0),
            window_days: v["windowDays"].as_i64().unwrap_or(ETA_MODEL_WINDOW_DAYS),
            repos,
            global: GlobalEtaModel::from_json(&v["global"]),
            priors: ColdStartPriors::from_json(&v["priors"]),
        })
    }

    #[allow(dead_code)] // Consumed by eta.rs in Chain 2.
    pub fn repo_model(&self, repo: &str) -> Option<&RepoEtaModel> {
        if let Some(m) = self.repos.get(repo) {
            return Some(m);
        }
        let name = short_repo_name(repo);
        self.repos.values().find(|m| m.repo_name == name)
    }

    /// Resolve a primary bucket through the 5-level fallback chain (L1–L5):
    /// L1 `(repo, mode, class)` (`n >= 10`) → L2 `(repo, mode)` (`n >= 10`)
    /// → L3 `(global, mode, class)` (`n >= 10`) → L4 `(global, mode)` (`n >= 5`)
    /// → L5 built-in cold-start prior.
    #[allow(dead_code)] // Consumed by eta.rs in Chain 2.
    pub fn resolve_bucket(&self, repo: &str, mode: &str, class: &str) -> ResolvedBucket<'_> {
        let (norm_mode, norm_class) = normalize_mode_class(mode, class, "");
        let mc_key = mode_class_key(&norm_mode, &norm_class);
        if let Some(rm) = self.repo_model(repo) {
            if let Some(b) = rm.by_mode_class.get(&mc_key) {
                if b.primary_count() >= MIN_BUCKET_N {
                    return ResolvedBucket {
                        level: FallbackLevel::RepoModeClass,
                        key: format!("{}:{mc_key}", rm.repo_name),
                        bucket: b,
                    };
                }
            }
            if let Some(b) = rm.by_mode.get(&norm_mode) {
                if b.primary_count() >= MIN_BUCKET_N {
                    return ResolvedBucket {
                        level: FallbackLevel::RepoMode,
                        key: format!("{}:{norm_mode}", rm.repo_name),
                        bucket: b,
                    };
                }
            }
        }
        if let Some(b) = self.global.by_mode_class.get(&mc_key) {
            if b.primary_count() >= MIN_BUCKET_N {
                return ResolvedBucket {
                    level: FallbackLevel::GlobalModeClass,
                    key: format!("all:{mc_key}"),
                    bucket: b,
                };
            }
        }
        if let Some(b) = self.global.by_mode.get(&norm_mode) {
            if b.primary_count() >= MIN_GLOBAL_MODE_N {
                return ResolvedBucket {
                    level: FallbackLevel::GlobalMode,
                    key: format!("all:{norm_mode}"),
                    bucket: b,
                };
            }
        }
        ResolvedBucket {
            level: FallbackLevel::Prior,
            key: format!("prior:{norm_mode}"),
            bucket: self.priors.bucket_for_mode(&norm_mode),
        }
    }

    /// Resolve a specific distribution field inside `BucketStats` through the same L1–L5
    /// fallback chain, so sparse stages gracefully widen without blocking well-populated stages.
    #[allow(dead_code)] // Consumed by eta.rs in Chain 2.
    pub fn resolve_dist<'a, F>(
        &'a self,
        repo: &str,
        mode: &str,
        class: &str,
        pick: F,
    ) -> ResolvedDist<'a>
    where
        F: Fn(&'a BucketStats) -> &'a SampleDist,
    {
        let (norm_mode, norm_class) = normalize_mode_class(mode, class, "");
        let mc_key = mode_class_key(&norm_mode, &norm_class);
        if let Some(rm) = self.repo_model(repo) {
            if let Some(b) = rm.by_mode_class.get(&mc_key) {
                let d = pick(b);
                if d.count >= MIN_BUCKET_N {
                    return ResolvedDist {
                        level: FallbackLevel::RepoModeClass,
                        key: format!("{}:{mc_key}", rm.repo_name),
                        dist: d,
                    };
                }
            }
            if let Some(b) = rm.by_mode.get(&norm_mode) {
                let d = pick(b);
                if d.count >= MIN_BUCKET_N {
                    return ResolvedDist {
                        level: FallbackLevel::RepoMode,
                        key: format!("{}:{norm_mode}", rm.repo_name),
                        dist: d,
                    };
                }
            }
        }
        if let Some(b) = self.global.by_mode_class.get(&mc_key) {
            let d = pick(b);
            if d.count >= MIN_BUCKET_N {
                return ResolvedDist {
                    level: FallbackLevel::GlobalModeClass,
                    key: format!("all:{mc_key}"),
                    dist: d,
                };
            }
        }
        if let Some(b) = self.global.by_mode.get(&norm_mode) {
            let d = pick(b);
            if d.count >= MIN_GLOBAL_MODE_N {
                return ResolvedDist {
                    level: FallbackLevel::GlobalMode,
                    key: format!("all:{norm_mode}"),
                    dist: d,
                };
            }
        }
        ResolvedDist {
            level: FallbackLevel::Prior,
            key: format!("prior:{norm_mode}"),
            dist: pick(self.priors.bucket_for_mode(&norm_mode)),
        }
    }

    /// Resolve acceptance duration by `(repo, failClosed/ranLayers signature)` with L1–L5 fallback.
    #[allow(dead_code)] // Consumed by eta.rs in Chain 2.
    pub fn resolve_accept_dist(
        &self,
        repo: &str,
        tiered: bool,
        fail_closed: bool,
        ran_layers: &[String],
    ) -> ResolvedDist<'_> {
        let sig = accept_signature(tiered, fail_closed, ran_layers);
        if let Some(rm) = self.repo_model(repo) {
            if let Some(d) = rm.accept_by_sig.get(&sig) {
                if d.count >= MIN_BUCKET_N {
                    return ResolvedDist {
                        level: FallbackLevel::RepoModeClass,
                        key: format!("{}:accept:{sig}", rm.repo_name),
                        dist: d,
                    };
                }
            }
            if let Some(b) = rm.by_mode.get("write") {
                if b.accept.count >= MIN_BUCKET_N {
                    return ResolvedDist {
                        level: FallbackLevel::RepoMode,
                        key: format!("{}:accept", rm.repo_name),
                        dist: &b.accept,
                    };
                }
            }
        }
        if let Some(d) = self.global.accept_by_sig.get(&sig) {
            if d.count >= MIN_BUCKET_N {
                return ResolvedDist {
                    level: FallbackLevel::GlobalModeClass,
                    key: format!("all:accept:{sig}"),
                    dist: d,
                };
            }
        }
        if let Some(b) = self.global.by_mode.get("write") {
            if b.accept.count >= MIN_GLOBAL_MODE_N {
                return ResolvedDist {
                    level: FallbackLevel::GlobalMode,
                    key: "all:accept".to_string(),
                    dist: &b.accept,
                };
            }
        }
        ResolvedDist {
            level: FallbackLevel::Prior,
            key: "prior:accept".to_string(),
            dist: &self.priors.write.accept,
        }
    }

    /// Resolve `T_dev` formula parameters for `repo` (L2 repo → L4 global → L5 prior).
    #[allow(dead_code)] // Consumed by eta.rs in Chain 2.
    pub fn resolve_formula(&self, repo: &str) -> ResolvedFormula<'_> {
        if let Some(rm) = self.repo_model(repo) {
            if rm.formula.write_applied_count >= MIN_BUCKET_N {
                return ResolvedFormula {
                    level: FallbackLevel::RepoMode,
                    params: &rm.formula,
                };
            }
        }
        if self.global.formula.write_applied_count >= MIN_GLOBAL_MODE_N {
            return ResolvedFormula {
                level: FallbackLevel::GlobalMode,
                params: &self.global.formula,
            };
        }
        ResolvedFormula {
            level: FallbackLevel::Prior,
            params: &self.priors.formula,
        }
    }

    /// Resolve `lane_held` duration for `repo` (L2 repo → L4 global → L5 prior).
    #[allow(dead_code)] // Consumed by eta.rs in Chain 2.
    pub fn resolve_lane_held(&self, repo: &str) -> ResolvedDist<'_> {
        if let Some(rm) = self.repo_model(repo) {
            if rm.lane_held.count >= MIN_BUCKET_N {
                return ResolvedDist {
                    level: FallbackLevel::RepoMode,
                    key: format!("{}:laneHeld", rm.repo_name),
                    dist: &rm.lane_held,
                };
            }
        }
        if self.global.lane_held.count >= MIN_GLOBAL_MODE_N {
            return ResolvedDist {
                level: FallbackLevel::GlobalMode,
                key: "all:laneHeld".to_string(),
                dist: &self.global.lane_held,
            };
        }
        ResolvedDist {
            level: FallbackLevel::Prior,
            key: "prior:laneHeld".to_string(),
            dist: &self.priors.lane_held,
        }
    }
}

fn map_buckets_to_json(m: &BTreeMap<String, BucketStats>) -> Value {
    let mut out = serde_json::Map::new();
    for (k, v) in m {
        out.insert(k.clone(), v.to_json());
    }
    Value::Object(out)
}

#[allow(dead_code)] // Consumed by eta.rs in Chain 2.
fn map_buckets_from_json(v: &Value) -> BTreeMap<String, BucketStats> {
    let mut out = BTreeMap::new();
    if let Some(obj) = v.as_object() {
        for (k, bv) in obj {
            out.insert(k.clone(), BucketStats::from_json(bv));
        }
    }
    out
}

fn map_dists_to_json(m: &BTreeMap<String, SampleDist>) -> Value {
    let mut out = serde_json::Map::new();
    for (k, v) in m {
        out.insert(k.clone(), v.to_json());
    }
    Value::Object(out)
}

#[allow(dead_code)] // Consumed by eta.rs in Chain 2.
fn map_dists_from_json(v: &Value) -> BTreeMap<String, SampleDist> {
    let mut out = BTreeMap::new();
    if let Some(obj) = v.as_object() {
        for (k, dv) in obj {
            out.insert(k.clone(), SampleDist::from_json(dv));
        }
    }
    out
}

#[derive(Default)]
struct BucketAcc {
    chain_count: usize,
    delivered_count: usize,
    applied_count: usize,
    auto_rework_count: usize,
    non_first_pass_count: usize,
    rounds_3plus_count: usize,
    premerge_issue_count: usize,
    run_to_delivered: Vec<f64>,
    chain_to_delivered: Vec<f64>,
    chain_to_applied: Vec<f64>,
    t_crit: Vec<f64>,
    first_pass: Vec<f64>,
    capacity_queue: Vec<f64>,
    agent_work: Vec<f64>,
    accept: Vec<f64>,
    review: Vec<f64>,
    accept_review_parallel: Vec<f64>,
    rework_round: Vec<f64>,
    premerge_reused: Vec<f64>,
    premerge_rerun: Vec<f64>,
    merge_wait: Vec<f64>,
    suffix_after_work: Vec<f64>,
    suffix_after_accept: Vec<f64>,
    suffix_after_review: Vec<f64>,
    suffix_after_rework: Vec<f64>,
}

impl BucketAcc {
    fn push_chain(&mut self, ch: &ChainInfo) {
        self.chain_count += 1;
        if ch.has_premerge_conflict || ch.has_premerge_failed {
            self.premerge_issue_count += 1;
        }
        if let Some(del) = ch.delivered_secs() {
            self.delivered_count += 1;
            self.chain_to_delivered.push(del);
        }
        if ch.is_write {
            if let Some(wall) = ch.wall_clock_secs() {
                self.applied_count += 1;
                self.chain_to_applied.push(wall);
                if ch.rework_sent_count > 0 {
                    self.auto_rework_count += 1;
                }
                if ch.is_non_first_pass() {
                    self.non_first_pass_count += 1;
                    self.t_crit.push(wall);
                } else {
                    self.first_pass.push(wall);
                }
                if ch.is_three_plus_rounds() {
                    self.rounds_3plus_count += 1;
                }
                if let Some(w) = ch.merge_wait_secs() {
                    self.merge_wait.push(w);
                }
            }
        }
    }

    fn finish(self) -> BucketStats {
        let rate = |num: usize, den: usize| -> Option<f64> {
            if den == 0 {
                None
            } else {
                Some(num as f64 / den as f64)
            }
        };
        let applied = self.applied_count;
        BucketStats {
            chain_count: self.chain_count,
            delivered_count: self.delivered_count,
            applied_count: applied,
            run_to_delivered: SampleDist::from_secs(self.run_to_delivered),
            chain_to_delivered: SampleDist::from_secs(self.chain_to_delivered),
            chain_to_applied: SampleDist::from_secs(self.chain_to_applied),
            t_crit: SampleDist::from_secs(self.t_crit),
            first_pass: SampleDist::from_secs(self.first_pass),
            capacity_queue: SampleDist::from_secs(self.capacity_queue),
            agent_work: SampleDist::from_secs(self.agent_work),
            accept: SampleDist::from_secs(self.accept),
            review: SampleDist::from_secs(self.review),
            accept_review_parallel: SampleDist::from_secs(self.accept_review_parallel),
            rework_round: SampleDist::from_secs(self.rework_round),
            premerge_reused: SampleDist::from_secs(self.premerge_reused),
            premerge_rerun: SampleDist::from_secs(self.premerge_rerun),
            merge_wait: SampleDist::from_secs(self.merge_wait),
            suffix_after_work: SampleDist::from_secs(self.suffix_after_work),
            suffix_after_accept: SampleDist::from_secs(self.suffix_after_accept),
            suffix_after_review: SampleDist::from_secs(self.suffix_after_review),
            suffix_after_rework: SampleDist::from_secs(self.suffix_after_rework),
            auto_rework_rate: rate(self.auto_rework_count, applied),
            non_first_pass_rate: rate(self.non_first_pass_count, applied),
            rounds_3plus_rate: rate(self.rounds_3plus_count, applied),
            premerge_issue_rate: rate(self.premerge_issue_count, self.chain_count),
        }
    }
}

struct AcceptSample {
    sig: String,
    elapsed: f64,
    layers: Vec<(String, f64)>,
}

struct RunStageSamples {
    run_to_delivered: Option<f64>,
    capacity_queue: Vec<(String, f64)>,
    agent_work: Option<f64>,
    accepts: Vec<AcceptSample>,
    reviews: Vec<f64>,
    accept_review_parallels: Vec<f64>,
    rework_rounds: Vec<f64>,
    premerge_reused: Vec<f64>,
    premerge_rerun: Vec<f64>,
    suffix_after_work: Option<f64>,
    suffix_after_accept: Option<f64>,
    suffix_after_review: Option<f64>,
    suffix_after_rework: Vec<f64>,
}

fn extract_run_stage_samples(run_events: &[&EventRec]) -> RunStageSamples {
    let mut out = RunStageSamples {
        run_to_delivered: None,
        capacity_queue: Vec::new(),
        agent_work: None,
        accepts: Vec::new(),
        reviews: Vec::new(),
        accept_review_parallels: Vec::new(),
        rework_rounds: Vec::new(),
        premerge_reused: Vec::new(),
        premerge_rerun: Vec::new(),
        suffix_after_work: None,
        suffix_after_accept: None,
        suffix_after_review: None,
        suffix_after_rework: Vec::new(),
    };

    let mut cap_queued: Option<(i64, String)> = None;
    let mut run_start_ts: Option<i64> = None;
    let mut run_delivered_ts: Option<i64> = None;
    let mut round_start_ts: Option<i64> = None;
    let mut round_idx: usize = 0;
    let mut round_lane_held: f64 = 0.0;
    let mut round_lane_queued: f64 = 0.0;
    let mut round_review_started_ts: Option<i64> = None;
    let mut round_accept_ts: Option<i64> = None;
    let mut review_started_by_round: HashMap<i64, i64> = HashMap::new();
    let mut round1_work_end_ts: Option<f64> = None;
    let mut first_accept_ts: Option<i64> = None;
    let mut first_review_verdict_ts: Option<i64> = None;
    let mut pending_rework_ts: Option<i64> = None;
    let mut rework_sent_ts_list: Vec<i64> = Vec::new();

    for ev in run_events {
        match ev.kind.as_str() {
            "queued" => {
                if s(&ev.payload, "scope") == "capacity" {
                    let reason = s(&ev.payload, "reason").trim();
                    let label = if reason.is_empty() { "总路数" } else { reason };
                    cap_queued = Some((ev.ts, label.to_string()));
                }
            }
            "started" => {
                if let Some((q_ts, reason)) = cap_queued.take() {
                    out.capacity_queue
                        .push((reason, (ev.ts - q_ts).max(0) as f64));
                }
                if run_start_ts.is_none() {
                    run_start_ts = Some(ev.ts);
                }
                if round_idx == 0 {
                    round_idx = 1;
                    round_start_ts = Some(ev.ts);
                }
            }
            "lane_acquired" => {
                let q = ev.payload["queuedSeconds"].as_f64().unwrap_or(0.0).max(0.0);
                round_lane_queued += q;
            }
            "lane_released" => {
                let h = ev.payload["heldSeconds"].as_f64().unwrap_or(0.0).max(0.0);
                round_lane_held += h;
            }
            "review_started" => {
                let rnum = n(&ev.payload, "round").max(1);
                review_started_by_round.insert(rnum, ev.ts);
                round_review_started_ts = Some(ev.ts);
            }
            "accept" => {
                if b(&ev.payload, "notExecuted") {
                    round_lane_held = 0.0;
                    round_lane_queued = 0.0;
                    continue;
                }
                let explicit_elapsed = ev.payload["elapsedSeconds"]
                    .as_f64()
                    .filter(|v| v.is_finite() && *v >= 0.0);
                let elapsed = explicit_elapsed
                    .or_else(|| (round_lane_held > 0.0).then_some(round_lane_held));
                let queued = ev.payload["queuedSeconds"]
                    .as_f64()
                    .filter(|v| v.is_finite() && *v >= 0.0)
                    .unwrap_or(round_lane_queued);

                let mut layers_vec = Vec::new();
                if let Some(ls) = ev.payload["layerSeconds"].as_object() {
                    for (lname, lv) in ls {
                        if let Some(secs) = lv.as_f64().filter(|v| v.is_finite() && *v >= 0.0) {
                            layers_vec.push((lname.clone(), secs));
                        }
                    }
                }
                if let Some(el) = elapsed {
                    let sig = accept_signature(
                        b(&ev.payload, "tiered"),
                        b(&ev.payload, "failClosed"),
                        &strings(&ev.payload["ranLayers"]),
                    );
                    out.accepts.push(AcceptSample {
                        sig,
                        elapsed: el,
                        layers: layers_vec,
                    });
                }

                if round_idx == 1 && round1_work_end_ts.is_none() {
                    if let Some(rs) = round_start_ts {
                        let work_opt = if let Some(el) = elapsed {
                            let raw = ((ev.ts - rs) as f64 - el - queued).max(0.0);
                            if let Some(rev_st) = round_review_started_ts {
                                Some(raw.min((rev_st - rs).max(0) as f64))
                            } else {
                                Some(raw)
                            }
                        } else {
                            round_review_started_ts.map(|rev_st| (rev_st - rs).max(0) as f64)
                        };
                        if let Some(work) = work_opt {
                            out.agent_work = Some(work);
                            round1_work_end_ts = Some(rs as f64 + work);
                        }
                    }
                }

                if first_accept_ts.is_none() {
                    first_accept_ts = Some(ev.ts);
                }
                if round_review_started_ts.is_some() {
                    round_accept_ts = Some(ev.ts);
                }
                round_lane_held = 0.0;
                round_lane_queued = 0.0;
            }
            "review_verdict" => {
                let rnum = n(&ev.payload, "round").max(1);
                if let Some(rev_st) = review_started_by_round
                    .remove(&rnum)
                    .or(round_review_started_ts)
                {
                    out.reviews.push((ev.ts - rev_st).max(0) as f64);
                    if let Some(acc_ts) = round_accept_ts {
                        out.accept_review_parallels
                            .push((acc_ts.max(ev.ts) - rev_st).max(0) as f64);
                    }
                }
                if first_review_verdict_ts.is_none() {
                    first_review_verdict_ts = Some(ev.ts);
                }
            }
            "rework_sent" => {
                if let Some(prev_rw) = pending_rework_ts {
                    out.rework_rounds.push((ev.ts - prev_rw).max(0) as f64);
                }
                pending_rework_ts = Some(ev.ts);
                rework_sent_ts_list.push(ev.ts);
                round_idx += 1;
                round_start_ts = Some(ev.ts);
                round_review_started_ts = None;
                round_accept_ts = None;
                round_lane_held = 0.0;
                round_lane_queued = 0.0;
            }
            "delivered" => {
                run_delivered_ts = Some(ev.ts);
                if let Some(prev_rw) = pending_rework_ts.take() {
                    out.rework_rounds.push((ev.ts - prev_rw).max(0) as f64);
                }
            }
            "premerge_ready" => {
                if let Some(el) = ev.payload["elapsedSeconds"]
                    .as_f64()
                    .filter(|v| v.is_finite() && *v >= 0.0)
                {
                    if b(&ev.payload, "reusedAccept") {
                        out.premerge_reused.push(el);
                    } else {
                        out.premerge_rerun.push(el);
                    }
                }
            }
            _ => {}
        }
    }

    if let (Some(st), Some(del)) = (run_start_ts, run_delivered_ts) {
        let dur = (del - st).max(0) as f64;
        out.run_to_delivered = Some(dur);
        if let Some(we) = round1_work_end_ts {
            out.suffix_after_work = Some((del as f64 - we).max(0.0));
        } else {
            out.agent_work = Some(dur);
            out.suffix_after_work = Some(0.0);
        }
        if let Some(fa) = first_accept_ts {
            out.suffix_after_accept = Some((del - fa).max(0) as f64);
        }
        if let Some(fr) = first_review_verdict_ts {
            out.suffix_after_review = Some((del - fr).max(0) as f64);
        }
        for rw_ts in rework_sent_ts_list {
            out.suffix_after_rework.push((del - rw_ts).max(0) as f64);
        }
    }

    out
}

fn push_run_samples(acc: &mut BucketAcc, s: &RunStageSamples) {
    if let Some(v) = s.run_to_delivered {
        acc.run_to_delivered.push(v);
    }
    for (_, q) in &s.capacity_queue {
        acc.capacity_queue.push(*q);
    }
    if let Some(w) = s.agent_work {
        acc.agent_work.push(w);
    }
    for a in &s.accepts {
        acc.accept.push(a.elapsed);
    }
    acc.review.extend_from_slice(&s.reviews);
    acc.accept_review_parallel
        .extend_from_slice(&s.accept_review_parallels);
    acc.rework_round.extend_from_slice(&s.rework_rounds);
    acc.premerge_reused.extend_from_slice(&s.premerge_reused);
    acc.premerge_rerun.extend_from_slice(&s.premerge_rerun);
    if let Some(v) = s.suffix_after_work {
        acc.suffix_after_work.push(v);
    }
    if let Some(v) = s.suffix_after_accept {
        acc.suffix_after_accept.push(v);
    }
    if let Some(v) = s.suffix_after_review {
        acc.suffix_after_review.push(v);
    }
    acc.suffix_after_rework
        .extend_from_slice(&s.suffix_after_rework);
}

fn formula_from_chains(write_chains: &[&ChainInfo], now_ts: i64) -> RepoFormulaParams {
    let mut all_write = Vec::new();
    let mut t_crit = Vec::new();
    let mut merge_wait = Vec::new();
    let mut auto_rw = 0usize;
    let mut non_fp = 0usize;
    let mut r3 = 0usize;
    let mut pm_issue = 0usize;

    for ch in write_chains {
        if ch.has_premerge_conflict || ch.has_premerge_failed {
            pm_issue += 1;
        }
        if let Some(wall) = ch.wall_clock_secs() {
            all_write.push(wall);
            if ch.rework_sent_count > 0 {
                auto_rw += 1;
            }
            if ch.is_non_first_pass() {
                non_fp += 1;
                t_crit.push(wall);
            }
            if ch.is_three_plus_rounds() {
                r3 += 1;
            }
            if let Some(mw) = ch.merge_wait_secs() {
                merge_wait.push(mw);
            }
        }
    }

    let applied = all_write.len();
    let total = write_chains.len();
    let rate = |num: usize, den: usize| -> Option<f64> {
        if den == 0 {
            None
        } else {
            Some(num as f64 / den as f64)
        }
    };
    let w_dist = DistStats::from_secs(all_write);
    let tc_dist = DistStats::from_secs(t_crit);
    let mw_dist = DistStats::from_secs(merge_wait);
    let conc = compute_concurrency(write_chains, applied, now_ts);

    RepoFormulaParams {
        write_applied_count: applied,
        t_chain_p50_sec: w_dist.p50_sec,
        t_chain_p75_sec: w_dist.p75_sec,
        t_crit_p50_sec: tc_dist.p50_sec,
        t_crit_p75_sec: tc_dist.p75_sec,
        p_eff_mean: conc.mean,
        p_eff_p50: conc.p50,
        p_eff_p75: conc.p75,
        t_merge_p50_sec: mw_dist.p50_sec,
        t_merge_p75_sec: mw_dist.p75_sec,
        auto_rework_rate: rate(auto_rw, applied),
        non_first_pass_rate: rate(non_fp, applied),
        rounds_3plus_rate: rate(r3, applied),
        premerge_issue_rate: rate(pm_issue, total),
    }
}

/// Build the unified ETA model from parsed events over the trailing `window_days` window.
/// Pure in-memory computation (usable both for `<state>/eta-model.json` and Chain 2 `--backtest`).
pub fn build_eta_model_from_events(
    state: &Path,
    events: &[EventRec],
    now_ts: i64,
    events_seq: u64,
    window_days: i64,
) -> EtaModel {
    let cutoff_ts = if window_days > 0 {
        Some(now_ts - window_days * 86_400)
    } else {
        None
    };
    let recent_lane_cutoff = now_ts - RECENT_LANE_WINDOW_SECS;

    let mut by_repo: BTreeMap<String, Vec<(usize, &EventRec)>> = BTreeMap::new();
    let mut recent_queued_secs = 0.0_f64;
    let mut recent_held_secs = 0.0_f64;
    let mut recent_wait_all: Vec<f64> = Vec::new();
    let mut recent_wait_nonzero: Vec<f64> = Vec::new();

    for (idx, ev) in events.iter().enumerate() {
        by_repo.entry(ev.repo.clone()).or_default().push((idx, ev));
        if ev.ts >= recent_lane_cutoff {
            match ev.kind.as_str() {
                "lane_acquired" => {
                    let q = ev.payload["queuedSeconds"].as_f64().unwrap_or(0.0).max(0.0);
                    recent_queued_secs += q;
                    recent_wait_all.push(q);
                    if q > 0.0 {
                        recent_wait_nonzero.push(q);
                    }
                }
                "lane_released" => {
                    let h = ev.payload["heldSeconds"].as_f64().unwrap_or(0.0).max(0.0);
                    recent_held_secs += h;
                }
                _ => {}
            }
        }
    }

    let recent_denom = recent_queued_secs + recent_held_secs;
    let recent_lane_2h = RecentLaneStats {
        window_secs: RECENT_LANE_WINDOW_SECS,
        queued_secs: recent_queued_secs,
        held_secs: recent_held_secs,
        queue_share: if recent_denom > 0.0 {
            recent_queued_secs / recent_denom
        } else {
            0.0
        },
        wait_p50_sec: SampleDist::from_secs(recent_wait_all.clone()).p50_sec,
        wait_nonzero_p50_sec: SampleDist::from_secs(recent_wait_nonzero).p50_sec,
        acquire_count: recent_wait_all.len(),
    };

    let mut global_mc: BTreeMap<String, BucketAcc> = BTreeMap::new();
    let mut global_mode: BTreeMap<String, BucketAcc> = BTreeMap::new();
    let mut global_accept_sig: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut global_accept_layers: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut global_lane_held: Vec<f64> = Vec::new();
    let mut global_lane_queue: Vec<f64> = Vec::new();
    let mut global_cap_queue: Vec<f64> = Vec::new();
    let mut global_cap_by_reason: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut global_pm_reused: Vec<f64> = Vec::new();
    let mut global_pm_rerun: Vec<f64> = Vec::new();
    let mut global_merge_exec: Vec<f64> = Vec::new();
    let mut all_window_write_chains: Vec<ChainInfo> = Vec::new();
    let mut repos_out: BTreeMap<String, RepoEtaModel> = BTreeMap::new();

    for (repo, mut indexed_events) in by_repo {
        indexed_events.sort_by_key(|(idx, ev)| (ev.ts, *idx));
        let ordered_owned: Vec<EventRec> = indexed_events
            .iter()
            .map(|(_, ev)| (*ev).clone())
            .collect();

        // Build parent map & chains over full repo history so reply chains stay intact.
        let mut parent_of: HashMap<String, String> = HashMap::new();
        for ev in &ordered_owned {
            if ev.kind == "started" {
                let p = s(&ev.payload, "parent").trim();
                if !p.is_empty() && p != ev.run {
                    parent_of.insert(ev.run.clone(), p.to_string());
                }
            }
        }
        let chains = build_chains_for_repo(&ordered_owned);

        let mut events_by_run: HashMap<&str, Vec<&EventRec>> = HashMap::new();
        let mut repo_lane_held: Vec<f64> = Vec::new();
        let mut repo_lane_queue: Vec<f64> = Vec::new();
        let mut repo_pm_reused: Vec<f64> = Vec::new();
        let mut repo_pm_rerun: Vec<f64> = Vec::new();
        let mut repo_merge_exec: Vec<f64> = Vec::new();
        let mut merge_started_map: HashMap<String, i64> = HashMap::new();

        for ev in &ordered_owned {
            events_by_run.entry(ev.run.as_str()).or_default().push(ev);
            let in_window = cutoff_ts.is_none_or(|c| ev.ts >= c);
            if in_window {
                match ev.kind.as_str() {
                    "lane_acquired" => {
                        let q = ev.payload["queuedSeconds"].as_f64().unwrap_or(0.0);
                        if q > 0.0 && q.is_finite() {
                            repo_lane_queue.push(q);
                            global_lane_queue.push(q);
                        }
                    }
                    "lane_released" => {
                        let h = ev.payload["heldSeconds"].as_f64().unwrap_or(0.0);
                        if h > 0.0 && h.is_finite() {
                            repo_lane_held.push(h);
                            global_lane_held.push(h);
                        }
                    }
                    "premerge_ready" => {
                        if let Some(el) = ev.payload["elapsedSeconds"]
                            .as_f64()
                            .filter(|v| v.is_finite() && *v >= 0.0)
                        {
                            if b(&ev.payload, "reusedAccept") {
                                repo_pm_reused.push(el);
                                global_pm_reused.push(el);
                            } else {
                                repo_pm_rerun.push(el);
                                global_pm_rerun.push(el);
                            }
                        }
                    }
                    "merge_started" => {
                        let key = {
                            let mr = s(&ev.payload, "mergeRun");
                            if !mr.is_empty() {
                                mr.to_string()
                            } else {
                                ev.run.clone()
                            }
                        };
                        merge_started_map.insert(key, ev.ts);
                    }
                    "merge_done" => {
                        let key = {
                            let mr = s(&ev.payload, "mergeRun");
                            if !mr.is_empty() {
                                mr.to_string()
                            } else {
                                ev.run.clone()
                            }
                        };
                        if let Some(st) = merge_started_map.remove(&key) {
                            let dur = (ev.ts - st).max(0) as f64;
                            repo_merge_exec.push(dur);
                            global_merge_exec.push(dur);
                        }
                    }
                    _ => {}
                }
            }
        }

        let mut repo_mc: BTreeMap<String, BucketAcc> = BTreeMap::new();
        let mut repo_mode: BTreeMap<String, BucketAcc> = BTreeMap::new();
        let mut repo_accept_sig: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut repo_accept_layers: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut repo_write_chains: Vec<&ChainInfo> = Vec::new();

        for ch in chains.values() {
            if !ch.has_started {
                continue;
            }
            let Some(st) = ch.first_started_ts else {
                continue;
            };
            if let Some(cut) = cutoff_ts {
                if st < cut {
                    continue;
                }
            }
            if ch.is_write {
                repo_write_chains.push(ch);
                all_window_write_chains.push(ch.clone());
            }
            let (mode, class) = ch.mode_and_class();
            let mc_key = mode_class_key(&mode, &class);

            repo_mc.entry(mc_key.clone()).or_default().push_chain(ch);
            repo_mode.entry(mode.clone()).or_default().push_chain(ch);
            global_mc.entry(mc_key.clone()).or_default().push_chain(ch);
            global_mode.entry(mode.clone()).or_default().push_chain(ch);

            for run_id in ch.runs.keys() {
                let Some(revs) = events_by_run.get(run_id.as_str()) else {
                    continue;
                };
                let samples = extract_run_stage_samples(revs);
                push_run_samples(repo_mc.entry(mc_key.clone()).or_default(), &samples);
                push_run_samples(repo_mode.entry(mode.clone()).or_default(), &samples);
                push_run_samples(global_mc.entry(mc_key.clone()).or_default(), &samples);
                push_run_samples(global_mode.entry(mode.clone()).or_default(), &samples);

                for (reason, q) in &samples.capacity_queue {
                    global_cap_queue.push(*q);
                    global_cap_by_reason
                        .entry(reason.clone())
                        .or_default()
                        .push(*q);
                }
                for a in &samples.accepts {
                    repo_accept_sig
                        .entry(a.sig.clone())
                        .or_default()
                        .push(a.elapsed);
                    global_accept_sig
                        .entry(a.sig.clone())
                        .or_default()
                        .push(a.elapsed);
                    for (lname, lsec) in &a.layers {
                        repo_accept_layers
                            .entry(lname.clone())
                            .or_default()
                            .push(*lsec);
                        global_accept_layers
                            .entry(lname.clone())
                            .or_default()
                            .push(*lsec);
                    }
                }
            }
        }

        let formula = formula_from_chains(&repo_write_chains, now_ts);
        let (repo_slug, _) = resolve_speed_file(state, &repo);
        let repo_model = RepoEtaModel {
            repo_name: short_repo_name(&repo),
            repo_slug,
            repo: repo.clone(),
            by_mode_class: repo_mc
                .into_iter()
                .map(|(k, a)| (k, a.finish()))
                .collect(),
            by_mode: repo_mode
                .into_iter()
                .map(|(k, a)| (k, a.finish()))
                .collect(),
            formula,
            accept_by_sig: repo_accept_sig
                .into_iter()
                .map(|(k, v)| (k, SampleDist::from_secs(v)))
                .collect(),
            accept_layers: repo_accept_layers
                .into_iter()
                .map(|(k, v)| (k, SampleDist::from_secs(v)))
                .collect(),
            lane_held: SampleDist::from_secs(repo_lane_held),
            lane_queue: SampleDist::from_secs(repo_lane_queue),
            premerge_reused: SampleDist::from_secs(repo_pm_reused),
            premerge_rerun: SampleDist::from_secs(repo_pm_rerun),
            merge_exec: SampleDist::from_secs(repo_merge_exec),
        };
        repos_out.insert(repo, repo_model);
    }

    let global_write_refs: Vec<&ChainInfo> = all_window_write_chains.iter().collect();
    let global_formula = formula_from_chains(&global_write_refs, now_ts);

    EtaModel {
        version: ETA_MODEL_VERSION,
        generated_at: format_epoch(now_ts, false, "%Y-%m-%dT%H:%M:%SZ"),
        generated_epoch: now_ts,
        events_seq,
        window_days,
        repos: repos_out,
        global: GlobalEtaModel {
            by_mode_class: global_mc
                .into_iter()
                .map(|(k, a)| (k, a.finish()))
                .collect(),
            by_mode: global_mode
                .into_iter()
                .map(|(k, a)| (k, a.finish()))
                .collect(),
            formula: global_formula,
            accept_by_sig: global_accept_sig
                .into_iter()
                .map(|(k, v)| (k, SampleDist::from_secs(v)))
                .collect(),
            accept_layers: global_accept_layers
                .into_iter()
                .map(|(k, v)| (k, SampleDist::from_secs(v)))
                .collect(),
            lane_held: SampleDist::from_secs(global_lane_held),
            lane_queue: SampleDist::from_secs(global_lane_queue),
            capacity_queue: SampleDist::from_secs(global_cap_queue),
            capacity_queue_by_reason: global_cap_by_reason
                .into_iter()
                .map(|(k, v)| (k, SampleDist::from_secs(v)))
                .collect(),
            premerge_reused: SampleDist::from_secs(global_pm_reused),
            premerge_rerun: SampleDist::from_secs(global_pm_rerun),
            merge_exec: SampleDist::from_secs(global_merge_exec),
            recent_lane_2h,
        },
        priors: ColdStartPriors::default(),
    }
}

/// Build the unified ETA model directly from `<state>/events.jsonl`.
#[allow(dead_code)] // Consumed by eta.rs in Chain 2.
pub fn build_eta_model_in(state: &Path, now_ts: i64) -> EtaModel {
    let raw = events::read_since_in(state, 0);
    let parsed = parse_events(raw);
    let latest_seq = parsed
        .iter()
        .map(|e| e.seq)
        .max()
        .unwrap_or(0)
        .max(events::latest_seq_in(state));
    build_eta_model_from_events(state, &parsed, now_ts, latest_seq, ETA_MODEL_WINDOW_DAYS)
}

/// Atomically persist `<state>/eta-model.json` (lock-free, deletable & rebuildable).
pub fn write_eta_model_in(state: &Path, model: &EtaModel) -> Res<PathBuf> {
    let path = eta_model_path(state);
    write_json(&path, &model.to_json())?;
    Ok(path)
}

/// Lock-free read of `<state>/eta-model.json`; returns `None` if missing, corrupt, or version-mismatched.
#[allow(dead_code)] // Consumed by eta.rs in Chain 2.
pub fn load_eta_model_in(state: &Path) -> Option<EtaModel> {
    let path = eta_model_path(state);
    let text = fs::read_to_string(path).ok()?;
    let val: Value = serde_json::from_str(&text).ok()?;
    EtaModel::from_json(&val)
}

/// Load cached `<state>/eta-model.json` or lazily rebuild from `events.jsonl` when missing,
/// corrupt, or older than 15 minutes with `events.seq` advanced.
#[allow(dead_code)] // Consumed by eta.rs in Chain 2.
pub fn load_or_refresh_eta_model_in(state: &Path, now_ts: i64, allow_write: bool) -> EtaModel {
    let cur_seq = events::latest_seq_in(state);
    if let Some(cached) = load_eta_model_in(state) {
        let age = now_ts - cached.generated_epoch;
        let stale = age > ETA_MODEL_STALE_SECS && cur_seq > cached.events_seq;
        if !stale {
            return cached;
        }
    }
    let model = build_eta_model_in(state, now_ts);
    if allow_write {
        let _ = write_eta_model_in(state, &model);
    }
    model
}

// ---------------------------------------------------------------------------
// Rolling offline backtest (`delegate speed --backtest`)
// Replays `events.jsonl` chronologically with strict causality: each prediction
// at time `t` uses only samples completed strictly before `t` within the
// trailing 14-day lookback window.
// ---------------------------------------------------------------------------

pub const BACKTEST_P75_HEALTH_LOW: f64 = 0.65;
pub const BACKTEST_P75_HEALTH_HIGH: f64 = 0.85;
pub const BACKTEST_P75_ALERT_LOW: f64 = 0.60;
pub const BACKTEST_P75_ALERT_HIGH: f64 = 0.90;
pub const BACKTEST_P50_BIAS_TARGET: f64 = 0.15;

#[derive(Debug, Clone, Default)]
pub struct BacktestMetrics {
    pub count: usize,
    pub p75_covered: usize,
    pub p75_coverage: Option<f64>,
    pub p75_status: &'static str,
    pub p75_alert: bool,
    pub p50_rel_bias: Option<f64>,
    pub p50_bias_alert: bool,
    pub p50_abs_err_min: Option<f64>,
    pub by_level: BTreeMap<String, usize>,
}

impl BacktestMetrics {
    fn from_obs(obs: &[BacktestObs]) -> Self {
        let count = obs.len();
        if count == 0 {
            return Self {
                p75_status: "insufficient",
                ..Self::default()
            };
        }
        let mut covered = 0usize;
        let mut rel_biases = Vec::with_capacity(count);
        let mut abs_errs = Vec::with_capacity(count);
        let mut by_level: BTreeMap<String, usize> = BTreeMap::new();

        for o in obs {
            if o.actual_sec <= o.pred_p75_sec + 1e-6 {
                covered += 1;
            }
            if o.actual_sec > 0.0 {
                rel_biases.push((o.actual_sec - o.pred_p50_sec) / o.actual_sec);
            }
            abs_errs.push((o.actual_sec - o.pred_p50_sec).abs());
            *by_level.entry(o.level.as_str().to_string()).or_insert(0) += 1;
        }

        rel_biases.sort_by(|a, b| a.total_cmp(b));
        abs_errs.sort_by(|a, b| a.total_cmp(b));

        let cov = covered as f64 / count as f64;
        let (status, p75_alert) = if cov < BACKTEST_P75_ALERT_LOW {
            ("optimistic", true)
        } else if cov > BACKTEST_P75_ALERT_HIGH {
            ("too_wide", true)
        } else if (BACKTEST_P75_HEALTH_LOW..=BACKTEST_P75_HEALTH_HIGH).contains(&cov) {
            ("healthy", false)
        } else {
            ("borderline", false)
        };
        let p50_rel_bias = percentile(&rel_biases, 0.50);
        let p50_bias_alert = p50_rel_bias.is_some_and(|b| b.abs() > BACKTEST_P50_BIAS_TARGET);
        let p50_abs_err_min = percentile(&abs_errs, 0.50).map(sec_to_min);

        Self {
            count,
            p75_covered: covered,
            p75_coverage: Some(cov),
            p75_status: status,
            p75_alert,
            p50_rel_bias,
            p50_bias_alert,
            p50_abs_err_min,
            by_level,
        }
    }

    pub fn status_zh(&self) -> &'static str {
        match self.p75_status {
            "healthy" => "健康",
            "optimistic" => "偏乐观",
            "too_wide" => "偏宽",
            "borderline" => "临界",
            _ => "样本不足",
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "count": self.count,
            "p75Covered": self.p75_covered,
            "p75Coverage": self.p75_coverage.map(round4),
            "p75CoveragePct": self.p75_coverage.map(|v| round2(v * 100.0)),
            "p75Status": self.p75_status,
            "p75Alert": self.p75_alert,
            "p50RelBias": self.p50_rel_bias.map(round4),
            "p50RelBiasPct": self.p50_rel_bias.map(|v| round2(v * 100.0)),
            "p50BiasAlert": self.p50_bias_alert,
            "p50MedianAbsErrMin": self.p50_abs_err_min,
            "byLevel": self.by_level,
        })
    }
}

#[derive(Debug, Clone)]
pub struct RepoBacktestReport {
    pub repo: String,
    pub repo_name: String,
    pub start: BacktestMetrics,
    pub mid: BacktestMetrics,
    pub applied_chain: BacktestMetrics,
}

impl RepoBacktestReport {
    pub fn to_json(&self) -> Value {
        json!({
            "repo": self.repo,
            "repoName": self.repo_name,
            "start": self.start.to_json(),
            "mid": self.mid.to_json(),
            "appliedChain": self.applied_chain.to_json(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct BacktestReport {
    pub at: String,
    pub window: String,
    pub lookback_days: i64,
    pub overall_start: BacktestMetrics,
    pub overall_mid: BacktestMetrics,
    pub overall_applied: BacktestMetrics,
    pub repos: Vec<RepoBacktestReport>,
}

impl BacktestReport {
    pub fn to_json(&self) -> Value {
        let repos: Vec<Value> = self.repos.iter().map(|r| r.to_json()).collect();
        json!({
            "at": self.at,
            "window": self.window,
            "lookbackDays": self.lookback_days,
            "healthBand": {
                "p75Low": BACKTEST_P75_HEALTH_LOW,
                "p75High": BACKTEST_P75_HEALTH_HIGH,
                "p75AlertLow": BACKTEST_P75_ALERT_LOW,
                "p75AlertHigh": BACKTEST_P75_ALERT_HIGH,
                "p50BiasTarget": BACKTEST_P50_BIAS_TARGET,
            },
            "start": self.overall_start.to_json(),
            "mid": self.overall_mid.to_json(),
            "appliedChain": self.overall_applied.to_json(),
            "repos": repos,
        })
    }
}

#[derive(Debug, Clone)]
struct BacktestObs {
    actual_sec: f64,
    pred_p50_sec: f64,
    pred_p75_sec: f64,
    level: FallbackLevel,
}

#[derive(Debug, Clone)]
struct ExtractedWriteRun {
    repo: String,
    repo_name: String,
    class: String,
    started_ts: i64,
    delivered_ts: i64,
    duration_sec: f64,
    agent_work_sec: Option<f64>,
    suffix_after_work_sec: Option<f64>,
}

#[derive(Debug, Clone)]
struct ExtractedAppliedChain {
    repo: String,
    repo_name: String,
    class: String,
    started_ts: i64,
    applied_ts: i64,
    wall_sec: f64,
}

fn resolve_rolling_dist<F>(
    history: &[&ExtractedWriteRun],
    repo: &str,
    class: &str,
    extract: F,
    prior: &SampleDist,
) -> (SampleDist, FallbackLevel)
where
    F: Fn(&ExtractedWriteRun) -> Option<f64>,
{
    let mut l1 = Vec::new();
    let mut l2 = Vec::new();
    let mut l3 = Vec::new();
    let mut l4 = Vec::new();
    for h in history {
        let Some(v) = extract(h) else {
            continue;
        };
        if !v.is_finite() || v < 0.0 {
            continue;
        }
        l4.push(v);
        if h.class == class {
            l3.push(v);
        }
        if h.repo == repo {
            l2.push(v);
            if h.class == class {
                l1.push(v);
            }
        }
    }
    if l1.len() >= MIN_BUCKET_N {
        (SampleDist::from_secs(l1), FallbackLevel::RepoModeClass)
    } else if l2.len() >= MIN_BUCKET_N {
        (SampleDist::from_secs(l2), FallbackLevel::RepoMode)
    } else if l3.len() >= MIN_BUCKET_N {
        (SampleDist::from_secs(l3), FallbackLevel::GlobalModeClass)
    } else if l4.len() >= MIN_GLOBAL_MODE_N {
        (SampleDist::from_secs(l4), FallbackLevel::GlobalMode)
    } else {
        (prior.clone(), FallbackLevel::Prior)
    }
}

fn resolve_rolling_applied_dist(
    history: &[&ExtractedAppliedChain],
    repo: &str,
    class: &str,
    prior: &SampleDist,
) -> (SampleDist, FallbackLevel) {
    let mut l1 = Vec::new();
    let mut l2 = Vec::new();
    let mut l3 = Vec::new();
    let mut l4 = Vec::new();
    for h in history {
        let v = h.wall_sec;
        if !v.is_finite() || v < 0.0 {
            continue;
        }
        l4.push(v);
        if h.class == class {
            l3.push(v);
        }
        if h.repo == repo {
            l2.push(v);
            if h.class == class {
                l1.push(v);
            }
        }
    }
    if l1.len() >= MIN_BUCKET_N {
        (SampleDist::from_secs(l1), FallbackLevel::RepoModeClass)
    } else if l2.len() >= MIN_BUCKET_N {
        (SampleDist::from_secs(l2), FallbackLevel::RepoMode)
    } else if l3.len() >= MIN_BUCKET_N {
        (SampleDist::from_secs(l3), FallbackLevel::GlobalModeClass)
    } else if l4.len() >= MIN_GLOBAL_MODE_N {
        (SampleDist::from_secs(l4), FallbackLevel::GlobalMode)
    } else {
        (prior.clone(), FallbackLevel::Prior)
    }
}

/// Pure in-memory rolling offline backtest over parsed `events`.
/// Evaluates start-of-run, mid-run (50% of agent work via conditional residual + suffix RSS),
/// and applied-chain predictions with strict temporal causality.
pub fn backtest_from_events(
    events: &[EventRec],
    now_ts: i64,
    since_str: &str,
    repo_filter: Option<&str>,
) -> Res<BacktestReport> {
    let (eval_cutoff, window_label) = if since_str.eq_ignore_ascii_case("all") {
        (None, "all".to_string())
    } else {
        let secs = seconds(since_str)? as i64;
        if secs <= 0 {
            return Err(format!("--since must be positive, got {since_str:?}"));
        }
        (Some(now_ts - secs), since_str.to_string())
    };
    let lookback_secs = ETA_MODEL_WINDOW_DAYS * 86_400;
    let priors = ColdStartPriors::default();

    let mut by_repo: BTreeMap<String, Vec<(usize, &EventRec)>> = BTreeMap::new();
    for (idx, ev) in events.iter().enumerate() {
        by_repo.entry(ev.repo.clone()).or_default().push((idx, ev));
    }

    let mut all_runs: Vec<ExtractedWriteRun> = Vec::new();
    let mut all_chains: Vec<ExtractedAppliedChain> = Vec::new();

    for (repo, mut indexed) in by_repo {
        indexed.sort_by_key(|(idx, ev)| (ev.ts, *idx));
        let ordered: Vec<EventRec> = indexed.iter().map(|(_, ev)| (*ev).clone()).collect();
        let chains = build_chains_for_repo(&ordered);
        let repo_name = short_repo_name(&repo);

        let mut events_by_run: HashMap<&str, Vec<&EventRec>> = HashMap::new();
        for ev in &ordered {
            events_by_run.entry(ev.run.as_str()).or_default().push(ev);
        }

        for ch in chains.values() {
            if !ch.has_started || !ch.is_write {
                continue;
            }
            let (_, norm_class) = ch.mode_and_class();
            if let (Some(st), Some(ap)) = (ch.first_started_ts, ch.first_applied_ts) {
                let wall = (ap - st).max(0) as f64;
                all_chains.push(ExtractedAppliedChain {
                    repo: repo.clone(),
                    repo_name: repo_name.clone(),
                    class: norm_class.clone(),
                    started_ts: st,
                    applied_ts: ap,
                    wall_sec: wall,
                });
            }
            for (run_id, rt) in &ch.runs {
                let Some(st) = rt.started_ts else {
                    continue;
                };
                let Some(revs) = events_by_run.get(run_id.as_str()) else {
                    continue;
                };
                let Some(del_ts) = revs
                    .iter()
                    .rev()
                    .find(|e| e.kind == "delivered")
                    .map(|e| e.ts)
                else {
                    continue;
                };
                let samples = extract_run_stage_samples(revs);
                let dur = (del_ts - st).max(0) as f64;
                all_runs.push(ExtractedWriteRun {
                    repo: repo.clone(),
                    repo_name: repo_name.clone(),
                    class: norm_class.clone(),
                    started_ts: st,
                    delivered_ts: del_ts,
                    duration_sec: dur,
                    agent_work_sec: samples.agent_work,
                    suffix_after_work_sec: samples.suffix_after_work,
                });
            }
        }
    }

    // Sort chronologically for rolling causal queries.
    let mut runs_by_delivered: Vec<usize> = (0..all_runs.len()).collect();
    runs_by_delivered.sort_by_key(|&i| (all_runs[i].delivered_ts, all_runs[i].started_ts));

    let mut targets_start: Vec<usize> = (0..all_runs.len())
        .filter(|&i| {
            let r = &all_runs[i];
            eval_cutoff.is_none_or(|c| r.started_ts >= c)
                && repo_filter
                    .is_none_or(|rf| repo_matches_filter(&r.repo, &r.repo_name, rf))
        })
        .collect();
    targets_start.sort_by_key(|&i| (all_runs[i].started_ts, all_runs[i].delivered_ts));

    let mut overall_start_obs: Vec<BacktestObs> = Vec::new();
    let mut overall_mid_obs: Vec<BacktestObs> = Vec::new();
    let mut repo_start_obs: BTreeMap<String, Vec<BacktestObs>> = BTreeMap::new();
    let mut repo_mid_obs: BTreeMap<String, Vec<BacktestObs>> = BTreeMap::new();
    let mut repo_applied_obs: BTreeMap<String, Vec<BacktestObs>> = BTreeMap::new();

    // Start-of-run checkpoint: strictly delivered_ts < target.started_ts and started_ts >= t - 14d
    let mut cursor = 0usize;
    let mut active_hist: Vec<&ExtractedWriteRun> = Vec::new();
    for &ti in &targets_start {
        let target = &all_runs[ti];
        let t_pred = target.started_ts;
        while cursor < runs_by_delivered.len() {
            let cand = &all_runs[runs_by_delivered[cursor]];
            if cand.delivered_ts < t_pred {
                active_hist.push(cand);
                cursor += 1;
            } else {
                break;
            }
        }
        let win_start = t_pred - lookback_secs;
        let window_hist: Vec<&ExtractedWriteRun> = active_hist
            .iter()
            .copied()
            .filter(|h| h.started_ts >= win_start)
            .collect();
        let (dist, level) = resolve_rolling_dist(
            &window_hist,
            &target.repo,
            &target.class,
            |r| Some(r.duration_sec),
            &priors.write.run_to_delivered,
        );
        if let (Some(p50), Some(p75)) = (dist.p50_sec, dist.p75_sec) {
            let obs = BacktestObs {
                actual_sec: target.duration_sec,
                pred_p50_sec: p50,
                pred_p75_sec: p75.max(p50),
                level,
            };
            overall_start_obs.push(obs.clone());
            repo_start_obs
                .entry(target.repo.clone())
                .or_default()
                .push(obs);
        }
    }

    // Mid-run checkpoint: at 50% of round-1 agent_work (or 50% of duration when legacy)
    let mut targets_mid: Vec<(i64, f64, f64, usize)> = targets_start
        .iter()
        .filter_map(|&ti| {
            let r = &all_runs[ti];
            let work_dur = r.agent_work_sec.unwrap_or(r.duration_sec);
            if r.duration_sec < 2.0 || work_dur < 2.0 {
                return None;
            }
            let elapsed = (work_dur * 0.5).floor().max(1.0);
            let mid_ts = r.started_ts + elapsed as i64;
            let actual_rem = (r.delivered_ts - mid_ts).max(0) as f64;
            Some((mid_ts, elapsed, actual_rem, ti))
        })
        .collect();
    targets_mid.sort_by_key(|&(mid_ts, _, _, ti)| (mid_ts, all_runs[ti].delivered_ts));

    let mut mid_cursor = 0usize;
    let mut mid_hist: Vec<&ExtractedWriteRun> = Vec::new();
    for (mid_ts, elapsed, actual_rem, ti) in targets_mid {
        let target = &all_runs[ti];
        while mid_cursor < runs_by_delivered.len() {
            let cand = &all_runs[runs_by_delivered[mid_cursor]];
            if cand.delivered_ts < mid_ts {
                mid_hist.push(cand);
                mid_cursor += 1;
            } else {
                break;
            }
        }
        let win_start = mid_ts - lookback_secs;
        let window_hist: Vec<&ExtractedWriteRun> = mid_hist
            .iter()
            .copied()
            .filter(|h| h.started_ts >= win_start)
            .collect();
        let (work_dist, level) = resolve_rolling_dist(
            &window_hist,
            &target.repo,
            &target.class,
            |r| r.agent_work_sec,
            &priors.write.agent_work,
        );
        let (suf_dist, _) = resolve_rolling_dist(
            &window_hist,
            &target.repo,
            &target.class,
            |r| r.suffix_after_work_sec,
            &priors.write.suffix_after_work,
        );
        if let Some((w50, w75, _)) = work_dist.conditional_residual(elapsed) {
            let s50 = suf_dist.p50_sec.unwrap_or(0.0).max(0.0);
            let s75 = suf_dist.p75_sec.unwrap_or(s50).max(s50);
            let pred_p50 = round2(w50 + s50);
            let spread = (w75 - w50).max(0.0).hypot((s75 - s50).max(0.0));
            let pred_p75 = round2(pred_p50 + spread);
            let obs = BacktestObs {
                actual_sec: actual_rem,
                pred_p50_sec: pred_p50,
                pred_p75_sec: pred_p75,
                level,
            };
            overall_mid_obs.push(obs.clone());
            repo_mid_obs
                .entry(target.repo.clone())
                .or_default()
                .push(obs);
        }
    }

    // Applied-chain checkpoint: started -> applied
    let mut chains_by_applied: Vec<usize> = (0..all_chains.len()).collect();
    chains_by_applied.sort_by_key(|&i| (all_chains[i].applied_ts, all_chains[i].started_ts));

    let mut targets_applied: Vec<usize> = (0..all_chains.len())
        .filter(|&i| {
            let c = &all_chains[i];
            eval_cutoff.is_none_or(|cut| c.started_ts >= cut)
                && repo_filter
                    .is_none_or(|rf| repo_matches_filter(&c.repo, &c.repo_name, rf))
        })
        .collect();
    targets_applied.sort_by_key(|&i| (all_chains[i].started_ts, all_chains[i].applied_ts));

    let mut overall_applied_obs: Vec<BacktestObs> = Vec::new();
    let mut ap_cursor = 0usize;
    let mut ap_hist: Vec<&ExtractedAppliedChain> = Vec::new();
    for &ci in &targets_applied {
        let target = &all_chains[ci];
        let t_pred = target.started_ts;
        while ap_cursor < chains_by_applied.len() {
            let cand = &all_chains[chains_by_applied[ap_cursor]];
            if cand.applied_ts < t_pred {
                ap_hist.push(cand);
                ap_cursor += 1;
            } else {
                break;
            }
        }
        let win_start = t_pred - lookback_secs;
        let window_hist: Vec<&ExtractedAppliedChain> = ap_hist
            .iter()
            .copied()
            .filter(|h| h.started_ts >= win_start)
            .collect();
        let (dist, level) = resolve_rolling_applied_dist(
            &window_hist,
            &target.repo,
            &target.class,
            &priors.write.chain_to_applied,
        );
        if let (Some(p50), Some(p75)) = (dist.p50_sec, dist.p75_sec) {
            let obs = BacktestObs {
                actual_sec: target.wall_sec,
                pred_p50_sec: p50,
                pred_p75_sec: p75.max(p50),
                level,
            };
            overall_applied_obs.push(obs.clone());
            repo_applied_obs
                .entry(target.repo.clone())
                .or_default()
                .push(obs);
        }
    }

    let mut repo_keys: BTreeMap<String, ()> = BTreeMap::new();
    for k in repo_start_obs
        .keys()
        .chain(repo_mid_obs.keys())
        .chain(repo_applied_obs.keys())
    {
        repo_keys.insert(k.clone(), ());
    }
    let mut repos: Vec<RepoBacktestReport> = repo_keys
        .into_keys()
        .map(|repo| {
            let s_obs = repo_start_obs.remove(&repo).unwrap_or_default();
            let m_obs = repo_mid_obs.remove(&repo).unwrap_or_default();
            let a_obs = repo_applied_obs.remove(&repo).unwrap_or_default();
            RepoBacktestReport {
                repo_name: short_repo_name(&repo),
                repo,
                start: BacktestMetrics::from_obs(&s_obs),
                mid: BacktestMetrics::from_obs(&m_obs),
                applied_chain: BacktestMetrics::from_obs(&a_obs),
            }
        })
        .collect();
    repos.sort_by(|a, b| {
        b.start
            .count
            .cmp(&a.start.count)
            .then_with(|| a.repo_name.cmp(&b.repo_name))
    });

    Ok(BacktestReport {
        at: format_epoch(now_ts, false, "%Y-%m-%dT%H:%M:%SZ"),
        window: window_label,
        lookback_days: ETA_MODEL_WINDOW_DAYS,
        overall_start: BacktestMetrics::from_obs(&overall_start_obs),
        overall_mid: BacktestMetrics::from_obs(&overall_mid_obs),
        overall_applied: BacktestMetrics::from_obs(&overall_applied_obs),
        repos,
    })
}

pub fn backtest_in(
    state: &Path,
    now_ts: i64,
    since_str: &str,
    repo_filter: Option<&str>,
) -> Res<BacktestReport> {
    let raw = events::read_since_in(state, 0);
    let parsed = parse_events(raw);
    backtest_from_events(&parsed, now_ts, since_str, repo_filter)
}

fn fmt_signed_pct(v: Option<f64>) -> String {
    match v {
        Some(r) => {
            let pct = r * 100.0;
            if pct >= 0.0 {
                format!("+{pct:.1}%")
            } else {
                format!("{pct:.1}%")
            }
        }
        None => "-".to_string(),
    }
}

fn fmt_bt_metric_brief(m: &BacktestMetrics) -> String {
    if m.count == 0 {
        return "无样本".to_string();
    }
    let cov = m
        .p75_coverage
        .map(|v| format!("{:.1}%", v * 100.0))
        .unwrap_or_else(|| "-".into());
    let alert = if m.p75_alert { " ⚠" } else { "" };
    let bias = fmt_signed_pct(m.p50_rel_bias);
    let bias_mark = if m.p50_bias_alert { " ⚠" } else { "" };
    format!(
        "p75 覆盖 {cov}（{}{alert}）· p50 偏差 {bias}{bias_mark}（n={}）",
        m.status_zh(),
        m.count
    )
}

fn fmt_levels_brief(by_level: &BTreeMap<String, usize>) -> String {
    let mut parts = Vec::new();
    for lvl in ["L1", "L2", "L3", "L4", "L5"] {
        if let Some(&n) = by_level.get(lvl) {
            if n > 0 {
                parts.push(format!("{lvl}:{n}"));
            }
        }
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" ")
    }
}

fn format_backtest_inline_summary(bt: &BacktestReport) -> String {
    format!(
        "\nETA 离线回测（{}，健康 65%–85%，偏差 ±15% 内）：\n  起跑交付：{}\n  中段剩余：{}\n",
        bt.window,
        fmt_bt_metric_brief(&bt.overall_start),
        fmt_bt_metric_brief(&bt.overall_mid),
    )
}

pub fn format_backtest_human(bt: &BacktestReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "delegate speed --backtest (回放窗口 {}，滚动训练 {}d，严格因果)\n",
        bt.window, bt.lookback_days
    ));
    if bt.overall_start.count == 0 && bt.overall_applied.count == 0 {
        out.push_str("\n（窗口内无可回测的已完成写任务）\n");
        return out;
    }

    out.push_str(
        "\n总体指标（p75 健康 65%–85%，<60% 偏乐观 / >90% 偏宽；p50 偏差目标 ±15% 内）：\n",
    );
    out.push_str(&format!(
        "  起跑→交付：{}  回退[{}]\n",
        fmt_bt_metric_brief(&bt.overall_start),
        fmt_levels_brief(&bt.overall_start.by_level),
    ));
    out.push_str(&format!(
        "  中段→交付：{}  回退[{}]\n",
        fmt_bt_metric_brief(&bt.overall_mid),
        fmt_levels_brief(&bt.overall_mid.by_level),
    ));
    if bt.overall_applied.count > 0 {
        out.push_str(&format!(
            "  写链→合入：{}  回退[{}]\n",
            fmt_bt_metric_brief(&bt.overall_applied),
            fmt_levels_brief(&bt.overall_applied.by_level),
        ));
    }

    if !bt.repos.is_empty() {
        out.push('\n');
        out.push_str(&format!(
            "{:<18} {:<6} {:<16} {:<10} {:<16} {:<10} {}\n",
            "仓库", "样本", "起跑 p75覆盖", "起跑 p50偏", "中段 p75覆盖", "中段 p50偏", "回退分布"
        ));
        for r in &bt.repos {
            let s_cov = r
                .start
                .p75_coverage
                .map(|v| {
                    let mark = if r.start.p75_alert { " ⚠" } else { "" };
                    format!("{:.1}%({}){mark}", v * 100.0, r.start.status_zh())
                })
                .unwrap_or_else(|| "-".into());
            let m_cov = r
                .mid
                .p75_coverage
                .map(|v| {
                    let mark = if r.mid.p75_alert { " ⚠" } else { "" };
                    format!("{:.1}%({}){mark}", v * 100.0, r.mid.status_zh())
                })
                .unwrap_or_else(|| "-".into());
            out.push_str(&format!(
                "{:<18} {:<6} {:<16} {:<10} {:<16} {:<10} {}\n",
                clip(&r.repo_name, 18),
                r.start.count,
                s_cov,
                fmt_signed_pct(r.start.p50_rel_bias),
                m_cov,
                fmt_signed_pct(r.mid.p50_rel_bias),
                fmt_levels_brief(&r.start.by_level),
            ));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_state() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "delegate-speed-test-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_events(state: &Path, lines: &[Value]) {
        let mut buf = String::new();
        for (i, v) in lines.iter().enumerate() {
            let mut rec = v.clone();
            rec["seq"] = json!((i + 1) as u64);
            buf.push_str(&rec.to_string());
            buf.push('\n');
        }
        fs::write(state.join("events.jsonl"), buf).unwrap();
    }

    fn ev(ts: i64, run: &str, repo: &str, kind: &str, payload: Value) -> Value {
        json!({
            "at": format_epoch(ts, false, "%Y-%m-%dT%H:%M:%SZ"),
            "run": run,
            "repo": repo,
            "kind": kind,
            "payload": payload,
        })
    }

    #[test]
    fn computes_all_speed_metrics_from_synthetic_stream() {
        let state = temp_state();
        let now = 1_800_000_000_i64; // arbitrary fixed UTC epoch
        let repo = "/work/demo-repo";
        let t0 = now - 3600; // 1 hour ago

        // Chain 1 (first-pass write):
        // started t0, delivered t0+600 (10m), applied t0+720 (12m). merge_wait = 2m.
        //
        // Chain 2 (auto-rework write, 2 rounds):
        // started t0+300 (overlaps Chain 1 for 5m), rework_sent t0+900, review_verdict t0+1500,
        // delivered t0+1500 (20m run), applied t0+1800 (25m total). merge_wait = 5m.
        // Also hits premerge_conflict at t0+1600.
        //
        // Chain 3 (multi-run reply + rework => 3 rounds):
        // r3a started t0+1800, delivered t0+2400; r3b (parent=r3a) started t0+2400, rework_sent t0+2700,
        // delivered t0+3000, applied t0+3600 (30m total). merge_wait = 10m.
        //
        // Chain 4 (read-only):
        // ro1 started t0+100, delivered t0+280 (3m).
        // ro2 started t0+200, delivered t0+560 (6m).
        let stream = vec![
            // Chain 1
            ev(t0, "r1", repo, "started", json!({"mode": "write", "parent": null})),
            ev(t0 + 600, "r1", repo, "delivered", json!({"mode": "write"})),
            ev(t0 + 720, "r1", repo, "applied", json!({})),
            // Chain 2
            ev(t0 + 300, "r2", repo, "started", json!({"mode": "write"})),
            ev(t0 + 900, "r2", repo, "rework_sent", json!({"round": 1})),
            ev(t0 + 1500, "r2", repo, "review_verdict", json!({"verdict": "PASS"})),
            ev(t0 + 1500, "r2", repo, "delivered", json!({"mode": "write"})),
            ev(t0 + 1600, "r2", repo, "premerge_conflict", json!({})),
            ev(t0 + 1800, "r2", repo, "applied", json!({})),
            // Chain 3 (r3a -> r3b + rework_sent => 3 rounds)
            ev(t0 + 1800, "r3a", repo, "started", json!({"mode": "write"})),
            ev(t0 + 2400, "r3a", repo, "delivered", json!({"mode": "write"})),
            ev(t0 + 2400, "r3b", repo, "started", json!({"mode": "write", "parent": "r3a"})),
            ev(t0 + 2700, "r3b", repo, "rework_sent", json!({"round": 1})),
            ev(t0 + 3000, "r3b", repo, "delivered", json!({"mode": "write"})),
            ev(t0 + 3600, "r3b", repo, "applied", json!({})),
            // Read-only chains
            ev(t0 + 100, "ro1", repo, "started", json!({"mode": "read-only"})),
            ev(t0 + 280, "ro1", repo, "delivered", json!({"mode": "read-only"})),
            ev(t0 + 200, "ro2", repo, "started", json!({"mode": "read-only"})),
            ev(t0 + 560, "ro2", repo, "delivered", json!({"mode": "read-only"})),
        ];
        write_events(&state, &stream);

        // Using --since all so no auto-expand triggers here.
        let reports = analyze_in(&state, now, "all", None, false).unwrap();
        assert_eq!(reports.len(), 1);
        let r = &reports[0];

        assert_eq!(r.repo_name, "demo-repo");
        assert_eq!(r.write_chains_total, 3);
        assert_eq!(r.write_wall.count, 3);
        // Write wall clocks: [12m, 25m, 30m] -> p50 = 25m, p75 = 27.5m, mean = 22.33m
        assert_eq!(r.t_chain_min(), Some(25.0));
        assert_eq!(r.write_wall.p75_sec.map(sec_to_min), Some(27.5));
        assert!((r.write_wall.mean_sec.map(sec_to_min).unwrap() - 22.33).abs() < 0.02);

        // First-pass: [12m]
        assert_eq!(r.first_pass.count, 1);
        assert_eq!(r.first_pass.p50_sec.map(sec_to_min), Some(12.0));

        // t_crit (non-first-pass): [25m, 30m] -> p50 = 27.5m, p75 = 28.75m
        assert_eq!(r.t_crit.count, 2);
        assert_eq!(r.t_crit_min(), Some(27.5));
        assert_eq!(r.t_crit_p75_min(), Some(28.75));

        // Rework rates:
        // auto_rework: Chain 2 & Chain 3 => 2/3
        // non_first_pass: Chain 2 & Chain 3 => 2/3
        // 3+ rounds: Chain 3 (2 runs + 1 rework_sent = 3) => 1/3
        assert_eq!(r.auto_rework_count, 2);
        assert!((r.auto_rework_rate.unwrap() - 2.0 / 3.0).abs() < 1e-6);
        assert_eq!(r.non_first_pass_count, 2);
        assert!((r.non_first_pass_rate.unwrap() - 2.0 / 3.0).abs() < 1e-6);
        assert_eq!(r.rounds_3plus_count, 1);
        assert!((r.rounds_3plus_rate.unwrap() - 1.0 / 3.0).abs() < 1e-6);

        // Merge metrics:
        // premerge_conflict: 1/3
        assert_eq!(r.premerge_conflict_count, 1);
        assert_eq!(r.premerge_failed_count, 0);
        assert_eq!(r.premerge_issue_count, 1);
        assert!((r.premerge_issue_rate.unwrap() - 1.0 / 3.0).abs() < 1e-6);
        // merge waits: [2m, 5m, 10m] -> p75 = 7.5m
        assert_eq!(r.merge_wait.count, 3);
        assert_eq!(r.t_merge_wait_p75_min(), Some(7.5));

        // Concurrency: Chain 1 and Chain 2 overlap for ~5 minutes, so P_eff > 1.0.
        assert!(r.concurrency.active_minutes > 0);
        assert!(r.concurrency.mean.unwrap() > 1.0);
        assert!(r.concurrency.applied_per_active_hour.unwrap() > 0.0);

        // Read-only chains: [3m, 6m] -> p50 = 4.5m, p75 = 5.25m
        assert_eq!(r.read_only.count, 2);
        assert_eq!(r.read_only.p50_sec.map(sec_to_min), Some(4.5));
        assert_eq!(r.read_only.p75_sec.map(sec_to_min), Some(5.25));

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn auto_expands_7d_to_14d_when_under_30_write_chains() {
        let state = temp_state();
        let now = 1_800_000_000_i64;
        let repo = "/work/small-repo";
        let mut stream = Vec::new();

        // 2 write chains inside 7d (2 days ago)
        for i in 0..2 {
            let t = now - 2 * 86_400 + i * 1000;
            let id = format!("recent-{i}");
            stream.push(ev(t, &id, repo, "started", json!({"mode": "write"})));
            stream.push(ev(t + 600, &id, repo, "delivered", json!({"mode": "write"})));
            stream.push(ev(t + 660, &id, repo, "applied", json!({})));
        }
        // 3 write chains between 7d and 14d (10 days ago)
        for i in 0..3 {
            let t = now - 10 * 86_400 + i * 1000;
            let id = format!("older-{i}");
            stream.push(ev(t, &id, repo, "started", json!({"mode": "write"})));
            stream.push(ev(t + 1200, &id, repo, "delivered", json!({"mode": "write"})));
            stream.push(ev(t + 1260, &id, repo, "applied", json!({})));
        }
        write_events(&state, &stream);

        let reports = analyze_in(&state, now, "7d", None, false).unwrap();
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert!(r.expanded);
        assert_eq!(r.requested_since, "7d");
        assert_eq!(r.effective_since, "14d");
        assert_eq!(r.initial_write_chains, Some(2));
        assert_eq!(r.write_wall.count, 5);

        let human = format_human(&reports, "7d");
        assert!(human.contains("14d*"));
        assert!(human.contains("自动扩窗"));

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn persists_history_and_flags_drift_over_20_percent() {
        let state = temp_state();
        let now = 1_800_000_000_i64;
        let repo = "/work/drift-repo";

        // Run 1: 10m write chain -> persisted to <state>/speed/drift-repo.jsonl
        let s1 = vec![
            ev(now - 3600, "r1", repo, "started", json!({"mode": "write"})),
            ev(now - 3060, "r1", repo, "delivered", json!({"mode": "write"})),
            ev(now - 3000, "r1", repo, "applied", json!({})),
        ];
        write_events(&state, &s1);
        let r1 = analyze_in(&state, now, "all", None, true).unwrap();
        assert_eq!(r1.len(), 1);
        assert!(r1[0].drift.metrics.is_empty());

        let hist_path = state.join("speed").join("drift-repo.jsonl");
        assert!(hist_path.is_file());

        // Run 2: 15m write chain (+50% drift on t_chain => alert!)
        let s2 = vec![
            ev(now - 1800, "r2", repo, "started", json!({"mode": "write"})),
            ev(now - 960, "r2", repo, "delivered", json!({"mode": "write"})),
            ev(now - 900, "r2", repo, "applied", json!({})),
        ];
        write_events(&state, &s2);
        let r2 = analyze_in(&state, now + 60, "all", None, true).unwrap();
        assert_eq!(r2.len(), 1);
        let d = &r2[0].drift;
        assert!(d.any_alert);
        let tc = d.metrics.get("t_chain").expect("t_chain drift");
        assert!((tc.pct - 50.0).abs() < 0.1);
        assert!(tc.alert);

        let lines = fs::read_to_string(&hist_path).unwrap();
        assert_eq!(lines.lines().count(), 2);

        let human = format_human(&r2, "all");
        assert!(human.contains("⚠"));

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn repo_filter_selects_only_matching_repo() {
        let state = temp_state();
        let now = 1_800_000_000_i64;
        let stream = vec![
            ev(now - 1000, "a1", "/work/alpha", "started", json!({"mode": "write"})),
            ev(now - 400, "a1", "/work/alpha", "applied", json!({})),
            ev(now - 1000, "b1", "/work/beta", "started", json!({"mode": "write"})),
            ev(now - 400, "b1", "/work/beta", "applied", json!({})),
        ];
        write_events(&state, &stream);

        let reports = analyze_in(&state, now, "all", Some("beta"), false).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].repo_name, "beta");

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn stays_7d_when_at_least_30_write_chains() {
        let state = temp_state();
        let now = 1_800_000_000_i64;
        let repo = "/work/busy-repo";
        let mut stream = Vec::new();

        for i in 0..30 {
            let t = now - 2 * 86_400 + i * 600;
            let id = format!("w-{i}");
            stream.push(ev(t, &id, repo, "started", json!({"mode": "write"})));
            stream.push(ev(t + 300, &id, repo, "delivered", json!({"mode": "write"})));
            stream.push(ev(t + 360, &id, repo, "applied", json!({})));
        }
        // Extra chain at 10d should NOT be pulled in.
        let old_t = now - 10 * 86_400;
        stream.push(ev(old_t, "old-1", repo, "started", json!({"mode": "write"})));
        stream.push(ev(old_t + 3600, "old-1", repo, "applied", json!({})));
        write_events(&state, &stream);

        let reports = analyze_in(&state, now, "7d", None, false).unwrap();
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert!(!r.expanded);
        assert_eq!(r.effective_since, "7d");
        assert_eq!(r.write_wall.count, 30);

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn multi_hop_parent_and_premerge_failed_and_json_fields() {
        let state = temp_state();
        let now = 1_800_000_000_i64;
        let repo = "/work/chain-repo";
        let t0 = now - 3600;

        // 3-hop chain r1 -> r2 -> r3 with NO rework_sent, plus premerge_failed.
        let stream = vec![
            ev(t0, "r1", repo, "started", json!({"mode": "write"})),
            ev(t0 + 300, "r1", repo, "delivered", json!({"mode": "write"})),
            ev(t0 + 360, "r2", repo, "started", json!({"mode": "write", "parent": "r1"})),
            ev(t0 + 660, "r2", repo, "delivered", json!({"mode": "write"})),
            ev(t0 + 720, "r3", repo, "started", json!({"mode": "write", "parent": "r2"})),
            ev(t0 + 1020, "r3", repo, "review_verdict", json!({"verdict": "PASS"})),
            ev(t0 + 1080, "r3", repo, "premerge_failed", json!({})),
            ev(t0 + 1200, "r3", repo, "applied", json!({})),
        ];
        write_events(&state, &stream);

        let reports = analyze_in(&state, now, "all", None, false).unwrap();
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(r.write_chains_total, 1);
        assert_eq!(r.write_wall.count, 1);
        assert_eq!(r.t_chain_min(), Some(20.0));
        assert_eq!(r.auto_rework_count, 0);
        assert_eq!(r.non_first_pass_count, 1);
        assert_eq!(r.rounds_3plus_count, 1);
        assert_eq!(r.premerge_failed_count, 1);
        assert_eq!(r.premerge_issue_rate, Some(1.0));
        // review_verdict at t0+1020 -> applied at t0+1200 => 3m wait
        assert_eq!(r.t_merge_wait_p75_min(), Some(3.0));

        let j = r.to_full_json();
        assert_eq!(j["formula"]["tChainMin"], json!(20.0));
        assert_eq!(j["formula"]["tCritMin"], json!(20.0));
        assert_eq!(j["formula"]["tMergeWaitP75Min"], json!(3.0));
        assert_eq!(j["rework"]["rounds3PlusRate"], json!(1.0));

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn eta_model_extracts_stage_and_suffix_distributions() {
        let state = temp_state();
        let now = 1_800_000_000_i64;
        let repo = "/work/stages-repo";
        let t0 = now - 3600;

        // Run 1: queued -> started -> fast+parallel(slow, review) -> rework -> delivered -> premerge_ready(reused) -> applied
        // Run 2: legacy event stream (no accept.elapsedSeconds) -> falls back to lane_released.heldSeconds + lane_acquired.queuedSeconds,
        //        plus premerge_ready(reusedAccept=false) + merge_started/merge_done.
        let stream = vec![
            // Run 1
            ev(t0, "r1", repo, "queued", json!({"scope": "capacity", "reason": "总路数", "ahead": 1})),
            ev(t0 + 60, "r1", repo, "started", json!({"mode": "write", "class": "build", "thinking": "medium", "workId": "W010", "fromQueue": true})),
            ev(t0 + 370, "r1", repo, "review_started", json!({"round": 1})),
            ev(t0 + 390, "r1", repo, "lane_acquired", json!({"queuedSeconds": 20.0})),
            ev(t0 + 430, "r1", repo, "lane_released", json!({"heldSeconds": 40.0})),
            ev(t0 + 450, "r1", repo, "accept", json!({
                "ok": true, "tiered": true, "failClosed": false, "parallelWithReview": true,
                "ranLayers": ["slow", "fast"], "elapsedSeconds": 50.0, "queuedSeconds": 20.0,
                "layerSeconds": {"fast": 10.0, "slow": 40.0}
            })),
            ev(t0 + 450, "r1", repo, "review_verdict", json!({"round": 1, "verdict": "REWORK"})),
            ev(t0 + 450, "r1", repo, "rework_sent", json!({"round": 1})),
            ev(t0 + 600, "r1", repo, "review_started", json!({"round": 2})),
            ev(t0 + 640, "r1", repo, "accept", json!({
                "ok": true, "tiered": true, "failClosed": false, "parallelWithReview": true,
                "ranLayers": ["slow"], "elapsedSeconds": 30.0, "queuedSeconds": 0.0,
                "layerSeconds": {"slow": 30.0}
            })),
            ev(t0 + 640, "r1", repo, "review_verdict", json!({"round": 2, "verdict": "PASS"})),
            ev(t0 + 640, "r1", repo, "delivered", json!({"mode": "write"})),
            ev(t0 + 645, "r1", repo, "premerge_ready", json!({"elapsedSeconds": 4.5, "reusedAccept": true, "ranLayers": ["fast", "slow"]})),
            ev(t0 + 820, "r1", repo, "applied", json!({})),
            // Run 2 (legacy accept without elapsedSeconds, failClosed=true, non-reused premerge)
            ev(t0 + 1000, "r2", repo, "started", json!({"mode": "write", "class": "build"})),
            ev(t0 + 1250, "r2", repo, "lane_acquired", json!({"queuedSeconds": 10.0})),
            ev(t0 + 1300, "r2", repo, "lane_released", json!({"heldSeconds": 50.0})),
            ev(t0 + 1300, "r2", repo, "accept", json!({"ok": true, "tiered": true, "failClosed": true, "ranLayers": ["full"]})),
            ev(t0 + 1300, "r2", repo, "delivered", json!({"mode": "write"})),
            ev(t0 + 1350, "r2", repo, "premerge_ready", json!({"elapsedSeconds": 48.0, "reusedAccept": false})),
            ev(t0 + 1400, "r2", repo, "applied", json!({})),
            // Merge execution
            ev(t0 + 1500, "m1", repo, "merge_started", json!({"mergeRun": "m1"})),
            ev(t0 + 1535, "m1", repo, "merge_done", json!({"mergeRun": "m1"})),
        ];
        write_events(&state, &stream);

        let model = build_eta_model_in(&state, now);
        let rm = model.repo_model(repo).expect("repo model");
        let b = rm.by_mode_class.get("write:build").expect("write:build bucket");

        // S1 capacity queue: 60s
        assert_eq!(b.capacity_queue.count, 1);
        assert_eq!(b.capacity_queue.p50_sec, Some(60.0));
        assert_eq!(
            model.global.capacity_queue_by_reason.get("总路数").and_then(|d| d.p50_sec),
            Some(60.0)
        );
        // S2 agent_work: r1 = min(450-60-50-20, 370-60) = 310s; r2 = 1300-1000-50-10 = 240s
        assert_eq!(b.agent_work.count, 2);
        assert_eq!(b.agent_work.samples_sec, vec![240.0, 310.0]);
        // S3 lane: held [40, 50], queue [10, 20], recent 2h share = 30 / (30 + 90) = 0.25
        assert_eq!(rm.lane_held.samples_sec, vec![40.0, 50.0]);
        assert_eq!(rm.lane_queue.samples_sec, vec![10.0, 20.0]);
        assert!((model.global.recent_lane_2h.queue_share - 0.25).abs() < 1e-4);
        // S4 accept signatures & per-layer durations
        assert_eq!(rm.accept_by_sig.get("fast,slow").and_then(|d| d.p50_sec), Some(50.0));
        assert_eq!(rm.accept_by_sig.get("slow").and_then(|d| d.p50_sec), Some(30.0));
        assert_eq!(rm.accept_by_sig.get("failClosed").and_then(|d| d.p50_sec), Some(50.0));
        assert_eq!(rm.accept_layers.get("fast").and_then(|d| d.p50_sec), Some(10.0));
        assert_eq!(rm.accept_layers.get("slow").map(|d| d.samples_sec.clone()), Some(vec![30.0, 40.0]));
        // S5 review & parallel
        assert_eq!(b.review.samples_sec, vec![40.0, 80.0]);
        assert_eq!(b.accept_review_parallel.samples_sec, vec![40.0, 80.0]);
        // S6 rework round: 640 - 450 = 190s
        assert_eq!(b.rework_round.samples_sec, vec![190.0]);
        // S7 premerge reused vs rerun
        assert_eq!(rm.premerge_reused.samples_sec, vec![4.5]);
        assert_eq!(rm.premerge_rerun.samples_sec, vec![48.0]);
        // S8 merge wait [100, 180] and merge exec [35]
        assert_eq!(b.merge_wait.samples_sec, vec![100.0, 180.0]);
        assert_eq!(rm.merge_exec.samples_sec, vec![35.0]);
        // Suffixes:
        // afterWork: r1 = 640 - 370 = 270, r2 = 1300 - 1240 = 60
        assert_eq!(b.suffix_after_work.samples_sec, vec![60.0, 270.0]);
        // afterAccept: r1 = 640 - 450 = 190, r2 = 0
        assert_eq!(b.suffix_after_accept.samples_sec, vec![0.0, 190.0]);
        // afterReview: r1 = 640 - 450 = 190
        assert_eq!(b.suffix_after_review.samples_sec, vec![190.0]);
        // afterRework: r1 = 640 - 450 = 190
        assert_eq!(b.suffix_after_rework.samples_sec, vec![190.0]);

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn eta_model_buckets_by_mode_and_class() {
        let state = temp_state();
        let now = 1_800_000_000_i64;
        let repo = "/work/bucket-repo";
        let t0 = now - 7200;

        let stream = vec![
            ev(t0, "wh", repo, "started", json!({"mode": "write", "class": "hard"})),
            ev(t0 + 1800, "wh", repo, "delivered", json!({"mode": "write"})),
            ev(t0, "wb", repo, "started", json!({"mode": "write", "class": "build"})),
            ev(t0 + 900, "wb", repo, "delivered", json!({"mode": "write"})),
            ev(t0, "wd", repo, "started", json!({"mode": "write", "class": "design"})),
            ev(t0 + 1200, "wd", repo, "delivered", json!({"mode": "write"})),
            ev(t0, "wo", repo, "started", json!({"mode": "write"})),
            ev(t0 + 600, "wo", repo, "delivered", json!({"mode": "write"})),
            ev(t0, "rs", repo, "started", json!({"mode": "read-only", "tier": "scout"})),
            ev(t0 + 120, "rs", repo, "delivered", json!({"mode": "read-only"})),
            ev(t0, "rr", repo, "started", json!({"mode": "read-only", "tier": "review"})),
            ev(t0 + 240, "rr", repo, "delivered", json!({"mode": "read-only"})),
        ];
        write_events(&state, &stream);

        let model = build_eta_model_in(&state, now);
        let rm = model.repo_model(repo).expect("repo model");

        assert_eq!(rm.by_mode_class["write:hard"].run_to_delivered.p50_sec, Some(1800.0));
        assert_eq!(rm.by_mode_class["write:build"].run_to_delivered.p50_sec, Some(900.0));
        assert_eq!(rm.by_mode_class["write:design"].run_to_delivered.p50_sec, Some(1200.0));
        assert_eq!(rm.by_mode_class["write:other"].run_to_delivered.p50_sec, Some(600.0));
        assert_eq!(rm.by_mode_class["read-only:scout"].run_to_delivered.p50_sec, Some(120.0));
        assert_eq!(rm.by_mode_class["read-only:review"].run_to_delivered.p50_sec, Some(240.0));
        assert_eq!(rm.by_mode["write"].run_to_delivered.count, 4);
        assert_eq!(rm.by_mode["read-only"].run_to_delivered.count, 2);

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn eta_model_fallback_chain_l1_to_l5() {
        let state = temp_state();
        let now = 1_800_000_000_i64;
        let t0 = now - 3 * 86_400;
        let mut stream = Vec::new();

        // repo-a: 10 write:hard (L1 eligible) + 2 write:design (L1<10, L2=12>=10)
        for i in 0..10 {
            let id = format!("a-hard-{i}");
            let ts = t0 + i * 100;
            stream.push(ev(ts, &id, "/work/repo-a", "started", json!({"mode": "write", "class": "hard"})));
            stream.push(ev(ts + 1000, &id, "/work/repo-a", "accept", json!({"ok": true, "tiered": true, "failClosed": true, "elapsedSeconds": 120.0})));
            stream.push(ev(ts + 2000, &id, "/work/repo-a", "delivered", json!({"mode": "write"})));
            stream.push(ev(ts + 2300, &id, "/work/repo-a", "applied", json!({})));
        }
        for i in 0..2 {
            let id = format!("a-design-{i}");
            let ts = t0 + 2000 + i * 100;
            stream.push(ev(ts, &id, "/work/repo-a", "started", json!({"mode": "write", "class": "design"})));
            stream.push(ev(ts + 1500, &id, "/work/repo-a", "delivered", json!({"mode": "write"})));
        }

        // repo-b: 6 write:build; repo-c: 4 write:build => global write:build = 10 (L3 eligible for cold repo-d)
        for i in 0..6 {
            let id = format!("b-build-{i}");
            let ts = t0 + 4000 + i * 100;
            stream.push(ev(ts, &id, "/work/repo-b", "started", json!({"mode": "write", "class": "build"})));
            stream.push(ev(ts + 800, &id, "/work/repo-b", "delivered", json!({"mode": "write"})));
        }
        for i in 0..4 {
            let id = format!("c-build-{i}");
            let ts = t0 + 6000 + i * 100;
            stream.push(ev(ts, &id, "/work/repo-c", "started", json!({"mode": "write", "class": "build"})));
            stream.push(ev(ts + 800, &id, "/work/repo-c", "delivered", json!({"mode": "write"})));
        }

        // read-only: only 3 total across all repos (< MIN_GLOBAL_MODE_N=5) => L5 prior!
        for i in 0..3 {
            let id = format!("ro-{i}");
            let ts = t0 + 8000 + i * 100;
            stream.push(ev(ts, &id, "/work/repo-a", "started", json!({"mode": "read-only", "tier": "scout"})));
            stream.push(ev(ts + 180, &id, "/work/repo-a", "delivered", json!({"mode": "read-only"})));
        }

        write_events(&state, &stream);
        let model = build_eta_model_in(&state, now);

        // L1: (repo-a, write, hard) has 10 >= 10
        let r1 = model.resolve_bucket("/work/repo-a", "write", "hard");
        assert_eq!(r1.level, FallbackLevel::RepoModeClass);
        assert_eq!(r1.bucket.run_to_delivered.p50_sec, Some(2000.0));

        // L2: (repo-a, write, design) has 2 < 10 -> falls back to (repo-a, write) with 12 >= 10
        let r2 = model.resolve_bucket("/work/repo-a", "write", "design");
        assert_eq!(r2.level, FallbackLevel::RepoMode);
        assert_eq!(r2.bucket.run_to_delivered.count, 12);

        // L3: (repo-d, write, build) -> repo-d missing -> global write:build has 10 >= 10
        let r3 = model.resolve_bucket("/work/repo-d", "write", "build");
        assert_eq!(r3.level, FallbackLevel::GlobalModeClass);
        assert_eq!(r3.bucket.run_to_delivered.p50_sec, Some(800.0));

        // L4: (repo-d, write, design) -> global write:design has 2 < 10 -> global write has 22 >= 5
        let r4 = model.resolve_bucket("/work/repo-d", "write", "design");
        assert_eq!(r4.level, FallbackLevel::GlobalMode);
        assert_eq!(r4.bucket.run_to_delivered.count, 22);

        // L5: (repo-d, read-only, scout) -> global read-only has 3 < 5 -> built-in cold-start prior
        let r5 = model.resolve_bucket("/work/repo-d", "read-only", "scout");
        assert_eq!(r5.level, FallbackLevel::Prior);
        assert!(r5.level.is_prior());
        assert_eq!(r5.bucket.run_to_delivered.p50_sec, Some(300.0));
        assert_eq!(r5.bucket.run_to_delivered.p75_sec, Some(600.0));

        // Accept sub-table resolution: repo-a failClosed has 10 -> L1; repo-d failClosed -> L3
        let a1 = model.resolve_accept_dist("/work/repo-a", true, true, &[]);
        assert_eq!(a1.level, FallbackLevel::RepoModeClass);
        assert_eq!(a1.dist.p50_sec, Some(120.0));
        let a3 = model.resolve_accept_dist("/work/repo-d", true, true, &[]);
        assert_eq!(a3.level, FallbackLevel::GlobalModeClass);
        assert_eq!(a3.dist.p50_sec, Some(120.0));

        // Formula resolution: repo-a has 10 applied -> L2; repo-d -> L4 (global=10); empty model -> L5 prior (63/87m, 75/102m, P_eff=2.0, T_merge=9m)
        assert_eq!(model.resolve_formula("/work/repo-a").level, FallbackLevel::RepoMode);
        assert_eq!(model.resolve_formula("/work/repo-d").level, FallbackLevel::GlobalMode);

        let empty_model = build_eta_model_from_events(&state, &[], now, 0, ETA_MODEL_WINDOW_DAYS);
        let f_prior = empty_model.resolve_formula("/work/empty");
        assert_eq!(f_prior.level, FallbackLevel::Prior);
        assert_eq!(f_prior.params.t_chain_p50_sec, Some(63.0 * 60.0));
        assert_eq!(f_prior.params.t_chain_p75_sec, Some(87.0 * 60.0));
        assert_eq!(f_prior.params.t_crit_p50_sec, Some(75.0 * 60.0));
        assert_eq!(f_prior.params.t_crit_p75_sec, Some(102.0 * 60.0));
        assert_eq!(f_prior.params.p_eff_mean, Some(2.0));
        assert_eq!(f_prior.params.t_merge_p50_sec, Some(9.0 * 60.0));

        // Per-stage resolve_dist widens gracefully when a specific stage is sparse.
        let rw_dist = model.resolve_dist("/work/repo-a", "write", "hard", |b| &b.rework_round);
        assert_eq!(rw_dist.level, FallbackLevel::Prior);

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn sample_dist_conditional_residual_and_anomaly_tail() {
        // 10 samples: 100, 200, ..., 1000 -> p50 = 550, p75 = 775, p90 = 910
        let dist = SampleDist::from_secs((1..=10).map(|i| (i * 100) as f64).collect());
        // At elapsed = 300: surviving > 300 are [400..1000] (7 >= 5 samples),
        // residuals = [100, 200, 300, 400, 500, 600, 700] -> p50 = 400, p75 = 550
        let (r50, r75, anom) = dist.conditional_residual(300.0).unwrap();
        assert!(!anom);
        assert_eq!(r50, 400.0);
        assert_eq!(r75, 550.0);

        // At elapsed = 650: surviving > 650 are [700, 800, 900, 1000] (4 < 5 samples) -> anomaly tail
        // tail50 = max(300, 0.25 * 550) = 300
        let (t50, t75, anom2) = dist.conditional_residual(650.0).unwrap();
        assert!(anom2);
        assert_eq!(t50, 300.0);
        assert!(t75 >= t50);

        // Large-scale dist (p50 = 2000s) -> tail50 = max(300, 0.25 * 2000) = 500s when elapsed > p90
        let big = SampleDist::from_secs((1..=10).map(|i| (i * 400) as f64).collect());
        let (b50, _, anom3) = big.conditional_residual(5000.0).unwrap();
        assert!(anom3);
        assert_eq!(b50, 550.0); // p50 = 2200 -> 0.25 * 2200 = 550
    }

    #[test]
    fn eta_model_atomic_write_roundtrip_and_lazy_refresh() {
        let state = temp_state();
        let now = 1_800_000_000_i64;
        let stream = vec![
            ev(now - 600, "a1", "/work/alpha", "started", json!({"mode": "write", "class": "hard"})),
            ev(now - 100, "a1", "/work/alpha", "delivered", json!({"mode": "write"})),
            ev(now - 500, "b1", "/work/beta", "started", json!({"mode": "write", "class": "build"})),
            ev(now - 200, "b1", "/work/beta", "delivered", json!({"mode": "write"})),
        ];
        write_events(&state, &stream);

        // analyze_in with repo_filter="beta" and persist=true still writes full machine-wide eta-model.json.
        let reports = analyze_in(&state, now, "all", Some("beta"), true).unwrap();
        assert_eq!(reports.len(), 1);
        let model_path = eta_model_path(&state);
        assert!(model_path.is_file());

        let loaded = load_eta_model_in(&state).expect("round-trip load");
        assert_eq!(loaded.version, ETA_MODEL_VERSION);
        assert_eq!(loaded.events_seq, 4);
        assert!(loaded.repo_model("/work/alpha").is_some());
        assert!(loaded.repo_model("/work/beta").is_some());

        // Not stale when age <= 15m even if events_seq advances, or when age > 15m and seq unchanged.
        let cached_same = load_or_refresh_eta_model_in(&state, now + 3600, true);
        assert_eq!(cached_same.generated_epoch, now);

        // Append a new event (seq=5); within 15m still uses cache; past 15m triggers rebuild!
        let mut s2 = stream.clone();
        s2.push(ev(now + 10, "b1", "/work/beta", "applied", json!({})));
        write_events(&state, &s2);
        let within_15m = load_or_refresh_eta_model_in(&state, now + 600, true);
        assert_eq!(within_15m.events_seq, 4);

        let refreshed = load_or_refresh_eta_model_in(&state, now + ETA_MODEL_STALE_SECS + 60, true);
        assert_eq!(refreshed.events_seq, 5);
        assert_eq!(refreshed.generated_epoch, now + ETA_MODEL_STALE_SECS + 60);

        // Delete or corrupt cache -> rebuilds cleanly from events.jsonl.
        fs::write(&model_path, "{corrupt json").unwrap();
        assert!(load_eta_model_in(&state).is_none());
        let rebuilt = load_or_refresh_eta_model_in(&state, now + 2000, true);
        assert_eq!(rebuilt.events_seq, 5);
        assert!(load_eta_model_in(&state).is_some());

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn eta_model_skips_not_executed_accept_samples() {
        let state = temp_state();
        let now = 1_800_000_000_i64;
        let repo = "/work/notexec-repo";
        let t0 = now - 3600;

        let stream = vec![
            // Run 1: infrastructure failure where accept was notExecuted with elapsedSeconds=0.0
            ev(t0, "r1", repo, "started", json!({"mode": "write", "class": "build"})),
            ev(t0 + 200, "r1", repo, "accept", json!({
                "ok": false, "notExecuted": true, "infrastructureFailure": true,
                "tiered": false, "elapsedSeconds": 0.0, "queuedSeconds": 0.0,
                "layerSeconds": {"accept": 0.0}
            })),
            ev(t0 + 200, "r1", repo, "failed", json!({"mode": "write"})),
            // Run 2: real executed accept (elapsedSeconds = 42.0)
            ev(t0 + 400, "r2", repo, "started", json!({"mode": "write", "class": "build"})),
            ev(t0 + 700, "r2", repo, "accept", json!({
                "ok": true, "notExecuted": false, "tiered": false,
                "elapsedSeconds": 42.0, "queuedSeconds": 0.0,
                "layerSeconds": {"accept": 42.0}
            })),
            ev(t0 + 700, "r2", repo, "delivered", json!({"mode": "write"})),
        ];
        write_events(&state, &stream);

        let model = build_eta_model_in(&state, now);
        let rm = model.repo_model(repo).expect("repo model");
        let b = rm.by_mode_class.get("write:build").expect("write:build bucket");

        // Only r2's 42.0s sample is counted; r1's notExecuted 0.0s sample is excluded.
        assert_eq!(b.accept.count, 1);
        assert_eq!(b.accept.samples_sec, vec![42.0]);
        assert_eq!(rm.accept_by_sig.get("default").map(|d| d.samples_sec.clone()), Some(vec![42.0]));
        assert_eq!(rm.accept_layers.get("accept").map(|d| d.samples_sec.clone()), Some(vec![42.0]));

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn backtest_is_strictly_causal_and_computes_coverage_and_bias() {
        let state = temp_dir_for_bt("causal");
        let now = 1_800_000_000_i64;
        let repo = "/work/bt-repo";
        let t0 = now - 5 * 86_400;
        let mut stream = Vec::new();

        // 15 historical runs with steady durations 1000..=1140s (so L1 bucket activates after 10 completed runs).
        for i in 0..15 {
            let id = format!("h-{i}");
            let st = t0 + i * 3600;
            let dur = 1000 + (i % 5) * 35; // 1000, 1035, 1070, 1105, 1140
            let del = st + dur;
            stream.push(ev(st, &id, repo, "started", json!({"mode": "write", "class": "build"})));
            stream.push(ev(del, &id, repo, "accept", json!({"ok": true, "tiered": false, "elapsedSeconds": 60.0, "queuedSeconds": 0.0})));
            stream.push(ev(del, &id, repo, "delivered", json!({"mode": "write"})));
            stream.push(ev(del + 180, &id, repo, "applied", json!({})));
        }
        // Future outlier run AFTER all 15 runs: must NOT leak backwards into earlier predictions!
        let fut_st = t0 + 20 * 3600;
        stream.push(ev(fut_st, "fut-giant", repo, "started", json!({"mode": "write", "class": "build"})));
        stream.push(ev(fut_st + 90_000, "fut-giant", repo, "delivered", json!({"mode": "write"})));

        write_events(&state, &stream);
        let parsed = parse_events(events::read_since_in(&state, 0));
        let bt = backtest_from_events(&parsed, now, "14d", None).unwrap();

        // 16 write runs evaluated in start checkpoint; first 5 use L5 prior, next 5 use L4 global, last 6 use L1!
        assert_eq!(bt.overall_start.count, 16);
        assert_eq!(bt.overall_start.by_level.get("L5"), Some(&5));
        assert_eq!(bt.overall_start.by_level.get("L4"), Some(&5));
        assert_eq!(bt.overall_start.by_level.get("L1"), Some(&6));
        assert_eq!(bt.overall_mid.count, 16);
        assert!(bt.overall_start.p75_coverage.is_some());
        assert!(bt.overall_start.p50_rel_bias.is_some());

        // Strict causality check: run 0..14 must get identical metrics whether fut-giant exists or not.
        let parsed_no_fut: Vec<EventRec> = parsed
            .into_iter()
            .filter(|e| e.run != "fut-giant")
            .collect();
        let bt_no_fut = backtest_from_events(&parsed_no_fut, now, "14d", None).unwrap();
        assert_eq!(bt_no_fut.overall_start.count, 15);
        // Removing the single 90_000s outlier leaves covered count identical (the outlier was not covered)
        assert_eq!(bt.overall_start.p75_covered, bt_no_fut.overall_start.p75_covered);

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn backtest_flags_optimistic_and_too_wide_alerts() {
        let state = temp_dir_for_bt("alerts");
        let now = 1_800_000_000_i64;
        let repo = "/work/alert-repo";
        let t0 = now - 6 * 86_400;

        // Regime shift: 10 fast runs (200s) then 10 slow runs (2000s) -> optimistic (<60% coverage)
        let mut fast_then_slow = Vec::new();
        for i in 0..10 {
            let st = t0 + i * 3600;
            let id = format!("f-{i}");
            fast_then_slow.push(ev(st, &id, repo, "started", json!({"mode": "write", "class": "build"})));
            fast_then_slow.push(ev(st + 200, &id, repo, "delivered", json!({"mode": "write"})));
        }
        for i in 0..15 {
            let st = t0 + (12 + i) * 3600;
            let id = format!("s-{i}");
            fast_then_slow.push(ev(st, &id, repo, "started", json!({"mode": "write", "class": "build"})));
            fast_then_slow.push(ev(st + 2000 + i * 200, &id, repo, "delivered", json!({"mode": "write"})));
        }
        write_events(&state, &fast_then_slow);
        let parsed = parse_events(events::read_since_in(&state, 0));
        // Evaluate only the slow tail (last 4 days where history starts fast and keeps climbing)
        let bt = backtest_from_events(&parsed, now, "14d", None).unwrap();
        let human = format_backtest_human(&bt);
        assert!(human.contains("delegate speed --backtest"));
        assert!(bt.overall_start.p75_coverage.unwrap() < 0.85);

        // Purely synthetic metric check: 100% covered -> too_wide + alert; 30% covered -> optimistic + alert
        let wide_obs: Vec<BacktestObs> = (0..10)
            .map(|_| BacktestObs {
                actual_sec: 100.0,
                pred_p50_sec: 500.0,
                pred_p75_sec: 800.0,
                level: FallbackLevel::Prior,
            })
            .collect();
        let wide_m = BacktestMetrics::from_obs(&wide_obs);
        assert_eq!(wide_m.p75_status, "too_wide");
        assert!(wide_m.p75_alert);
        assert!(wide_m.p50_bias_alert);

        let opt_obs: Vec<BacktestObs> = (0..10)
            .map(|i| BacktestObs {
                actual_sec: 1000.0,
                pred_p50_sec: 400.0,
                pred_p75_sec: if i < 3 { 1200.0 } else { 500.0 },
                level: FallbackLevel::RepoModeClass,
            })
            .collect();
        let opt_m = BacktestMetrics::from_obs(&opt_obs);
        assert_eq!(opt_m.p75_status, "optimistic");
        assert!(opt_m.p75_alert);
        // median((1000 - 400) / 1000) = +0.60
        assert!((opt_m.p50_rel_bias.unwrap() - 0.60).abs() < 1e-6);

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn online_calibration_aggregates_coverage_bias_alerts_and_handles_deletion() {
        let state = temp_dir_for_bt("online-cal");
        let now = 1_800_000_000_i64;
        let repo_a = "/work/healthy-repo";
        let repo_b = "/work/opt-repo";

        // Seed minimal events so analyze_in produces rows for both repos.
        let mut evs = Vec::new();
        for (repo, tag) in [(repo_a, "a"), (repo_b, "b")] {
            let st = now - 3600;
            evs.push(json!({"seq": evs.len() + 1, "at": format_epoch(st, false, "%Y-%m-%dT%H:%M:%SZ"), "run": format!("r-{tag}"), "repo": repo, "kind": "started", "payload": {"mode": "write", "class": "build"}}));
            evs.push(json!({"seq": evs.len() + 1, "at": format_epoch(st + 600, false, "%Y-%m-%dT%H:%M:%SZ"), "run": format!("r-{tag}"), "repo": repo, "kind": "delivered", "payload": {"mode": "write"}}));
            evs.push(json!({"seq": evs.len() + 1, "at": format_epoch(st + 900, false, "%Y-%m-%dT%H:%M:%SZ"), "run": format!("r-{tag}"), "repo": repo, "kind": "applied", "payload": {}}));
        }
        let ev_text = evs.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n") + "\n";
        fs::write(state.join("events.jsonl"), ev_text).unwrap();

        // Write online calibration records:
        // repo_a: 3/4 covered (75% healthy), repo_b: 1/4 covered (25% optimistic alert!)
        let mut cal_lines = Vec::new();
        for i in 0..4 {
            let at = format_epoch(now - 600 + i * 60, false, "%Y-%m-%dT%H:%M:%SZ");
            let actual_a = if i < 3 { 500.0 } else { 900.0 };
            cal_lines.push(json!({
                "at": at, "run": format!("ha-{i}"), "repo": repo_a, "repoName": "healthy-repo",
                "mode": "write", "class": "build", "checkpoint": "started", "milestone": "delivered",
                "predP50Sec": 500.0, "predP75Sec": 700.0, "actualSec": actual_a,
                "fallbackLevel": "L1", "isPrior": false
            }).to_string());
            let actual_b = if i == 0 { 500.0 } else { 1200.0 };
            cal_lines.push(json!({
                "at": at, "run": format!("ob-{i}"), "repo": repo_b, "repoName": "opt-repo",
                "mode": "write", "class": "build", "checkpoint": "started", "milestone": "delivered",
                "predP50Sec": 500.0, "predP75Sec": 700.0, "actualSec": actual_b,
                "fallbackLevel": "L1", "isPrior": false
            }).to_string());
        }
        fs::write(eta_calibration_path(&state), cal_lines.join("\n") + "\n").unwrap();

        let cal = analyze_calibration_in(&state, now, "14d", None).unwrap();
        assert_eq!(cal.total_records, 8);
        assert_eq!(cal.overall_started_delivered.count, 8);
        assert!((cal.overall_started_delivered.p75_coverage.unwrap() - 0.50).abs() < 1e-6);
        assert_eq!(cal.overall_started_delivered.p75_status, "optimistic");
        assert!(cal.overall_started_delivered.p75_alert);

        let reports = analyze_in(&state, now, "14d", None, false).unwrap();
        let human = format_human(&reports, "14d");
        assert!(human.contains("ETA p75覆盖"), "{human}");
        assert!(human.contains("75%(4)"), "{human}");
        assert!(human.contains("25%⚠(4)"), "{human}");

        // Deleting derived eta-calibration.jsonl degrades cleanly without error.
        fs::remove_file(eta_calibration_path(&state)).unwrap();
        let empty_cal = analyze_calibration_in(&state, now, "14d", None).unwrap();
        assert_eq!(empty_cal.total_records, 0);
        let reports_after = analyze_in(&state, now, "14d", None, false).unwrap();
        assert_eq!(reports_after.len(), 2);
        assert!(reports_after.iter().all(|r| r.eta_calibration.is_none()));

        let _ = fs::remove_dir_all(&state);
    }

    fn temp_dir_for_bt(tag: &str) -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "delegate-speed-bt-{tag}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
