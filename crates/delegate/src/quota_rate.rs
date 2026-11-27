//! Quota rate drift detection (`weighted_tokens / 1% quota drop`), read-only cache,
//! `quota_rate_shift` event emission, and `delegate usage --by quota-rate`.
//!
//! Design invariants:
//! - Strictly lock-free and zero-subprocess on all read paths (`snapshot`, `pulse`, `status`,
//!   `delegate usage --by quota-rate`, and `delegate-tui`).
//! - Never blocks Stop hook (`quota_rate_shift` is intentionally excluded from `ALERT_KINDS`)
//!   and never auto-switches accounts or imposes hard caps.
//! - Computes `fiveHour` and `weekly` rates independently because their per-1% scales differ
//!   significantly (~3x).

use crate::agy_accounts::{
    classify_model_pool, extract_pool_window_history, QuotaPool, QuotaWindow,
};
use crate::common::*;
use crate::usage::{parse_ledger_entries_from, TokenCounts, UsageEntry};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

pub const QUOTA_RATE_SHIFT_FILE: &str = "quota-rate-shift.json";
pub const QUOTA_RATE_SHIFT_SCHEMA_VERSION: u64 = 1;
pub const RATE_SHIFT_THRESHOLD: f64 = 1.5;
pub const MIN_CURRENT_DROP_PCT: f64 = 5.0;
pub const MIN_CURRENT_RECORDS: usize = 20;
pub const MIN_BASELINE_DROP_PCT: f64 = 5.0;
pub const MIN_BASELINE_RECORDS: usize = 20;
pub const BASELINE_LOOKBACK_SECS: i64 = 7 * 86400;
pub const CURRENT_WINDOW_SECS: i64 = 24 * 3600;
pub const RESET_EP_TOLERANCE_SECS: i64 = 600;
pub const CACHED_SHIFT_MAX_AGE_SECS: i64 = 48 * 3600;
pub const UNKNOWN_AGY_VERSION: &str = "unknown";
pub const UNRECORDED_USAGE_NOTE: &str = "可能含本机未记录的用量";

/// Weighted token formula: `input + 8 * output + 0.1 * cacheRead`.
pub fn weighted_tokens(tok: TokenCounts) -> f64 {
    tok.input.max(0) as f64
        + 8.0 * tok.output.max(0) as f64
        + 0.1 * tok.cache_read.max(0) as f64
}

pub fn rate_shift_cache_path(state_root: &Path) -> PathBuf {
    state_root.join(QUOTA_RATE_SHIFT_FILE)
}

#[derive(Debug, Clone, Default)]
struct WindowAccum {
    drop_pct: f64,
    weighted_tokens: f64,
    records: usize,
    versions: BTreeSet<String>,
}

impl WindowAccum {
    fn add_segment(&mut self, drop: f64, entries: &[&UsageEntry]) {
        self.drop_pct += drop;
        for e in entries {
            self.weighted_tokens += weighted_tokens(e.tokens);
            self.records += 1;
            match e
                .agy_version
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
            {
                Some(ver) => {
                    self.versions.insert(ver.to_string());
                }
                None => {
                    self.versions.insert(UNKNOWN_AGY_VERSION.to_string());
                }
            }
        }
    }

    fn rate(&self) -> Option<f64> {
        if self.drop_pct > 0.0 {
            Some(self.weighted_tokens / self.drop_pct)
        } else {
            None
        }
    }

    fn sorted_versions(&self) -> Vec<String> {
        sort_versions_set(&self.versions)
    }
}

fn sort_versions_set(set: &BTreeSet<String>) -> Vec<String> {
    let mut known: Vec<String> = set
        .iter()
        .filter(|v| v.as_str() != UNKNOWN_AGY_VERSION)
        .cloned()
        .collect();
    if set.contains(UNKNOWN_AGY_VERSION) {
        known.push(UNKNOWN_AGY_VERSION.to_string());
    }
    known
}

fn window_key_and_label(window: QuotaWindow) -> (&'static str, &'static str) {
    match window {
        QuotaWindow::FiveHour => ("fiveHour", "5h额度"),
        QuotaWindow::Weekly => ("weekly", "周额度"),
    }
}

fn same_reset_cycle(
    p0: f64,
    r0: Option<i64>,
    p1: f64,
    r1: Option<i64>,
) -> bool {
    if let (Some(a), Some(b)) = (r0.filter(|&v| v > 0), r1.filter(|&v| v > 0)) {
        if (b - a).abs() > RESET_EP_TOLERANCE_SECS {
            return false;
        }
    }
    p1 <= p0
}

fn range_indices(sorted: &[&UsageEntry], ep0: i64, ep1: i64) -> std::ops::Range<usize> {
    let start = sorted.partition_point(|e| e.epoch <= ep0);
    let end = sorted.partition_point(|e| e.epoch <= ep1);
    start..end
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaRateShift {
    pub account: String,
    pub pool: String,
    pub pool_label: String,
    pub window: String,
    pub window_label: String,
    pub baseline_rate: f64,
    pub current_rate: f64,
    pub ratio: f64,
    pub baseline_drop_pct: f64,
    pub current_drop_pct: f64,
    pub baseline_weighted_tokens: f64,
    pub current_weighted_tokens: f64,
    pub baseline_records: usize,
    pub current_records: usize,
    pub baseline_agy_versions: Vec<String>,
    pub current_agy_versions: Vec<String>,
    pub detected_at: String,
    pub detected_epoch: i64,
}

impl QuotaRateShift {
    pub fn to_json(&self) -> Value {
        json!({
            "account": self.account,
            "pool": self.pool,
            "poolLabel": self.pool_label,
            "window": self.window,
            "windowLabel": self.window_label,
            "baselineRate": self.baseline_rate.round(),
            "currentRate": self.current_rate.round(),
            "ratio": (self.ratio * 100.0).round() / 100.0,
            "baselineDropPct": (self.baseline_drop_pct * 100.0).round() / 100.0,
            "currentDropPct": (self.current_drop_pct * 100.0).round() / 100.0,
            "baselineWeightedTokens": self.baseline_weighted_tokens.round(),
            "currentWeightedTokens": self.current_weighted_tokens.round(),
            "baselineRecords": self.baseline_records,
            "currentRecords": self.current_records,
            "baselineAgyVersions": self.baseline_agy_versions,
            "currentAgyVersions": self.current_agy_versions,
            "detectedAt": self.detected_at,
            "detectedEpoch": self.detected_epoch,
            "note": UNRECORDED_USAGE_NOTE,
        })
    }

    pub fn to_event_payload(&self) -> Value {
        self.to_json()
    }
}

/// Detect Gemini-pool quota rate shifts per account at `now_ep`.
///
/// Baseline window: `[now_ep - 7d, now_ep - 24h]`.
/// Current window: `(now_ep - 24h, now_ep]`.
/// Triggers when `current_drop >= 5%`, `current_records >= 20`,
/// `baseline_drop >= 5%`, `baseline_records >= 20`, and `current_rate < baseline_rate / 1.5`.
pub fn detect_shifts(
    quota_cache: &Value,
    entries: &[UsageEntry],
    now_ep: i64,
) -> Vec<QuotaRateShift> {
    let Some(accts_map) = quota_cache["accounts"].as_object() else {
        return Vec::new();
    };
    let baseline_start = now_ep - BASELINE_LOOKBACK_SECS;
    let current_start = now_ep - CURRENT_WINDOW_SECS;
    let detected_at = format_epoch(now_ep, false, "%Y-%m-%dT%H:%M:%SZ");

    let mut acct_ids: Vec<&String> = accts_map.keys().collect();
    acct_ids.sort();

    // Group Gemini agy usage entries by account, sorted by epoch.
    let mut by_acct: HashMap<&str, Vec<&UsageEntry>> = HashMap::new();
    for e in entries {
        if !e.is_agy || e.epoch < baseline_start || e.epoch > now_ep {
            continue;
        }
        let Some(acct) = e.account.as_deref().filter(|a| !a.is_empty()) else {
            continue;
        };
        if classify_model_pool(&e.model) != Some(QuotaPool::Gemini) {
            continue;
        }
        by_acct.entry(acct).or_default().push(e);
    }
    for list in by_acct.values_mut() {
        list.sort_by_key(|e| e.epoch);
    }

    let pool = QuotaPool::Gemini;
    let mut out = Vec::new();

    for acct_id in acct_ids {
        let acct_quota = &accts_map[acct_id];
        let Some(acct_entries) = by_acct.get(acct_id.as_str()) else {
            continue;
        };
        let mut best_for_acct: Option<QuotaRateShift> = None;

        for window in [QuotaWindow::Weekly, QuotaWindow::FiveHour] {
            let pts = extract_pool_window_history(acct_quota, pool, window);
            if pts.len() < 2 {
                continue;
            }
            let mut baseline = WindowAccum::default();
            let mut current = WindowAccum::default();

            for pair in pts.windows(2) {
                let (ep0, p0, r0) = pair[0];
                let (ep1, p1, r1) = pair[1];
                if ep1 <= ep0 || ep1 < baseline_start || ep1 > now_ep {
                    continue;
                }
                if !same_reset_cycle(p0, r0, p1, r1) {
                    continue;
                }
                let drop = (p0 - p1).max(0.0);
                let rng = range_indices(acct_entries, ep0, ep1);
                let seg_entries = &acct_entries[rng];
                if ep1 > current_start {
                    current.add_segment(drop, seg_entries);
                } else {
                    baseline.add_segment(drop, seg_entries);
                }
            }

            if current.drop_pct < MIN_CURRENT_DROP_PCT
                || current.records < MIN_CURRENT_RECORDS
                || baseline.drop_pct < MIN_BASELINE_DROP_PCT
                || baseline.records < MIN_BASELINE_RECORDS
            {
                continue;
            }
            let (Some(base_rate), Some(cur_rate)) = (baseline.rate(), current.rate()) else {
                continue;
            };
            if base_rate <= 0.0 || cur_rate <= 0.0 {
                continue;
            }
            if cur_rate * RATE_SHIFT_THRESHOLD < base_rate {
                let ratio = base_rate / cur_rate;
                let (win_key, win_label) = window_key_and_label(window);
                let candidate = QuotaRateShift {
                    account: acct_id.clone(),
                    pool: pool.key().to_string(),
                    pool_label: pool.short_label().to_string(),
                    window: win_key.to_string(),
                    window_label: win_label.to_string(),
                    baseline_rate: base_rate,
                    current_rate: cur_rate,
                    ratio,
                    baseline_drop_pct: baseline.drop_pct,
                    current_drop_pct: current.drop_pct,
                    baseline_weighted_tokens: baseline.weighted_tokens,
                    current_weighted_tokens: current.weighted_tokens,
                    baseline_records: baseline.records,
                    current_records: current.records,
                    baseline_agy_versions: baseline.sorted_versions(),
                    current_agy_versions: current.sorted_versions(),
                    detected_at: detected_at.clone(),
                    detected_epoch: now_ep,
                };
                if best_for_acct
                    .as_ref()
                    .is_none_or(|prev| candidate.ratio > prev.ratio)
                {
                    best_for_acct = Some(candidate);
                }
            }
        }

        if let Some(shift) = best_for_acct {
            out.push(shift);
        }
    }

    out
}

/// Write-path / `_gc-worker` updater: recomputes quota rate shifts, emits at most one
/// `quota_rate_shift` event per account per local day, and atomically writes
/// `<state>/quota-rate-shift.json` for lock-free read-only consumers.
///
/// Callers MUST NOT hold `.state.lock` when calling this function because `events::emit_in`
/// acquires `.state.lock`.
pub fn refresh_and_emit_in(state_root: &Path, now_ep: i64) -> Vec<QuotaRateShift> {
    let qpath = state_root.join("agy-quota.json");
    let upath = state_root.join("agy-usage.jsonl");
    if !qpath.is_file() || !upath.is_file() {
        return Vec::new();
    }
    let quota_cache = json(&qpath);
    if !quota_cache.is_object() {
        return Vec::new();
    }
    let (entries, _) = parse_ledger_entries_from(&upath, Some(now_ep - BASELINE_LOOKBACK_SECS), false);
    let shifts = detect_shifts(&quota_cache, &entries, now_ep);

    let cache_path = rate_shift_cache_path(state_root);
    let prev = json(&cache_path);
    let mut emitted_by_acct: BTreeMap<String, String> = BTreeMap::new();
    if let Some(map) = prev["emittedByAccountDay"].as_object() {
        for (k, v) in map {
            if let Some(day) = v.as_str().filter(|d| !d.is_empty()) {
                emitted_by_acct.insert(k.clone(), day.to_string());
            }
        }
    }

    let today_ymd = format_epoch(now_ep, true, "%Y-%m-%d");
    for shift in &shifts {
        let already = emitted_by_acct
            .get(&shift.account)
            .is_some_and(|d| d == &today_ymd);
        if !already
            && crate::events::emit_in(
                state_root,
                "",
                "",
                "quota_rate_shift",
                shift.to_event_payload(),
            )
            .is_ok()
        {
            emitted_by_acct.insert(shift.account.clone(), today_ymd.clone());
        }
    }

    let shifts_json: Vec<Value> = shifts.iter().map(QuotaRateShift::to_json).collect();
    let emitted_map: Map<String, Value> = emitted_by_acct
        .into_iter()
        .map(|(k, v)| (k, json!(v)))
        .collect();
    let doc = json!({
        "schemaVersion": QUOTA_RATE_SHIFT_SCHEMA_VERSION,
        "computedAt": format_epoch(now_ep, false, "%Y-%m-%dT%H:%M:%SZ"),
        "computedEpoch": now_ep,
        "shifts": shifts_json,
        "emittedByAccountDay": emitted_map,
    });
    if let Some(parent) = cache_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = atomic_write(&cache_path, format!("{doc}\n"));
    shifts
}

/// Strictly read-only, zero-lock, zero-subprocess loader of active cached rate shifts.
pub fn load_cached_shifts_in(state_root: &Path, now_ep: i64) -> Vec<Value> {
    let cache_path = rate_shift_cache_path(state_root);
    if !cache_path.is_file() {
        return Vec::new();
    }
    let doc = json(&cache_path);
    let Some(arr) = doc["shifts"].as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter(|item| {
            if !item.is_object() {
                return false;
            }
            let det = item["detectedEpoch"].as_i64().unwrap_or(0);
            if now_ep > 0 && det > 0 && now_ep - det > CACHED_SHIFT_MAX_AGE_SECS {
                return false;
            }
            true
        })
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Formatting Helpers (shared by events, pulse, CLI, and tests)
// ---------------------------------------------------------------------------

pub fn format_rate_compact(rate: f64) -> String {
    if !rate.is_finite() || rate <= 0.0 {
        return "-".to_string();
    }
    if rate >= 1_000_000.0 {
        format!("{:.2}M", rate / 1_000_000.0)
    } else if rate >= 1_000.0 {
        format!("{:.0}K", rate / 1_000.0)
    } else {
        format!("{:.0}", rate)
    }
}

pub fn format_versions_human(vers: &[String]) -> String {
    if vers.is_empty() {
        return "未知".to_string();
    }
    let parts: Vec<&str> = vers
        .iter()
        .map(|v| {
            if v == UNKNOWN_AGY_VERSION || v.is_empty() {
                "未知"
            } else {
                v.as_str()
            }
        })
        .collect();
    parts.join(",")
}

pub fn format_version_transition(base: &[String], cur: &[String]) -> String {
    if base.is_empty() && cur.is_empty() {
        return String::new();
    }
    let b = format_versions_human(base);
    let c = format_versions_human(cur);
    if b == c {
        format!(" · agy {c}")
    } else {
        format!(" · agy {b}→{c}")
    }
}

pub fn format_shift_one_line(shift: &Value, include_account: bool) -> String {
    let acct = s(shift, "account");
    let pool = {
        let lbl = s(shift, "poolLabel");
        if !lbl.is_empty() {
            lbl
        } else {
            s(shift, "pool")
        }
    };
    let win = {
        let lbl = s(shift, "windowLabel");
        if !lbl.is_empty() {
            lbl
        } else {
            s(shift, "window")
        }
    };
    let base = shift["baselineRate"].as_f64().unwrap_or(0.0);
    let cur = shift["currentRate"].as_f64().unwrap_or(0.0);
    let ratio = shift["ratio"].as_f64().unwrap_or(0.0);
    let b_vers = strings(&shift["baselineAgyVersions"]);
    let c_vers = strings(&shift["currentAgyVersions"]);
    let ver_part = format_version_transition(&b_vers, &c_vers);
    let acct_part = if include_account && !acct.is_empty() {
        format!("{acct} ")
    } else {
        String::new()
    };
    format!(
        "⚠ 费率漂移：{acct_part}{pool}({win}) {}→{}/1%（降为1/{ratio:.2}{ver_part}；{UNRECORDED_USAGE_NOTE}）",
        format_rate_compact(base),
        format_rate_compact(cur),
    )
}

// ---------------------------------------------------------------------------
// Daily Quota Rate Table (`delegate usage --by quota-rate`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct DailyWindowRate {
    pub drop_pct: f64,
    pub weighted_tokens: f64,
    pub rate_per_pct: Option<f64>,
    pub records: usize,
    pub agy_versions: Vec<String>,
}

impl DailyWindowRate {
    fn from_accum(a: WindowAccum) -> Self {
        let rate_per_pct = a.rate();
        let agy_versions = a.sorted_versions();
        Self {
            drop_pct: a.drop_pct,
            weighted_tokens: a.weighted_tokens,
            rate_per_pct,
            records: a.records,
            agy_versions,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "dropPct": (self.drop_pct * 100.0).round() / 100.0,
            "weightedTokens": self.weighted_tokens.round(),
            "ratePerPct": self.rate_per_pct.map(f64::round),
            "records": self.records,
            "agyVersions": self.agy_versions,
        })
    }
}

#[derive(Debug, Clone)]
pub struct DailyRateRow {
    pub account: String,
    pub pool: String,
    pub pool_label: String,
    pub date: String,
    pub five_hour: DailyWindowRate,
    pub weekly: DailyWindowRate,
    pub agy_versions: Vec<String>,
}

impl DailyRateRow {
    pub fn to_json(&self) -> Value {
        json!({
            "account": self.account,
            "pool": self.pool,
            "poolLabel": self.pool_label,
            "date": self.date,
            "fiveHour": self.five_hour.to_json(),
            "weekly": self.weekly.to_json(),
            "agyVersions": self.agy_versions,
        })
    }
}

/// Compute per-account, per-pool, per-local-day quota rate rows (strictly pure/read-only).
pub fn compute_daily_rates(
    quota_cache: &Value,
    entries: &[UsageEntry],
    since_epoch: Option<i64>,
    account_filter: Option<&str>,
) -> Vec<DailyRateRow> {
    let Some(accts_map) = quota_cache["accounts"].as_object() else {
        return Vec::new();
    };
    let mut acct_ids: Vec<&String> = accts_map
        .keys()
        .filter(|id| account_filter.is_none_or(|f| id.as_str() == f))
        .collect();
    acct_ids.sort();

    // Group agy usage entries by (account, pool), sorted by epoch.
    let mut by_acct_pool: HashMap<(&str, QuotaPool), Vec<&UsageEntry>> = HashMap::new();
    for e in entries {
        if !e.is_agy {
            continue;
        }
        let Some(acct) = e.account.as_deref().filter(|a| !a.is_empty()) else {
            continue;
        };
        if account_filter.is_some_and(|f| f != acct) {
            continue;
        }
        let Some(pool) = classify_model_pool(&e.model) else {
            continue;
        };
        by_acct_pool.entry((acct, pool)).or_default().push(e);
    }
    for list in by_acct_pool.values_mut() {
        list.sort_by_key(|e| e.epoch);
    }

    let mut rows = Vec::new();
    for acct_id in acct_ids {
        let acct_quota = &accts_map[acct_id];
        for pool in [QuotaPool::Gemini, QuotaPool::ClaudeGpt] {
            let empty_vec: Vec<&UsageEntry> = Vec::new();
            let pool_entries = by_acct_pool
                .get(&(acct_id.as_str(), pool))
                .unwrap_or(&empty_vec);

            let mut day_5h: BTreeMap<String, WindowAccum> = BTreeMap::new();
            let mut day_wk: BTreeMap<String, WindowAccum> = BTreeMap::new();

            for (window, target) in [
                (QuotaWindow::FiveHour, &mut day_5h),
                (QuotaWindow::Weekly, &mut day_wk),
            ] {
                let pts = extract_pool_window_history(acct_quota, pool, window);
                for pair in pts.windows(2) {
                    let (ep0, p0, r0) = pair[0];
                    let (ep1, p1, r1) = pair[1];
                    if ep1 <= ep0 {
                        continue;
                    }
                    if since_epoch.is_some_and(|cut| ep1 < cut) {
                        continue;
                    }
                    if !same_reset_cycle(p0, r0, p1, r1) {
                        continue;
                    }
                    let drop = (p0 - p1).max(0.0);
                    let rng = range_indices(pool_entries, ep0, ep1);
                    let seg_entries = &pool_entries[rng];
                    let day = format_epoch(ep1, true, "%Y-%m-%d");
                    target.entry(day).or_default().add_segment(drop, seg_entries);
                }
            }

            let all_days: BTreeSet<String> = day_5h
                .keys()
                .chain(day_wk.keys())
                .cloned()
                .collect();

            // Skip non-Gemini pools unless they observed both quota drops and local usage.
            if pool != QuotaPool::Gemini {
                let has_data = day_5h
                    .values()
                    .chain(day_wk.values())
                    .any(|a| a.drop_pct > 0.0 && a.records > 0);
                if !has_data {
                    continue;
                }
            }

            for date in all_days {
                let fh = DailyWindowRate::from_accum(day_5h.remove(&date).unwrap_or_default());
                let wk = DailyWindowRate::from_accum(day_wk.remove(&date).unwrap_or_default());
                if fh.drop_pct <= 0.0 && wk.drop_pct <= 0.0 {
                    continue;
                }
                let mut ver_set: BTreeSet<String> = BTreeSet::new();
                for v in fh.agy_versions.iter().chain(wk.agy_versions.iter()) {
                    ver_set.insert(v.clone());
                }
                rows.push(DailyRateRow {
                    account: acct_id.clone(),
                    pool: pool.key().to_string(),
                    pool_label: pool.short_label().to_string(),
                    date,
                    five_hour: fh,
                    weekly: wk,
                    agy_versions: sort_versions_set(&ver_set),
                });
            }
        }
    }

    rows
}

fn col_display_width(text: &str) -> usize {
    text.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

fn pad_right_dw(text: &str, width: usize) -> String {
    let w = col_display_width(text);
    if w >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - w))
    }
}

fn pad_left_dw(text: &str, width: usize) -> String {
    let w = col_display_width(text);
    if w >= width {
        text.to_string()
    } else {
        format!("{}{text}", " ".repeat(width - w))
    }
}

/// Strictly read-only, lock-free, zero-subprocess handler for `delegate usage --by quota-rate`.
pub fn run_cli_quota_rate(
    since_str: &str,
    since_epoch: Option<i64>,
    since_iso: &str,
    account_filter: Option<&str>,
    json_out: bool,
    now_ep: i64,
) -> Res<i32> {
    let state = state_dir();
    let qpath = state.join("agy-quota.json");
    let upath = state.join("agy-usage.jsonl");
    let quota_cache = json(&qpath);
    // Load enough usage history to cover both the requested `--since` range and the 7d shift window.
    let load_since = since_epoch.map(|cut| cut.min(now_ep - BASELINE_LOOKBACK_SECS));
    let (entries, _) = parse_ledger_entries_from(&upath, load_since, false);
    let rows = compute_daily_rates(&quota_cache, &entries, since_epoch, account_filter);
    let mut shifts = detect_shifts(&quota_cache, &entries, now_ep);
    if let Some(af) = account_filter {
        shifts.retain(|sh| sh.account == af);
    }

    if json_out {
        let rows_json: Vec<Value> = rows.iter().map(DailyRateRow::to_json).collect();
        let shifts_json: Vec<Value> = shifts.iter().map(QuotaRateShift::to_json).collect();
        let out_doc = json!({
            "since": since_str,
            "sinceIso": since_iso,
            "by": "quota-rate",
            "formula": "input + 8*output + 0.1*cacheRead",
            "note": UNRECORDED_USAGE_NOTE,
            "generatedAt": format_epoch(now_ep, false, "%Y-%m-%dT%H:%M:%SZ"),
            "rows": rows_json,
            "shifts": shifts_json,
        });
        println!("{out_doc}");
        return Ok(0);
    }

    println!("agy 额度费率表（范围: {since_str}，加权token/1%，仅本机 agy-usage.jsonl）:");
    if rows.is_empty() {
        println!("  （所选时间段内无可用的额度历史与用量匹配记录）");
    } else {
        let acct_w = rows
            .iter()
            .map(|r| col_display_width(&r.account))
            .max()
            .unwrap_or(8)
            .clamp(8, 24);
        let pool_w = rows
            .iter()
            .map(|r| col_display_width(&r.pool_label))
            .max()
            .unwrap_or(6)
            .clamp(6, 12);
        println!(
            "  {}  {}  {}  {}  {}  {}  {}  {}  agyVersion",
            pad_right_dw("账号", acct_w),
            pad_right_dw("池", pool_w),
            pad_right_dw("日期", 10),
            pad_left_dw("5h下降", 7),
            pad_left_dw("5h费率(/1%)", 11),
            pad_left_dw("周下降", 7),
            pad_left_dw("周费率(/1%)", 11),
            pad_left_dw("记录", 6),
        );
        for r in &rows {
            let fh_drop = if r.five_hour.drop_pct > 0.0 {
                format!("{:.1}%", r.five_hour.drop_pct)
            } else {
                "-".to_string()
            };
            let fh_rate = r
                .five_hour
                .rate_per_pct
                .map(format_rate_compact)
                .unwrap_or_else(|| "-".to_string());
            let wk_drop = if r.weekly.drop_pct > 0.0 {
                format!("{:.1}%", r.weekly.drop_pct)
            } else {
                "-".to_string()
            };
            let wk_rate = r
                .weekly
                .rate_per_pct
                .map(format_rate_compact)
                .unwrap_or_else(|| "-".to_string());
            let recs = r.five_hour.records.max(r.weekly.records);
            let vers = format_versions_human(&r.agy_versions);
            println!(
                "  {}  {}  {}  {}  {}  {}  {}  {}  {}",
                pad_right_dw(&clip(&r.account, acct_w), acct_w),
                pad_right_dw(&r.pool_label, pool_w),
                pad_right_dw(&r.date, 10),
                pad_left_dw(&fh_drop, 7),
                pad_left_dw(&fh_rate, 11),
                pad_left_dw(&wk_drop, 7),
                pad_left_dw(&wk_rate, 11),
                pad_left_dw(&recs.to_string(), 6),
                vers,
            );
        }
    }

    if !shifts.is_empty() {
        println!();
        for sh in &shifts {
            println!("  {}", format_shift_one_line(&sh.to_json(), true));
        }
    }
    println!(
        "注：加权 token = input + 8×output + 0.1×cacheRead；{UNRECORDED_USAGE_NOTE}（其他机器或手动使用）。"
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_fixture_entries(arr: &[Value]) -> Vec<UsageEntry> {
        arr.iter()
            .map(|v| UsageEntry {
                epoch: v["epoch"].as_i64().unwrap(),
                run: s(v, "run").to_string(),
                repo: "/repo".into(),
                repo_name: "repo".into(),
                role: "author".into(),
                mode: "write".into(),
                rework: false,
                model: s(v, "model").to_string(),
                is_agy: true,
                account: v["account"].as_str().map(str::to_string),
                tokens: TokenCounts::from_json(&v["tokens"]),
                agy_version: v["agyVersion"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                live: false,
            })
            .collect()
    }

    #[test]
    fn replay_fixture_triggers_4x_shift_and_ignores_sparse_account() {
        let raw = include_str!("../tests/fixtures/quota_rate_shift_replay.json");
        assert!(!raw.contains('@'), "fixture must be desensitized (no emails)");
        let doc: Value = serde_json::from_str(raw).expect("valid fixture json");
        let now_ep = doc["nowEpoch"].as_i64().unwrap();
        let quota_cache = &doc["quotaCache"];
        let entries = parse_fixture_entries(doc["usageEntries"].as_array().unwrap());

        let shifts = detect_shifts(quota_cache, &entries, now_ep);
        assert_eq!(shifts.len(), 1, "expected only acct-alpha to trigger: {shifts:?}");
        let sh = &shifts[0];
        assert_eq!(sh.account, "acct-alpha");
        assert_eq!(sh.pool, "gemini");
        assert!(
            (sh.ratio - 4.0).abs() < 0.15,
            "expected ~4x shift ratio, got {}",
            sh.ratio
        );
        assert_eq!(sh.baseline_agy_versions, vec!["1.5.16".to_string()]);
        assert_eq!(sh.current_agy_versions, vec!["1.5.17".to_string()]);
        let line = format_shift_one_line(&sh.to_json(), true);
        assert!(line.contains("acct-alpha"));
        assert!(line.contains("1.5.16→1.5.17"));
        assert!(line.contains(UNRECORDED_USAGE_NOTE));
    }

    #[test]
    fn refresh_and_emit_deduplicates_same_day_and_read_path_is_pure() {
        let raw = include_str!("../tests/fixtures/quota_rate_shift_replay.json");
        let doc: Value = serde_json::from_str(raw).expect("valid fixture json");
        let now_ep = doc["nowEpoch"].as_i64().unwrap();

        let tmp = std::env::temp_dir().join(format!(
            "delegate-quota-rate-test-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&tmp).unwrap();
        atomic_write(tmp.join("agy-quota.json"), format!("{}\n", doc["quotaCache"])).unwrap();
        let mut usage_lines = String::new();
        for u in doc["usageEntries"].as_array().unwrap() {
            usage_lines.push_str(&format!("{u}\n"));
        }
        atomic_write(tmp.join("agy-usage.jsonl"), usage_lines).unwrap();

        let s1 = refresh_and_emit_in(&tmp, now_ep);
        assert_eq!(s1.len(), 1);
        let evs1 = crate::events::read_since_in(&tmp, 0);
        let shift_evs1: Vec<&Value> = evs1
            .iter()
            .filter(|e| s(e, "kind") == "quota_rate_shift")
            .collect();
        assert_eq!(shift_evs1.len(), 1);
        assert!(!crate::events::is_alert_event(shift_evs1[0]));
        assert_eq!(s(&shift_evs1[0]["payload"], "note"), UNRECORDED_USAGE_NOTE);

        // Repeat in same window/day -> MUST NOT emit duplicate event.
        let s2 = refresh_and_emit_in(&tmp, now_ep + 60);
        assert_eq!(s2.len(), 1);
        let evs2 = crate::events::read_since_in(&tmp, 0);
        let shift_evs2: Vec<&Value> = evs2
            .iter()
            .filter(|e| s(e, "kind") == "quota_rate_shift")
            .collect();
        assert_eq!(shift_evs2.len(), 1, "must not duplicate event within same day");

        // Read-only cached load returns the shift and never emits.
        let cached = load_cached_shifts_in(&tmp, now_ep + 120);
        assert_eq!(cached.len(), 1);
        assert_eq!(s(&cached[0], "account"), "acct-alpha");
        let evs3 = crate::events::read_since_in(&tmp, 0);
        assert_eq!(evs3.len(), evs2.len());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn missing_agy_version_treated_as_unknown_and_resets_excluded() {
        let base_ep = 1_795_800_000i64;
        let reset_a = base_ep + 10_000;
        let reset_b = base_ep + 50_000;
        let quota_cache = json!({
            "accounts": {
                "acct-legacy": {
                    "history": [
                        // Baseline cycle (2 days ago): 100 -> 90 (10% drop)
                        { "epoch": base_ep - 2 * 86400, "pools": { "Gemini Models": { "weekly": 100.0, "weeklyResetEp": reset_a } } },
                        { "epoch": base_ep - 2 * 86400 + 3600, "pools": { "Gemini Models": { "weekly": 90.0, "weeklyResetEp": reset_a } } },
                        // Reset jump (different resetEp & pct increased): MUST be skipped!
                        { "epoch": base_ep - 12 * 3600, "pools": { "Gemini Models": { "weekly": 100.0, "weeklyResetEp": reset_b } } },
                        // Current cycle: 100 -> 80 (20% drop)
                        { "epoch": base_ep - 6 * 3600, "pools": { "Gemini Models": { "weekly": 80.0, "weeklyResetEp": reset_b } } }
                    ]
                }
            }
        });
        let mut entries = Vec::new();
        // 20 baseline records (no agyVersion -> unknown): 2M weighted each = 40M / 10% = 4M/1%
        for i in 0..20 {
            entries.push(UsageEntry {
                epoch: base_ep - 2 * 86400 + 100 + i * 100,
                run: format!("b-{i}"),
                repo: "/repo".into(),
                repo_name: "repo".into(),
                role: "author".into(),
                mode: "write".into(),
                rework: false,
                model: "agy/gemini-4-neon".into(),
                is_agy: true,
                account: Some("acct-legacy".into()),
                tokens: TokenCounts::new(1_000_000, 125_000, 0), // 1M + 1M = 2M
                agy_version: None,
                live: false,
            });
        }
        // Noise during reset segment (should be ignored!)
        entries.push(UsageEntry {
            epoch: base_ep - 18 * 3600,
            run: "reset-noise".into(),
            repo: "/repo".into(),
            repo_name: "repo".into(),
            role: "author".into(),
            mode: "write".into(),
            rework: false,
            model: "agy/gemini-4-neon".into(),
            is_agy: true,
            account: Some("acct-legacy".into()),
            tokens: TokenCounts::new(900_000_000, 0, 0),
            agy_version: Some("9.9.9".into()),
            live: false,
        });
        // 20 current records (agyVersion 1.5.18): 1M weighted each = 20M / 20% = 1M/1% (4x drop)
        for i in 0..20 {
            entries.push(UsageEntry {
                epoch: base_ep - 11 * 3600 + i * 100,
                run: format!("c-{i}"),
                repo: "/repo".into(),
                repo_name: "repo".into(),
                role: "author".into(),
                mode: "write".into(),
                rework: false,
                model: "agy/gemini-4-neon".into(),
                is_agy: true,
                account: Some("acct-legacy".into()),
                tokens: TokenCounts::new(1_000_000, 0, 0),
                agy_version: Some("1.5.18".into()),
                live: false,
            });
        }

        let shifts = detect_shifts(&quota_cache, &entries, base_ep);
        assert_eq!(shifts.len(), 1);
        let sh = &shifts[0];
        assert_eq!(sh.baseline_agy_versions, vec![UNKNOWN_AGY_VERSION.to_string()]);
        assert_eq!(sh.current_agy_versions, vec!["1.5.18".to_string()]);
        assert!((sh.ratio - 4.0).abs() < 0.01);
        assert!(format_shift_one_line(&sh.to_json(), true).contains("未知→1.5.18"));
    }

    #[test]
    fn insufficient_drop_or_records_never_triggers() {
        let base_ep = 1_795_800_000i64;
        let reset_ep = base_ep + 100_000;
        // Case A: 4% drop (< 5%), 25 records -> must not trigger.
        let qc_low_drop = json!({
            "accounts": {
                "a": {
                    "history": [
                        { "epoch": base_ep - 2 * 86400, "pools": { "Gemini Models": { "weekly": 100.0, "weeklyResetEp": reset_ep } } },
                        { "epoch": base_ep - 2 * 86400 + 3600, "pools": { "Gemini Models": { "weekly": 90.0, "weeklyResetEp": reset_ep } } },
                        { "epoch": base_ep - 6 * 3600, "pools": { "Gemini Models": { "weekly": 90.0, "weeklyResetEp": reset_ep } } },
                        { "epoch": base_ep - 3600, "pools": { "Gemini Models": { "weekly": 86.0, "weeklyResetEp": reset_ep } } }
                    ]
                }
            }
        });
        let mut entries = Vec::new();
        for i in 0..25 {
            entries.push(UsageEntry {
                epoch: base_ep - 2 * 86400 + 60 + i * 60,
                run: format!("b-{i}"),
                repo: "/r".into(),
                repo_name: "r".into(),
                role: "author".into(),
                mode: "write".into(),
                rework: false,
                model: "agy/gemini-4-neon".into(),
                is_agy: true,
                account: Some("a".into()),
                tokens: TokenCounts::new(2_000_000, 0, 0),
                agy_version: Some("1.5.16".into()),
                live: false,
            });
            entries.push(UsageEntry {
                epoch: base_ep - 5 * 3600 + i * 60,
                run: format!("c-{i}"),
                repo: "/r".into(),
                repo_name: "r".into(),
                role: "author".into(),
                mode: "write".into(),
                rework: false,
                model: "agy/gemini-4-neon".into(),
                is_agy: true,
                account: Some("a".into()),
                tokens: TokenCounts::new(100_000, 0, 0),
                agy_version: Some("1.5.17".into()),
                live: false,
            });
        }
        assert!(detect_shifts(&qc_low_drop, &entries, base_ep).is_empty());

        // Case B: 20% drop, only 19 records (< 20) -> must not trigger.
        let qc_low_recs = json!({
            "accounts": {
                "a": {
                    "history": [
                        { "epoch": base_ep - 2 * 86400, "pools": { "Gemini Models": { "weekly": 100.0, "weeklyResetEp": reset_ep } } },
                        { "epoch": base_ep - 2 * 86400 + 3600, "pools": { "Gemini Models": { "weekly": 90.0, "weeklyResetEp": reset_ep } } },
                        { "epoch": base_ep - 6 * 3600, "pools": { "Gemini Models": { "weekly": 90.0, "weeklyResetEp": reset_ep } } },
                        { "epoch": base_ep - 3600, "pools": { "Gemini Models": { "weekly": 70.0, "weeklyResetEp": reset_ep } } }
                    ]
                }
            }
        });
        let entries_19: Vec<UsageEntry> = entries
            .into_iter()
            .filter(|e| !e.run.starts_with("c-") || e.run[2..].parse::<usize>().unwrap() < 19)
            .collect();
        assert!(detect_shifts(&qc_low_recs, &entries_19, base_ep).is_empty());
    }
}
