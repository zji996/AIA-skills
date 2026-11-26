//! Value-quota auto-fill helpers and `delegate fill stats [--since 24h]` telemetry.
//!
//! All metrics follow Principle P3 (measure work outcomes and ratios, not raw wall seconds)
//! and ADR 0120 (stop lines alert only — never auto-downgrade `fill.target`).
use crate::common::*;
use crate::config::{self, FillConfig};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const STOP_CONFLICT_RATE: f64 = 0.10;
pub const STOP_REWORK_RATE: f64 = 0.283;
pub const STOP_PEAK_UNMERGED: usize = 5;
pub const STOP_LANE_QUEUE_SHARE: f64 = 0.15;

/// Maximum concurrent `infra` + `docs` write runs allowed for a given `target` and
/// `maxInfraDocsShare`. Uses nearest integer rounding (clamped to `0..=target`) so
/// `target = 6, maxInfraDocsShare = 0.3` yields `round(1.8) = 2`.
pub fn max_infra_docs_slots(target: usize, max_share: f64) -> usize {
    if target == 0 || !max_share.is_finite() || max_share <= 0.0 {
        return 0;
    }
    let clamped = max_share.clamp(0.0, 1.0);
    ((target as f64) * clamped).round().min(target as f64) as usize
}

/// Normalize a repo-relative path prefix for hotspot comparison.
pub fn normalize_rel_path(raw: &str) -> String {
    raw.trim().trim_start_matches("./").to_string()
}

/// True when a declared change path prefix `prefix` touches or contains `hotspot`.
pub fn path_prefix_hits_hotspot(prefix: &str, hotspot: &str) -> bool {
    let p = normalize_rel_path(prefix);
    let h = normalize_rel_path(hotspot);
    if p.is_empty() || h.is_empty() {
        return false;
    }
    if p == h {
        return true;
    }
    let p_dir = p.trim_end_matches('/');
    if !p_dir.is_empty()
        && h.len() > p_dir.len()
        && h.starts_with(p_dir)
        && h.as_bytes()[p_dir.len()] == b'/'
    {
        return true;
    }
    let h_dir = h.trim_end_matches('/');
    if !h_dir.is_empty()
        && p.len() > h_dir.len()
        && p.starts_with(h_dir)
        && p.as_bytes()[h_dir.len()] == b'/'
    {
        return true;
    }
    false
}

/// Collect hotspots from `hotspots` that are touched by any path in `active_files`.
pub fn active_hotspots_from_files(active_files: &[String], hotspots: &[String]) -> HashSet<String> {
    let mut active = HashSet::new();
    for h in hotspots {
        let h_norm = normalize_rel_path(h);
        if h_norm.is_empty() {
            continue;
        }
        if active_files
            .iter()
            .any(|p| path_prefix_hits_hotspot(p, &h_norm))
        {
            active.insert(h_norm);
        }
    }
    active
}

/// True when any path in `cand_files` hits a hotspot currently held by an active run.
pub fn candidate_hits_active_hotspot(
    cand_files: &[String],
    active_hotspots: &HashSet<String>,
) -> bool {
    if active_hotspots.is_empty() || cand_files.is_empty() {
        return false;
    }
    cand_files.iter().any(|p| {
        active_hotspots
            .iter()
            .any(|h| path_prefix_hits_hotspot(p, h))
    })
}

#[derive(Clone, Debug)]
pub struct UsageRow {
    pub epoch: i64,
    pub run: String,
    pub repo: String,
    pub mode: String,
    pub is_agy: bool,
    pub total_tokens: i64,
}

#[derive(Clone, Debug)]
pub struct FillStats {
    pub since: String,
    pub since_seconds: Option<i64>,
    pub repo: Option<String>,
    pub target: Option<usize>,
    pub max_infra_docs_share: Option<f64>,
    pub fill_dispatched: usize,
    pub write_deliveries: usize,
    pub premerge_conflicts: usize,
    pub premerge_denom: usize,
    pub premerge_conflict_rate: f64,
    pub rework_sent: usize,
    pub rework_rate: f64,
    pub peak_unmerged: usize,
    pub lane_queue_share: f64,
    pub lane_events: usize,
    pub agy_weekly_pct_consumed: Option<f64>,
    pub agy_weekly_pct_per_10: Option<f64>,
    pub readonly_runs: usize,
    pub readonly_agy_tokens: i64,
    pub total_agy_tokens: i64,
    pub readonly_agy_share: Option<f64>,
    pub conflict_triggered: bool,
    pub rework_triggered: bool,
    pub unmerged_triggered: bool,
    pub lane_triggered: bool,
}

impl FillStats {
    pub fn triggered_labels(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.conflict_triggered {
            out.push(format!(
                "premerge 冲突率 {:.1}% > 10%",
                self.premerge_conflict_rate * 100.0
            ));
        }
        if self.rework_triggered {
            out.push(format!("返工率 {:.1}% > 28.3%", self.rework_rate * 100.0));
        }
        if self.unmerged_triggered {
            out.push(format!("已交付未合入峰值 {} > 5", self.peak_unmerged));
        }
        if self.lane_triggered {
            out.push(format!(
                "lane 排队占比 {:.1}% > 15%",
                self.lane_queue_share * 100.0
            ));
        }
        out
    }

    pub fn stop_triggered(&self) -> bool {
        self.conflict_triggered
            || self.rework_triggered
            || self.unmerged_triggered
            || self.lane_triggered
    }

    pub fn to_json(&self) -> Value {
        let round4 = |x: f64| (x * 10000.0).round() / 10000.0;
        let round2 = |x: f64| (x * 100.0).round() / 100.0;
        json!({
            "since": self.since,
            "sinceSeconds": self.since_seconds,
            "repo": self.repo,
            "target": self.target,
            "maxInfraDocsShare": self.max_infra_docs_share,
            "fillDispatched": self.fill_dispatched,
            "writeDeliveries": self.write_deliveries,
            "premergeConflicts": self.premerge_conflicts,
            "premergeDenom": self.premerge_denom,
            "premergeConflictRate": round4(self.premerge_conflict_rate),
            "reworkSent": self.rework_sent,
            "reworkRate": round4(self.rework_rate),
            "peakUnmerged": self.peak_unmerged,
            "laneQueueShare": round4(self.lane_queue_share),
            "laneEvents": self.lane_events,
            "agyWeeklyPctConsumed": self.agy_weekly_pct_consumed.map(round2),
            "agyWeeklyPctPer10Deliveries": self.agy_weekly_pct_per_10.map(round2),
            "readonlyRuns": self.readonly_runs,
            "readonlyAgyTokens": self.readonly_agy_tokens,
            "totalAgyTokens": self.total_agy_tokens,
            "readonlyAgyShare": self.readonly_agy_share.map(round4),
            "stopLines": {
                "conflict": {
                    "value": round4(self.premerge_conflict_rate),
                    "threshold": STOP_CONFLICT_RATE,
                    "triggered": self.conflict_triggered,
                },
                "rework": {
                    "value": round4(self.rework_rate),
                    "threshold": STOP_REWORK_RATE,
                    "triggered": self.rework_triggered,
                },
                "peakUnmerged": {
                    "value": self.peak_unmerged,
                    "threshold": STOP_PEAK_UNMERGED,
                    "triggered": self.unmerged_triggered,
                },
                "laneQueue": {
                    "value": round4(self.lane_queue_share),
                    "threshold": STOP_LANE_QUEUE_SHARE,
                    "triggered": self.lane_triggered,
                },
            },
            "triggered": self.triggered_labels(),
            "stopTriggered": self.stop_triggered(),
        })
    }

    pub fn format_human(&self) -> String {
        let target_label = match self.target {
            Some(t) => format!("target={t}"),
            None => "target=未启用".to_string(),
        };
        let mut lines = Vec::new();
        lines.push(format!(
            "补位统计（近 {} · {} · 写入交付 {} 次 · 补位派发 {} 次）：",
            self.since, target_label, self.write_deliveries, self.fill_dispatched
        ));
        let conflict_tag = if self.conflict_triggered {
            " [越线]"
        } else {
            ""
        };
        let rework_tag = if self.rework_triggered {
            " [越线]"
        } else {
            ""
        };
        let unmerged_tag = if self.unmerged_triggered {
            " [越线]"
        } else {
            ""
        };
        let lane_tag = if self.lane_triggered {
            " [越线]"
        } else {
            ""
        };
        lines.push(format!(
            "  premerge 冲突率：{:.1}%（{}/{}，停止线 >10%）{}",
            self.premerge_conflict_rate * 100.0,
            self.premerge_conflicts,
            self.premerge_denom,
            conflict_tag
        ));
        lines.push(format!(
            "  返工率：{:.1}%（{}/{}，停止线 >28.3%）{}",
            self.rework_rate * 100.0,
            self.rework_sent,
            self.write_deliveries,
            rework_tag
        ));
        lines.push(format!(
            "  已交付未合入峰值：{}（停止线 >5）{}",
            self.peak_unmerged, unmerged_tag
        ));
        lines.push(format!(
            "  lane 排队占比：{:.1}%（{} 次 lane，停止线 >15%）{}",
            self.lane_queue_share * 100.0,
            self.lane_events,
            lane_tag
        ));
        let quota_line = match self.agy_weekly_pct_per_10 {
            Some(pct) => format!("  每 10 次交付 agy 周额度：{pct:.2}%"),
            None if self.write_deliveries == 0 => {
                "  每 10 次交付 agy 周额度：—（窗口内无写入交付）".to_string()
            }
            None => "  每 10 次交付 agy 周额度：—（缺少 agy 周额度快照）".to_string(),
        };
        lines.push(quota_line);
        let ro_share_str = match self.readonly_agy_share {
            Some(share) => format!("占 agy 用量 {:.1}%", share * 100.0),
            None => "占 agy 用量 —".to_string(),
        };
        lines.push(format!(
            "  只读 run：{} 次（{}）",
            self.readonly_runs, ro_share_str
        ));
        let trig = self.triggered_labels();
        if trig.is_empty() {
            lines.push(
                "停止线：未触发（满 20 次交付后若未越线，可由主控手动调高 .delegate.json 的 fill.target）"
                    .to_string(),
            );
        } else {
            lines.push(format!(
                "停止线：已触发（{}）— 仅提醒，不自动降档；请主控先补 hotspots，仍越线再手动调低 .delegate.json 的 fill.target",
                trig.join("；")
            ));
        }
        lines.join("\n")
    }
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

fn repo_matches(ev_repo: &str, filter_canon: Option<&str>, allow_empty: bool) -> bool {
    let Some(target) = filter_canon else {
        return true;
    };
    if ev_repo.trim().is_empty() {
        return allow_empty;
    }
    canonical_repo_key(ev_repo) == target
}

fn weekly_quota_drop_in_window(quota_doc: &Value, since_epoch: i64) -> (f64, bool) {
    let Some(accounts) = quota_doc["accounts"].as_object() else {
        return (0.0, false);
    };
    let mut total_drop = 0.0;
    let mut has_samples = false;
    for acct in accounts.values() {
        let mut pts: Vec<(i64, f64)> = Vec::new();
        if let Some(hist) = acct["history"].as_array() {
            for item in hist {
                let ep = item["epoch"].as_i64().unwrap_or(0);
                if ep <= 0 {
                    continue;
                }
                if let Some(w) = item["weekly"].as_f64().filter(|v| v.is_finite()) {
                    pts.push((ep, w));
                }
            }
        }
        if b(acct, "ok") {
            let fetched_ep = acct["fetchedEpoch"]
                .as_i64()
                .or_else(|| iso_epoch(s(acct, "fetchedAt")))
                .unwrap_or(0);
            if fetched_ep > 0 {
                let mut min_w: Option<f64> = None;
                if let Some(arr) = acct["entries"].as_array() {
                    for e in arr {
                        if s(e, "window") == "weekly" {
                            if let Some(pct) =
                                e["remainingPct"].as_f64().filter(|v| v.is_finite())
                            {
                                min_w = Some(min_w.map_or(pct, |prev| prev.min(pct)));
                            }
                        }
                    }
                }
                if let Some(w) = min_w {
                    if !pts.iter().any(|(ep, _)| *ep == fetched_ep) {
                        pts.push((fetched_ep, w));
                    }
                }
            }
        }
        pts.sort_by_key(|(ep, _)| *ep);
        pts.dedup_by_key(|(ep, _)| *ep);
        let in_win: Vec<(i64, f64)> = pts
            .into_iter()
            .filter(|(ep, _)| *ep >= since_epoch)
            .collect();
        if in_win.len() >= 2 {
            has_samples = true;
            for pair in in_win.windows(2) {
                let prev = pair[0].1;
                let cur = pair[1].1;
                if cur < prev {
                    total_drop += prev - cur;
                }
            }
        }
    }
    (total_drop, has_samples)
}

#[allow(clippy::too_many_arguments)]
pub fn compute_fill_stats(
    events: &[Value],
    usage_rows: &[UsageRow],
    quota_doc: &Value,
    repo_filter: Option<&str>,
    since_label: &str,
    since_secs: Option<i64>,
    since_epoch: i64,
    fill_cfg: Option<&FillConfig>,
) -> FillStats {
    let filter_canon = repo_filter.map(canonical_repo_key).filter(|s| !s.is_empty());
    let filter_ref = filter_canon.as_deref();

    let mut unmerged: HashSet<String> = HashSet::new();
    let mut peak_unmerged: usize = 0;
    let mut entered_window = false;

    let mut fill_dispatched: usize = 0;
    let mut write_deliveries: usize = 0;
    let mut conflict_runs: HashSet<String> = HashSet::new();
    let mut premerge_ready_count: usize = 0;
    let mut rework_sent: usize = 0;
    let mut lane_queued_secs: f64 = 0.0;
    let mut lane_held_secs: f64 = 0.0;
    let mut lane_events: usize = 0;
    let mut readonly_runs: HashSet<String> = HashSet::new();

    for (idx, ev) in events.iter().enumerate() {
        let kind = s(ev, "kind");
        let ev_repo = s(ev, "repo");
        let is_lane = matches!(kind, "lane_acquired" | "lane_released");
        if !repo_matches(ev_repo, filter_ref, is_lane) {
            continue;
        }
        let ev_ep = iso_epoch(s(ev, "at")).unwrap_or(0);
        let in_window = since_secs.is_none() || ev_ep >= since_epoch;
        let run = s(ev, "run");
        let p = &ev["payload"];

        if in_window && !entered_window {
            entered_window = true;
            peak_unmerged = peak_unmerged.max(unmerged.len());
        }

        // Maintain delivered-but-unmerged write run set across history so runs delivered
        // shortly before the window are still reflected accurately.
        match kind {
            "delivered" => {
                let mode = s(p, "mode");
                let state = s(p, "state");
                let is_ro = mode == "read-only" || state == "answered";
                if !is_ro {
                    let zero_files = p.get("files").and_then(Value::as_u64) == Some(0);
                    if !zero_files && !run.is_empty() {
                        unmerged.insert(run.to_string());
                    }
                }
            }
            "applied" | "cleaned" | "stopped" | "rework_sent" => {
                if !run.is_empty() {
                    unmerged.remove(run);
                }
            }
            "merge_item" if s(p, "status") == "MERGED" => {
                let src = s(p, "sourceRun");
                if !src.is_empty() {
                    unmerged.remove(src);
                }
            }
            _ => {}
        }

        if !in_window {
            continue;
        }

        peak_unmerged = peak_unmerged.max(unmerged.len());

        match kind {
            "work_dispatched" => {
                if s(p, "reason") == "fill" {
                    fill_dispatched += 1;
                }
            }
            "started" => {
                if s(p, "mode") == "read-only" {
                    let key = if run.is_empty() {
                        format!("#started-{idx}")
                    } else {
                        run.to_string()
                    };
                    readonly_runs.insert(key);
                }
            }
            "delivered" => {
                let mode = s(p, "mode");
                let state = s(p, "state");
                let is_ro = mode == "read-only" || state == "answered";
                if is_ro {
                    let key = if run.is_empty() {
                        format!("#ro-{idx}")
                    } else {
                        run.to_string()
                    };
                    readonly_runs.insert(key);
                } else {
                    write_deliveries += 1;
                }
            }
            "premerge_conflict" => {
                let key = if run.is_empty() {
                    format!("#conflict-{idx}")
                } else {
                    run.to_string()
                };
                conflict_runs.insert(key);
            }
            "premerge_ready" => {
                premerge_ready_count += 1;
            }
            "rework_sent" => {
                rework_sent += 1;
            }
            "lane_acquired" => {
                lane_queued_secs += p["queuedSeconds"].as_f64().unwrap_or(0.0).max(0.0);
                lane_events += 1;
            }
            "lane_released" => {
                lane_held_secs += p["heldSeconds"].as_f64().unwrap_or(0.0).max(0.0);
            }
            _ => {}
        }
    }

    if !entered_window && !unmerged.is_empty() {
        peak_unmerged = unmerged.len();
    }

    let mut all_repos_agy_tokens: i64 = 0;
    let mut repo_agy_tokens: i64 = 0;
    let mut readonly_agy_tokens: i64 = 0;
    for row in usage_rows {
        if !row.is_agy {
            continue;
        }
        if since_secs.is_some() && row.epoch < since_epoch {
            continue;
        }
        all_repos_agy_tokens = all_repos_agy_tokens.saturating_add(row.total_tokens.max(0));
        if !repo_matches(&row.repo, filter_ref, false) {
            continue;
        }
        repo_agy_tokens = repo_agy_tokens.saturating_add(row.total_tokens.max(0));
        let is_ro =
            row.mode == "read-only" || (!row.run.is_empty() && readonly_runs.contains(&row.run));
        if is_ro {
            readonly_agy_tokens = readonly_agy_tokens.saturating_add(row.total_tokens.max(0));
            if !row.run.is_empty() {
                readonly_runs.insert(row.run.clone());
            }
        }
    }

    let premerge_conflicts = conflict_runs.len();
    let premerge_denom = if write_deliveries > 0 {
        write_deliveries
    } else {
        premerge_conflicts + premerge_ready_count
    };
    let premerge_conflict_rate = if premerge_denom > 0 {
        premerge_conflicts as f64 / premerge_denom as f64
    } else {
        0.0
    };
    let rework_rate = if write_deliveries > 0 {
        rework_sent as f64 / write_deliveries as f64
    } else {
        0.0
    };
    let lane_total = lane_queued_secs + lane_held_secs;
    let lane_queue_share = if lane_total > 0.0 {
        lane_queued_secs / lane_total
    } else {
        0.0
    };

    let (raw_weekly_drop, has_quota_samples) = weekly_quota_drop_in_window(quota_doc, since_epoch);
    let agy_weekly_pct_consumed = if has_quota_samples {
        let attributed = if filter_ref.is_some() && all_repos_agy_tokens > 0 && repo_agy_tokens > 0
        {
            raw_weekly_drop * (repo_agy_tokens as f64 / all_repos_agy_tokens as f64)
        } else {
            raw_weekly_drop
        };
        Some(attributed)
    } else {
        None
    };
    let agy_weekly_pct_per_10 = agy_weekly_pct_consumed.and_then(|drop| {
        if write_deliveries > 0 {
            Some((drop / write_deliveries as f64) * 10.0)
        } else {
            None
        }
    });
    let readonly_agy_share = if repo_agy_tokens > 0 {
        Some(readonly_agy_tokens as f64 / repo_agy_tokens as f64)
    } else {
        None
    };

    let conflict_triggered = premerge_conflict_rate > STOP_CONFLICT_RATE;
    let rework_triggered = rework_rate > STOP_REWORK_RATE;
    let unmerged_triggered = peak_unmerged > STOP_PEAK_UNMERGED;
    let lane_triggered = lane_queue_share > STOP_LANE_QUEUE_SHARE;

    FillStats {
        since: since_label.to_string(),
        since_seconds: since_secs,
        repo: filter_canon,
        target: fill_cfg.map(|c| c.target),
        max_infra_docs_share: fill_cfg.map(|c| c.max_infra_docs_share),
        fill_dispatched,
        write_deliveries,
        premerge_conflicts,
        premerge_denom,
        premerge_conflict_rate,
        rework_sent,
        rework_rate,
        peak_unmerged,
        lane_queue_share,
        lane_events,
        agy_weekly_pct_consumed,
        agy_weekly_pct_per_10,
        readonly_runs: readonly_runs.len(),
        readonly_agy_tokens,
        total_agy_tokens: repo_agy_tokens,
        readonly_agy_share,
        conflict_triggered,
        rework_triggered,
        unmerged_triggered,
        lane_triggered,
    }
}

fn load_usage_rows(state: &Path) -> Vec<UsageRow> {
    let path = state.join("agy-usage.jsonl");
    if !path.is_file() {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for line in read(&path).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let is_agy = b(&v, "isAgy")
            || s(&v, "provider") == crate::agy::BIN
            || crate::agy::is_agy(s(&v, "model"));
        let ep = v["epoch"]
            .as_i64()
            .or_else(|| iso_epoch(s(&v, "at")))
            .unwrap_or(0);
        let tok = crate::usage::TokenCounts::from_json(&v["tokens"]);
        if tok.is_zero() {
            continue;
        }
        rows.push(UsageRow {
            epoch: ep,
            run: s(&v, "run").to_string(),
            repo: s(&v, "repo").to_string(),
            mode: s(&v, "mode").to_string(),
            is_agy,
            total_tokens: tok.total(),
        });
    }
    rows
}

fn parse_since_arg(raw: &str) -> Res<(String, Option<i64>)> {
    let t = raw.trim();
    if t.eq_ignore_ascii_case("all") || t == "0" {
        return Ok(("all".to_string(), None));
    }
    if let Some(days) = t.strip_suffix('d').and_then(|d| d.parse::<f64>().ok()) {
        if days > 0.0 && days.is_finite() {
            return Ok((t.to_string(), Some((days * 86400.0).round() as i64)));
        }
    }
    let secs = seconds(t)?;
    Ok((t.to_string(), Some(secs.round() as i64)))
}

pub fn command(args: &[String]) -> Res<i32> {
    let Some((sub, rest)) = args.split_first() else {
        return Err("usage: delegate fill stats [--since 24h] [--repo <PATH>] [--all] [--json]".into());
    };
    match sub.as_str() {
        "stats" => stats_cmd(rest),
        other => Err(format!(
            "unknown fill subcommand: '{other}' (expected: stats)"
        )),
    }
}

fn stats_cmd(args: &[String]) -> Res<i32> {
    let mut since_raw = "24h".to_string();
    let mut repo_arg: Option<String> = None;
    let mut all_repos = false;
    let mut json_mode = crate::output::json_enabled();

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
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
            "--since" => since_raw = take_val(key, &mut i)?,
            "--repo" => repo_arg = Some(take_val(key, &mut i)?),
            "--all" => all_repos = true,
            "--json" => json_mode = true,
            _ => {
                return Err(unrecognized_arg(
                    a,
                    &["--since", "--repo", "--all", "--json"],
                    "delegate fill stats [--since DURATION|all] [--repo PATH] [--all] [--json]",
                ));
            }
        }
        i += 1;
    }

    let (since_label, since_secs) = parse_since_arg(&since_raw)?;
    let now_ep = epoch() as i64;
    let since_ep = since_secs.map(|s| now_ep.saturating_sub(s)).unwrap_or(0);

    let repo_path: Option<PathBuf> = if all_repos {
        None
    } else if let Some(r) = repo_arg {
        let p = PathBuf::from(r);
        Some(fs::canonicalize(&p).unwrap_or(p))
    } else {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| git_top(&cwd))
            .map(|p| fs::canonicalize(&p).unwrap_or(p))
    };

    let fill_cfg = repo_path.as_deref().and_then(|top| {
        config::read_file(&top.join(".delegate.json"))
            .ok()
            .flatten()
            .and_then(|v| config::parse_fill_config(v.get("fill")))
    });

    let state = state_dir();
    let evs = crate::events::read_since_in(&state, 0);
    let usage_rows = load_usage_rows(&state);
    let quota_doc = json(state.join("agy-quota.json"));

    let repo_str = repo_path
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned());
    let stats = compute_fill_stats(
        &evs,
        &usage_rows,
        &quota_doc,
        repo_str.as_deref(),
        &since_label,
        since_secs,
        since_ep,
        fill_cfg.as_ref(),
    );

    if json_mode {
        println!("{}", stats.to_json());
    } else {
        println!("{}", stats.format_human());
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iso_at(ep: i64) -> String {
        // Build RFC3339 UTC timestamp from epoch via simple calendar math matching common::iso_epoch.
        let days = ep.div_euclid(86400);
        let rem = ep.rem_euclid(86400);
        let h = rem / 3600;
        let m = (rem % 3600) / 60;
        let s = rem % 60;
        // Howard Hinnant civil_from_days
        let z = days + 719468;
        let era = z.div_euclid(146097);
        let doe = z.rem_euclid(146097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let mo = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if mo <= 2 { y + 1 } else { y };
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
    }

    #[test]
    fn infra_docs_quota_matches_target_6_share_0_3() {
        assert_eq!(max_infra_docs_slots(6, 0.3), 2);
        assert_eq!(max_infra_docs_slots(8, 0.3), 2);
        assert_eq!(max_infra_docs_slots(10, 0.3), 3);
        assert_eq!(max_infra_docs_slots(6, 0.0), 0);
        assert_eq!(max_infra_docs_slots(6, 1.0), 6);
        assert_eq!(max_infra_docs_slots(0, 0.3), 0);
    }

    #[test]
    fn hotspot_path_prefix_matching() {
        assert!(path_prefix_hits_hotspot("README.md", "README.md"));
        assert!(path_prefix_hits_hotspot("./README.md", "README.md"));
        assert!(!path_prefix_hits_hotspot("docs/README.md", "README.md"));
        assert!(!path_prefix_hits_hotspot("README.md.bak", "README.md"));
        assert!(path_prefix_hits_hotspot("xtask/", "xtask/size-baseline.txt"));
        assert!(path_prefix_hits_hotspot("xtask", "xtask/size-baseline.txt"));
        assert!(path_prefix_hits_hotspot(
            "xtask/size-baseline.txt",
            "xtask/size-baseline.txt"
        ));
        assert!(!path_prefix_hits_hotspot(
            "xtask/src/main.rs",
            "xtask/size-baseline.txt"
        ));
        assert!(!path_prefix_hits_hotspot(
            "xtask2/size-baseline.txt",
            "xtask/size-baseline.txt"
        ));
    }

    #[test]
    fn compute_fill_stats_fixture_under_and_over_stop_lines() {
        let base_ep = 1_760_000_000i64;
        let repo = "/tmp/demo-repo";
        let fill_cfg = FillConfig {
            target: 6,
            max_infra_docs_share: 0.3,
        };

        // Fixture: 10 write deliveries, 2 premerge conflicts (20% > 10%), 3 rework_sent (30% > 28.3%),
        // 6 concurrent delivered-but-unmerged runs (peak 6 > 5), lane queued 40s / held 160s (20% > 15%),
        // 2 read-only runs taking 10% of agy tokens, weekly quota dropped from 95% to 91% (4% / 10 deliv = 4.0% per 10).
        let mut evs = Vec::new();
        let mut seq = 1u64;
        let mut push_ev = |offset: i64, run: &str, kind: &str, payload: Value| {
            evs.push(json!({
                "seq": seq,
                "at": iso_at(base_ep + offset),
                "run": run,
                "repo": repo,
                "kind": kind,
                "payload": payload,
            }));
            seq += 1;
        };

        // 6 runs delivered before any apply -> peak_unmerged = 6
        for i in 1..=6 {
            let rid = format!("w{i}");
            push_ev(i * 10, &rid, "work_dispatched", json!({"reason": "fill", "class": "feat", "activeWriteRuns": i - 1}));
            push_ev(i * 10 + 5, &rid, "delivered", json!({"mode": "write", "state": "delivered", "files": 2}));
        }
        for i in 1..=6 {
            push_ev(100 + i * 5, &format!("w{i}"), "applied", json!({}));
        }
        // 4 more write deliveries (total 10), applied promptly
        for i in 7..=10 {
            let rid = format!("w{i}");
            push_ev(200 + i * 10, &rid, "delivered", json!({"mode": "write", "state": "delivered", "files": 1}));
            push_ev(200 + i * 10 + 5, &rid, "applied", json!({}));
        }
        // 2 premerge conflicts -> 2/10 = 20% (> 10%)
        push_ev(150, "w2", "premerge_conflict", json!({"conflicts": ["README.md"]}));
        push_ev(160, "w3", "premerge_conflict", json!({"conflicts": ["README.md"]}));
        // 3 rework_sent -> 3/10 = 30% (> 28.3%)
        push_ev(170, "w1", "rework_sent", json!({"round": 1}));
        push_ev(175, "w4", "rework_sent", json!({"round": 1}));
        push_ev(180, "w5", "rework_sent", json!({"round": 1}));
        // Lane: queued 40s, held 160s -> 40 / 200 = 20% (> 15%)
        push_ev(190, "w1", "lane_acquired", json!({"queuedSeconds": 40.0}));
        push_ev(195, "w1", "lane_released", json!({"heldSeconds": 160.0}));
        // 2 read-only runs
        push_ev(210, "ro1", "delivered", json!({"mode": "read-only", "state": "answered"}));
        push_ev(220, "ro2", "started", json!({"mode": "read-only"}));

        let usage = vec![
            UsageRow {
                epoch: base_ep + 50,
                run: "w1".into(),
                repo: repo.into(),
                mode: "write".into(),
                is_agy: true,
                total_tokens: 900_000,
            },
            UsageRow {
                epoch: base_ep + 215,
                run: "ro1".into(),
                repo: repo.into(),
                mode: "read-only".into(),
                is_agy: true,
                total_tokens: 100_000,
            },
        ];
        let quota = json!({
            "accounts": {
                "a1": {
                    "history": [
                        {"epoch": base_ep + 10, "weekly": 95.0},
                        {"epoch": base_ep + 300, "weekly": 91.0}
                    ]
                }
            }
        });

        let stats = compute_fill_stats(
            &evs,
            &usage,
            &quota,
            Some(repo),
            "24h",
            Some(86400),
            base_ep,
            Some(&fill_cfg),
        );
        assert_eq!(stats.target, Some(6));
        assert_eq!(stats.fill_dispatched, 6);
        assert_eq!(stats.write_deliveries, 10);
        assert_eq!(stats.premerge_conflicts, 2);
        assert!((stats.premerge_conflict_rate - 0.20).abs() < 1e-6);
        assert_eq!(stats.rework_sent, 3);
        assert!((stats.rework_rate - 0.30).abs() < 1e-6);
        assert_eq!(stats.peak_unmerged, 6);
        assert!((stats.lane_queue_share - 0.20).abs() < 1e-6);
        assert!((stats.agy_weekly_pct_per_10.unwrap() - 4.0).abs() < 1e-6);
        assert_eq!(stats.readonly_runs, 2);
        assert!((stats.readonly_agy_share.unwrap() - 0.10).abs() < 1e-6);
        assert!(stats.conflict_triggered);
        assert!(stats.rework_triggered);
        assert!(stats.unmerged_triggered);
        assert!(stats.lane_triggered);
        assert!(stats.stop_triggered());
        let human = stats.format_human();
        assert!(human.contains("停止线：已触发"), "{human}");
        assert!(human.contains("不自动降档"), "{human}");
    }
}
