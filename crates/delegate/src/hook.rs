//! Stop hook telemetry (`<state>/stop-hook.jsonl`, `<state>/hook-actions.jsonl`) and
//! strictly read-only, zero-lock weekly statistics (`delegate hook stats`).

use crate::common::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

pub fn stop_log_path(state: &Path) -> PathBuf {
    state.join("stop-hook.jsonl")
}

pub fn actions_log_path(state: &Path) -> PathBuf {
    state.join("hook-actions.jsonl")
}

/// Append a lightweight, zero-lock action record (`wait` / `apply` / `defer`) when the
/// Stop hook log exists on this machine, so `delegate hook stats` can attribute follow-up
/// actions accurately without acquiring `.state.lock`.
pub fn record_action(action: &str, runs: &[String], work_id: Option<&str>, machine: bool) {
    let state = state_dir();
    if !stop_log_path(&state).is_file() {
        return;
    }
    record_action_in(&state, action, runs, work_id, machine);
}

pub fn record_action_timed(
    action: &str,
    runs: &[String],
    work_id: Option<&str>,
    machine: bool,
    started_at: &str,
    finished_at: &str,
    elapsed_ms: u64,
) {
    let state = state_dir();
    if !stop_log_path(&state).is_file() {
        return;
    }
    record_action_in_timed(
        &state,
        action,
        runs,
        work_id,
        machine,
        Some((started_at, finished_at, elapsed_ms)),
    );
}

pub fn record_action_in(
    state: &Path,
    action: &str,
    runs: &[String],
    work_id: Option<&str>,
    machine: bool,
) {
    record_action_in_timed(state, action, runs, work_id, machine, None);
}

fn record_action_in_timed(
    state: &Path,
    action: &str,
    runs: &[String],
    work_id: Option<&str>,
    machine: bool,
    timing: Option<(&str, &str, u64)>,
) {
    let mut entry = json!({
        "ts": iso(),
        "action": action,
        "runs": runs,
    });
    if let Some((started_at, finished_at, elapsed_ms)) = timing {
        if !started_at.is_empty() {
            entry["startedAt"] = json!(started_at);
        }
        if !finished_at.is_empty() {
            entry["finishedAt"] = json!(finished_at);
        }
        entry["elapsedMs"] = json!(elapsed_ms);
    }
    if let Some(wid) = work_id.filter(|w| !w.is_empty()) {
        entry["workId"] = json!(wid);
    }
    if machine {
        entry["machine"] = json!(true);
    }
    let _ = fs::create_dir_all(state);
    if let Ok(mut fh) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(actions_log_path(state))
    {
        let line = format!("{entry}\n");
        let _ = fh.write_all(line.as_bytes());
    }
}

/// Convert Unix epoch seconds to ISO-8601 week label (`YYYY-Www`).
pub fn iso_week_from_epoch(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    // 1970-01-01 was Thursday (ISO weekday 4).
    let dow = (days + 3).rem_euclid(7) + 1; // 1=Mon .. 7=Sun
    let thu_days = days + (4 - dow);
    let (iso_year, _, _) = civil_from_days(thu_days);
    let jan1_days = days_from_civil(iso_year, 1, 1);
    let week = (thu_days - jan1_days) / 7 + 1;
    format!("{iso_year:04}-W{week:02}")
}

/// Howard Hinnant's civil-from-days algorithm (days since 1970-01-01 -> (Y, M, D)).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u64;
    let m = m as u64;
    let d = d as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

#[derive(Clone, Debug)]
struct BlockRecord {
    ts: i64,
    week: String,
    hook: String,
    tokens: Vec<String>,
}

#[derive(Clone, Debug)]
struct ActionRecord {
    ts: i64,
    action: String, // "wait" | "apply" | "defer" | "close" | "wait_on"
    tokens: Vec<String>,
    machine: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Waited,
    Applied,
    Deferred,
    Closed,
    WaitedOn,
    Ignored,
}

#[derive(Clone, Debug, Default)]
struct WeekBucket {
    triggers: usize,
    by_hook: BTreeMap<String, usize>,
    waited: usize,
    applied: usize,
    deferred: usize,
    closed: usize,
    waited_on: usize,
    ignored: usize,
}

fn clean_run_token(raw: &str) -> String {
    let trimmed = raw.trim();
    // Strip cross-repo label prefix `【repo】name`
    let stripped = if let Some(rest) = trimmed.strip_prefix('【') {
        rest.split_once('】').map_or(trimmed, |(_, tail)| tail)
    } else {
        trimmed
    };
    stripped.trim().to_string()
}

fn extract_block_tokens(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut push_unique = |tok: String| {
        if !tok.is_empty() && !out.contains(&tok) {
            out.push(tok);
        }
    };
    if let Some(arr) = v["runIds"].as_array() {
        for item in arr {
            if let Some(st) = item.as_str() {
                push_unique(clean_run_token(st));
            }
        }
    }
    if let Some(arr) = v["workIds"].as_array() {
        for item in arr {
            if let Some(st) = item.as_str() {
                push_unique(clean_run_token(st));
            }
        }
    }
    if let Some(arr) = v["runs"].as_array() {
        for item in arr {
            if let Some(st) = item.as_str() {
                push_unique(clean_run_token(st));
            }
        }
    } else if let Some(raw) = v["runs"].as_str() {
        for part in raw.split(['、', ',']) {
            push_unique(clean_run_token(part));
        }
    }
    out
}

fn token_overlaps(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    if a.ends_with(&format!("-{b}")) || b.ends_with(&format!("-{a}")) {
        return true;
    }
    (b.len() >= 4 && a.contains(b)) || (a.len() >= 4 && b.contains(a))
}

fn tokens_overlap(left: &[String], right: &[String]) -> bool {
    left.iter()
        .any(|a| right.iter().any(|b| token_overlaps(a, b)))
}

fn load_blocks(state: &Path) -> Vec<BlockRecord> {
    let path = stop_log_path(state);
    let Ok(f) = OpenOptions::new().read(true).open(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(t) else {
            continue;
        };
        let ts_str = s(&v, "ts");
        let Some(ts) = iso_epoch(ts_str) else {
            continue;
        };
        let hook = {
            let h = s(&v, "hook").trim();
            if h.is_empty() {
                "unknown".to_string()
            } else {
                h.to_string()
            }
        };
        out.push(BlockRecord {
            ts,
            week: iso_week_from_epoch(ts),
            hook,
            tokens: extract_block_tokens(&v),
        });
    }
    out.sort_by_key(|b| b.ts);
    out
}

fn load_actions(state: &Path) -> Vec<ActionRecord> {
    let mut actions = Vec::new();

    // 1. Direct action log (`<state>/hook-actions.jsonl`)
    if let Ok(f) = OpenOptions::new().read(true).open(actions_log_path(state)) {
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(t) else {
                continue;
            };
            let Some(ts) = iso_epoch(s(&v, "ts")) else {
                continue;
            };
            let action = s(&v, "action").to_string();
            if !matches!(action.as_str(), "wait" | "apply" | "defer" | "close" | "wait_on") {
                continue;
            }
            let mut tokens: Vec<String> = strings(&v["runs"])
                .into_iter()
                .map(|r| clean_run_token(&r))
                .filter(|r| !r.is_empty())
                .collect();
            let wid = clean_run_token(s(&v, "workId"));
            if !wid.is_empty() && !tokens.contains(&wid) {
                tokens.push(wid);
            }
            actions.push(ActionRecord {
                ts,
                action,
                tokens,
                machine: b(&v, "machine"),
            });
        }
    }

    // 2. Events stream (`<state>/events.jsonl`) for `applied` / `work_deferred`
    if let Ok(f) = OpenOptions::new()
        .read(true)
        .open(state.join("events.jsonl"))
    {
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            if !t.contains("\"applied\"") && !t.contains("\"work_deferred\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(t) else {
                continue;
            };
            let kind = s(&v, "kind");
            let action = match kind {
                "applied" => "apply",
                "work_deferred" => "defer",
                _ => continue,
            };
            let Some(ts) = iso_epoch(s(&v, "at")) else {
                continue;
            };
            let mut tokens = Vec::new();
            let rid = s(&v, "run");
            if !rid.is_empty() {
                tokens.push(rid.to_string());
            }
            let name = s(&v["payload"], "name");
            if !name.is_empty() && !tokens.iter().any(|x| x == name) {
                tokens.push(name.to_string());
            }
            for r in strings(&v["payload"]["runs"]) {
                if !tokens.contains(&r) {
                    tokens.push(r);
                }
            }
            actions.push(ActionRecord {
                ts,
                action: action.to_string(),
                tokens,
                machine: false,
            });
        }
    }

    // 3. Work table (`<state>/work.jsonl`) for deferred / applied / closed / wait_on timestamps
    if let Ok(f) = OpenOptions::new()
        .read(true)
        .open(state.join("work.jsonl"))
    {
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(t) else {
                continue;
            };
            let mut tokens = strings(&v["runs"]);
            let wid = s(&v, "id");
            if !wid.is_empty() && !tokens.iter().any(|x| x == wid) {
                tokens.push(wid.to_string());
            }
            let title = s(&v, "title");
            if !title.is_empty() && !tokens.iter().any(|x| x == title) {
                tokens.push(title.to_string());
            }
            if tokens.is_empty() {
                continue;
            }
            let state_ts_str = if !s(&v, "stateSince").is_empty() {
                s(&v, "stateSince")
            } else {
                s(&v, "updatedAt")
            };
            let st = s(&v, "state");
            if st == "deferred" || !s(&v, "deferReason").is_empty() {
                if let Some(ts) = iso_epoch(state_ts_str) {
                    actions.push(ActionRecord {
                        ts,
                        action: "defer".to_string(),
                        tokens: tokens.clone(),
                        machine: false,
                    });
                }
            }
            if st == "done" && s(&v, "closedKind") != "dropped" {
                if let Some(ts) = iso_epoch(state_ts_str) {
                    actions.push(ActionRecord {
                        ts,
                        action: "close".to_string(),
                        tokens: tokens.clone(),
                        machine: false,
                    });
                }
            }
            if st == "blocked" && !strings(&v["waitOn"]).is_empty() {
                if let Some(ts) = iso_epoch(state_ts_str) {
                    actions.push(ActionRecord {
                        ts,
                        action: "wait_on".to_string(),
                        tokens: tokens.clone(),
                        machine: false,
                    });
                }
            }
            if b(&v, "applied") {
                if let Some(ts) = iso_epoch(s(&v, "updatedAt")) {
                    actions.push(ActionRecord {
                        ts,
                        action: "apply".to_string(),
                        tokens,
                        machine: false,
                    });
                }
            }
        }
    }

    // 4. Registered repo `history.jsonl` files for legacy `appliedAt`
    let mut hist_paths: Vec<PathBuf> = Vec::new();
    let mut seen_hist: HashSet<PathBuf> = HashSet::new();
    let mut add_hist = |p: PathBuf| {
        if p.is_file() {
            let k = fs::canonicalize(&p).unwrap_or(p);
            if seen_hist.insert(k.clone()) {
                hist_paths.push(k);
            }
        }
    };
    add_hist(runs_root().join("history.jsonl"));
    let repos_doc = json(state.join("repos.json"));
    if let Some(map) = repos_doc["repos"].as_object() {
        for entry in map.values() {
            let runs_s = s(entry, "runs");
            if !runs_s.is_empty() {
                add_hist(PathBuf::from(runs_s).join("history.jsonl"));
            }
        }
    }
    for hp in hist_paths {
        let Ok(f) = OpenOptions::new().read(true).open(&hp) else {
            continue;
        };
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            let t = line.trim();
            if t.is_empty() || !t.contains("\"appliedAt\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(t) else {
                continue;
            };
            let Some(ts) = iso_epoch(s(&v, "appliedAt")) else {
                continue;
            };
            let mut tokens = Vec::new();
            let rid = s(&v, "run");
            if !rid.is_empty() {
                tokens.push(rid.to_string());
            }
            let name = s(&v, "name");
            if !name.is_empty() && !tokens.iter().any(|x| x == name) {
                tokens.push(name.to_string());
            }
            if !tokens.is_empty() {
                actions.push(ActionRecord {
                    ts,
                    action: "apply".to_string(),
                    tokens,
                    machine: false,
                });
            }
        }
    }

    actions.sort_by_key(|a| a.ts);
    actions
}

fn attribute_outcome(
    idx: usize,
    blocks: &[BlockRecord],
    actions: &[ActionRecord],
) -> Outcome {
    const WINDOW_SECS: i64 = 3600;
    let blk = &blocks[idx];
    let mut end_ts = blk.ts.saturating_add(WINDOW_SECS);
    // Cap window at the next repeat block for the same hook & overlapping run.
    for next in &blocks[idx + 1..] {
        if next.ts > end_ts {
            break;
        }
        if next.ts > blk.ts
            && next.hook == blk.hook
            && (blk.tokens.is_empty()
                || next.tokens.is_empty()
                || tokens_overlap(&blk.tokens, &next.tokens))
        {
            end_ts = next.ts;
            break;
        }
    }

    let mut best: Option<(i64, Outcome)> = None;
    for act in actions {
        if act.ts < blk.ts {
            continue;
        }
        if act.ts > end_ts {
            break;
        }
        let matched = match act.action.as_str() {
            "wait" => {
                matches!(blk.hook.as_str(), "waiter" | "events")
                    && (act.machine
                        || act.tokens.is_empty()
                        || tokens_overlap(&blk.tokens, &act.tokens))
            }
            "apply" | "defer" | "close" | "wait_on" => tokens_overlap(&blk.tokens, &act.tokens),
            _ => false,
        };
        if !matched {
            continue;
        }
        let outcome = match act.action.as_str() {
            "wait" => Outcome::Waited,
            "apply" => Outcome::Applied,
            "defer" => Outcome::Deferred,
            "close" => Outcome::Closed,
            "wait_on" => Outcome::WaitedOn,
            _ => continue,
        };
        if best.is_none_or(|(bts, _)| act.ts < bts) {
            best = Some((act.ts, outcome));
        }
    }
    best.map_or(Outcome::Ignored, |(_, o)| o)
}

pub fn compute_stats(state: &Path, max_weeks: Option<usize>) -> Value {
    let blocks = load_blocks(state);
    let actions = load_actions(state);
    let mut buckets: BTreeMap<String, WeekBucket> = BTreeMap::new();

    for i in 0..blocks.len() {
        let outcome = attribute_outcome(i, &blocks, &actions);
        let blk = &blocks[i];
        let entry = buckets.entry(blk.week.clone()).or_default();
        entry.triggers += 1;
        *entry.by_hook.entry(blk.hook.clone()).or_insert(0) += 1;
        match outcome {
            Outcome::Waited => entry.waited += 1,
            Outcome::Applied => entry.applied += 1,
            Outcome::Deferred => entry.deferred += 1,
            Outcome::Closed => entry.closed += 1,
            Outcome::WaitedOn => entry.waited_on += 1,
            Outcome::Ignored => entry.ignored += 1,
        }
    }

    let mut week_list: Vec<(String, WeekBucket)> = buckets.into_iter().collect();
    if let Some(limit) = max_weeks {
        if limit > 0 && week_list.len() > limit {
            let drain_n = week_list.len() - limit;
            week_list.drain(0..drain_n);
        }
    }

    let mut total = 0usize;
    let mut total_waited = 0usize;
    let mut total_applied = 0usize;
    let mut total_deferred = 0usize;
    let mut total_closed = 0usize;
    let mut total_waited_on = 0usize;
    let mut total_ignored = 0usize;
    let mut total_by_hook: BTreeMap<String, usize> = BTreeMap::new();

    let weeks_json: Vec<Value> = week_list
        .iter()
        .map(|(w, b)| {
            total += b.triggers;
            total_waited += b.waited;
            total_applied += b.applied;
            total_deferred += b.deferred;
            total_closed += b.closed;
            total_waited_on += b.waited_on;
            total_ignored += b.ignored;
            for (k, &v) in &b.by_hook {
                *total_by_hook.entry(k.clone()).or_insert(0) += v;
            }
            json!({
                "week": w,
                "triggers": b.triggers,
                "byHook": b.by_hook,
                "outcomes": {
                    "waited": b.waited,
                    "applied": b.applied,
                    "deferred": b.deferred,
                    "closed": b.closed,
                    "waitedOn": b.waited_on,
                    "ignored": b.ignored,
                }
            })
        })
        .collect();

    json!({
        "logPath": stop_log_path(state).to_string_lossy(),
        "total": total,
        "byHook": total_by_hook,
        "outcomes": {
            "waited": total_waited,
            "applied": total_applied,
            "deferred": total_deferred,
            "closed": total_closed,
            "waitedOn": total_waited_on,
            "ignored": total_ignored,
        },
        "weeks": weeks_json,
    })
}

pub fn render_stats_human(doc: &Value) -> String {
    let total = doc["total"].as_u64().unwrap_or(0);
    let log_path = s(doc, "logPath");
    if total == 0 {
        return format!("Stop hook 尚无拦截记录（{log_path}）");
    }
    let mut lines = Vec::new();
    lines.push(format!("Stop hook 统计（{log_path}，共 {total} 次触发）"));
    lines.push(
        "周次       触发  waiter  unmerged  question  events  work | 挂了 wait  apply 了  defer 了  close 了  wait_on 了  被忽略了"
            .to_string(),
    );
    for w in doc["weeks"].as_array().into_iter().flatten() {
        let week = s(w, "week");
        let trig = w["triggers"].as_u64().unwrap_or(0);
        let bh = &w["byHook"];
        let oc = &w["outcomes"];
        lines.push(format!(
            "{week:<9} {:>5} {:>7} {:>9} {:>9} {:>7} {:>5} | {:>9} {:>9} {:>9} {:>9} {:>11} {:>8}",
            trig,
            bh["waiter"].as_u64().unwrap_or(0),
            bh["unmerged"].as_u64().unwrap_or(0),
            bh["question"].as_u64().unwrap_or(0),
            bh["events"].as_u64().unwrap_or(0),
            bh["work"].as_u64().unwrap_or(0),
            oc["waited"].as_u64().unwrap_or(0),
            oc["applied"].as_u64().unwrap_or(0),
            oc["deferred"].as_u64().unwrap_or(0),
            oc["closed"].as_u64().unwrap_or(0),
            oc["waitedOn"].as_u64().unwrap_or(0),
            oc["ignored"].as_u64().unwrap_or(0),
        ));
    }
    let bh = &doc["byHook"];
    let oc = &doc["outcomes"];
    lines.push(format!(
        "合计      {:>5} {:>7} {:>9} {:>9} {:>7} {:>5} | {:>9} {:>9} {:>9} {:>9} {:>11} {:>8}",
        total,
        bh["waiter"].as_u64().unwrap_or(0),
        bh["unmerged"].as_u64().unwrap_or(0),
        bh["question"].as_u64().unwrap_or(0),
        bh["events"].as_u64().unwrap_or(0),
        bh["work"].as_u64().unwrap_or(0),
        oc["waited"].as_u64().unwrap_or(0),
        oc["applied"].as_u64().unwrap_or(0),
        oc["deferred"].as_u64().unwrap_or(0),
        oc["closed"].as_u64().unwrap_or(0),
        oc["waitedOn"].as_u64().unwrap_or(0),
        oc["ignored"].as_u64().unwrap_or(0),
    ));
    lines.join("\n")
}

pub fn command(args: &[String]) -> Res<i32> {
    let Some((first, rest)) = args.split_first() else {
        return Err(
            "usage: delegate hook <check|pending|stats> [options]".into(),
        );
    };
    match first.as_str() {
        "check" => crate::hook_check::check_cmd(rest),
        "pending" => crate::hook_check::pending_cmd(rest),
        "stats" => stats_cmd(rest),
        other => Err(format!(
            "unknown hook subcommand '{other}' (expected: check, pending, stats)"
        )),
    }
}

fn stats_cmd(args: &[String]) -> Res<i32> {
    let mut json_mode = crate::output::json_enabled();
    let mut max_weeks: Option<usize> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (key, inline) = a
            .split_once('=')
            .map_or((a.as_str(), None), |(k, v)| (k, Some(v)));
        match key {
            "--json" => json_mode = true,
            "--weeks" => {
                let raw = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --weeks: expected an integer")?
                    }
                };
                let n = raw
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --weeks value '{raw}'"))?;
                if n == 0 {
                    return Err("--weeks must be >= 1".into());
                }
                max_weeks = Some(n);
            }
            _ => {
                return Err(unrecognized_arg(
                    a,
                    &["--weeks", "--json"],
                    "delegate hook stats [--weeks N] [--json]",
                ));
            }
        }
        i += 1;
    }

    let state = state_dir();
    let doc = compute_stats(&state, max_weeks);
    if json_mode {
        println!("{doc}");
    } else {
        println!("{}", render_stats_human(&doc));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "delegate-hook-test-{tag}-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn iso_week_calculation_handles_year_boundaries() {
        assert_eq!(iso_week_from_epoch(iso_epoch("2026-11-23T10:00:00Z").unwrap()), "2026-W48");
        assert_eq!(iso_week_from_epoch(iso_epoch("2026-01-01T00:00:00Z").unwrap()), "2026-W01");
        assert_eq!(iso_week_from_epoch(iso_epoch("2025-12-29T12:00:00Z").unwrap()), "2026-W01");
        assert_eq!(iso_week_from_epoch(iso_epoch("2020-12-31T12:00:00Z").unwrap()), "2020-W53");
    }

    #[test]
    fn hook_stats_groups_by_week_and_attributes_all_four_outcomes() {
        let state = temp_state("stats");
        // Week 47: 1 waiter -> waited, 1 unmerged -> ignored
        // Week 48: 1 unmerged -> applied, 1 unmerged -> deferred, 1 repeat ignored then applied
        fs::write(
            stop_log_path(&state),
            concat!(
                "{\"ts\":\"2026-11-18T10:00:00Z\",\"hook\":\"waiter\",\"runs\":\"job-w47\"}\n",
                "{\"ts\":\"2026-11-18T11:00:00Z\",\"hook\":\"unmerged\",\"runs\":\"orphan-w47\"}\n",
                "{\"ts\":\"2026-11-23T09:00:00Z\",\"hook\":\"unmerged\",\"runs\":\"feat-a\",\"runIds\":[\"20261123-085000-feat-a\"]}\n",
                "{\"ts\":\"2026-11-23T09:30:00Z\",\"hook\":\"unmerged\",\"runs\":\"feat-b\",\"runIds\":[\"20261123-091000-feat-b\"]}\n",
                "{\"ts\":\"2026-11-23T10:00:00Z\",\"hook\":\"unmerged\",\"runs\":\"feat-c\",\"runIds\":[\"20261123-095000-feat-c\"]}\n",
                "{\"ts\":\"2026-11-23T10:05:00Z\",\"hook\":\"unmerged\",\"runs\":\"feat-c\",\"runIds\":[\"20261123-095000-feat-c\"]}\n",
            ),
        )
        .unwrap();

        fs::write(
            actions_log_path(&state),
            concat!(
                "{\"ts\":\"2026-11-18T10:00:15Z\",\"action\":\"wait\",\"runs\":[]}\n",
                "{\"ts\":\"2026-11-23T09:30:20Z\",\"action\":\"defer\",\"runs\":[\"20261123-091000-feat-b\"]}\n",
            ),
        )
        .unwrap();

        fs::write(
            state.join("events.jsonl"),
            concat!(
                "{\"seq\":1,\"at\":\"2026-11-23T09:01:00Z\",\"kind\":\"applied\",\"run\":\"20261123-085000-feat-a\",\"payload\":{\"name\":\"feat-a\"}}\n",
                "{\"seq\":2,\"at\":\"2026-11-23T10:06:00Z\",\"kind\":\"applied\",\"run\":\"20261123-095000-feat-c\",\"payload\":{\"name\":\"feat-c\"}}\n",
            ),
        )
        .unwrap();

        let doc = compute_stats(&state, None);
        assert_eq!(doc["total"], 6);
        assert_eq!(doc["outcomes"]["waited"], 1);
        assert_eq!(doc["outcomes"]["applied"], 2);
        assert_eq!(doc["outcomes"]["deferred"], 1);
        assert_eq!(doc["outcomes"]["ignored"], 2);

        let weeks = doc["weeks"].as_array().unwrap();
        assert_eq!(weeks.len(), 2);
        assert_eq!(weeks[0]["week"], "2026-W47");
        assert_eq!(weeks[0]["triggers"], 2);
        assert_eq!(weeks[0]["outcomes"]["waited"], 1);
        assert_eq!(weeks[0]["outcomes"]["ignored"], 1);

        assert_eq!(weeks[1]["week"], "2026-W48");
        assert_eq!(weeks[1]["triggers"], 4);
        assert_eq!(weeks[1]["outcomes"]["applied"], 2);
        assert_eq!(weeks[1]["outcomes"]["deferred"], 1);
        assert_eq!(weeks[1]["outcomes"]["ignored"], 1);

        let rendered = render_stats_human(&doc);
        assert!(rendered.contains("2026-W47"), "{rendered}");
        assert!(rendered.contains("2026-W48"), "{rendered}");
        assert!(rendered.contains("挂了 wait"), "{rendered}");
        assert!(rendered.contains("apply 了"), "{rendered}");
        assert!(rendered.contains("defer 了"), "{rendered}");
        assert!(rendered.contains("close 了"), "{rendered}");
        assert!(rendered.contains("wait_on 了"), "{rendered}");
        assert!(rendered.contains("被忽略了"), "{rendered}");

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn hook_stats_attributes_work_hook_close_wait_on_and_defer() {
        let state = temp_state("work-stats");
        fs::write(
            stop_log_path(&state),
            concat!(
                "{\"ts\":\"2026-11-24T09:00:00Z\",\"hook\":\"work\",\"runs\":\"W001\",\"workIds\":[\"W001\"]}\n",
                "{\"ts\":\"2026-11-24T09:10:00Z\",\"hook\":\"work\",\"runs\":\"W002\",\"workIds\":[\"W002\"]}\n",
                "{\"ts\":\"2026-11-24T09:20:00Z\",\"hook\":\"work\",\"runs\":\"W003\",\"workIds\":[\"W003\"]}\n",
            ),
        )
        .unwrap();
        fs::write(
            actions_log_path(&state),
            concat!(
                "{\"ts\":\"2026-11-24T09:01:00Z\",\"action\":\"close\",\"runs\":[],\"workId\":\"W001\"}\n",
                "{\"ts\":\"2026-11-24T09:11:00Z\",\"action\":\"wait_on\",\"runs\":[],\"workId\":\"W002\"}\n",
                "{\"ts\":\"2026-11-24T09:21:00Z\",\"action\":\"defer\",\"runs\":[],\"workId\":\"W003\"}\n",
            ),
        )
        .unwrap();

        let doc = compute_stats(&state, None);
        assert_eq!(doc["total"], 3);
        assert_eq!(doc["byHook"]["work"], 3);
        assert_eq!(doc["outcomes"]["closed"], 1);
        assert_eq!(doc["outcomes"]["waitedOn"], 1);
        assert_eq!(doc["outcomes"]["deferred"], 1);
        assert_eq!(doc["outcomes"]["ignored"], 0);

        let rendered = render_stats_human(&doc);
        assert!(rendered.contains("work"), "{rendered}");
        assert!(rendered.contains("close 了"), "{rendered}");
        assert!(rendered.contains("wait_on 了"), "{rendered}");

        let _ = fs::remove_dir_all(&state);
    }
}
