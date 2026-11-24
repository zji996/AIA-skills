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

#[derive(Debug, Clone)]
struct EventRec {
    ts: i64,
    run: String,
    repo: String,
    kind: String,
    payload: Value,
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
    first_started_ts: Option<i64>,
    first_applied_ts: Option<i64>,
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
            first_started_ts: None,
            first_applied_ts: None,
            last_finished_ts: None,
            started_runs: HashSet::new(),
            rework_sent_count: 0,
            has_premerge_conflict: false,
            has_premerge_failed: false,
            ready_timestamps: Vec::new(),
            runs: HashMap::new(),
        }
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
        doc
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

fn sec_to_min(sec: f64) -> f64 {
    round2(sec / 60.0)
}

fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
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

fn short_repo_name(repo: &str) -> String {
    Path::new(repo)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(repo)
        .to_string()
}

fn repo_matches_filter(repo_path: &str, repo_name: &str, filter: &str) -> bool {
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

fn parse_events(raw: Vec<Value>) -> Vec<EventRec> {
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
        "{:<16} {:<5} {:<8} {:<18} {:<8} {:<13} {:<15} {:<15} {:<7} {:<14} {}\n",
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
        "只读 50/75(n)"
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

        out.push_str(&format!(
            "{:<16} {:<5} {:<8} {:<18} {:<8} {:<13} {:<15} {:<15} {:<7} {:<14} {}\n",
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
    }

    out
}

/// CLI entrypoint: `delegate speed [--since 7d|14d|all] [--repo PATH] [--json]`
pub fn command(args: &[String]) -> Res<i32> {
    let (pos, flags, kv) = parse_simple(args, &["--json"], &["--since", "--repo"])?;
    if !pos.is_empty() {
        return Err(format!("unrecognized arguments: {}", pos.join(" ")));
    }
    let json_out = has(&flags, "--json");
    let since_str = value(&kv, "--since").unwrap_or("7d");
    let repo_filter = value(&kv, "--repo");

    let now_ts = epoch() as i64;
    let state = state_dir();
    let reports = analyze_in(&state, now_ts, since_str, repo_filter, true)?;

    if json_out {
        let repos_json: Vec<Value> = reports.iter().map(|r| r.to_full_json()).collect();
        let doc = json!({
            "at": format_epoch(now_ts, false, "%Y-%m-%dT%H:%M:%SZ"),
            "requestedSince": since_str,
            "repos": repos_json,
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        print!("{}", format_human(&reports, since_str));
    }
    Ok(0)
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
}
