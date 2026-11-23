//! Per-round and per-account Antigravity CLI (`agy`) token usage tracking, live updates,
//! aggregation (`delegate usage`), and local-only quota snapshot estimation.
//!
//! Design invariants:
//! - Records only the active `agy` account slot ID (`agyAccount`), never email addresses or tokens.
//! - Never calls external Google quota APIs and never triggers automatic account switching.
//! - Avoids double-counting across `agy` rework/resume/reply/multi-round reviewer continuations by
//!   tracking per-session cumulative high-water marks and recording per-round deltas.

use crate::common::*;
use crate::runs;
use crate::{has, parse_simple, value};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const FIVE_HOURS_SECS: i64 = 5 * 3600;
pub const SEVEN_DAYS_SECS: i64 = 7 * 86400;
pub const UNASSIGNED_ACCOUNT: &str = "(unassigned)";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCounts {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
}

impl TokenCounts {
    pub fn new(input: i64, output: i64, cache_read: i64) -> Self {
        Self {
            input: input.max(0),
            output: output.max(0),
            cache_read: cache_read.max(0),
        }
    }

    pub fn from_tuple((input, output, cache_read): (i64, i64, i64)) -> Self {
        Self::new(input, output, cache_read)
    }

    pub fn from_json(v: &Value) -> Self {
        if !v.is_object() {
            return Self::default();
        }
        Self::new(n(v, "input"), n(v, "output"), n(v, "cacheRead"))
    }

    pub fn total(&self) -> i64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
    }

    pub fn is_zero(&self) -> bool {
        self.input == 0 && self.output == 0 && self.cache_read == 0
    }

    pub fn add(&mut self, other: Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
    }

    pub fn sub_clamped(self, base: Self) -> Self {
        Self {
            input: (self.input - base.input).max(0),
            output: (self.output - base.output).max(0),
            cache_read: (self.cache_read - base.cache_read).max(0),
        }
    }

    pub fn max_each(self, other: Self) -> Self {
        Self {
            input: self.input.max(other.input),
            output: self.output.max(other.output),
            cache_read: self.cache_read.max(other.cache_read),
        }
    }

    pub fn to_json(self) -> Value {
        json!({
            "input": self.input,
            "output": self.output,
            "cacheRead": self.cache_read,
        })
    }

    pub fn to_json_with_total(self) -> Value {
        json!({
            "input": self.input,
            "output": self.output,
            "cacheRead": self.cache_read,
            "total": self.total(),
        })
    }
}

pub fn ledger_path() -> PathBuf {
    state_dir().join("agy-usage.jsonl")
}

fn repo_path_from_meta(meta: &Value, run_dir: &Path) -> String {
    let src = s(&meta["worktree"], "source");
    let raw = if !src.is_empty() {
        src
    } else if !s(meta, "top").is_empty() {
        s(meta, "top")
    } else if !s(meta, "workdir").is_empty() {
        s(meta, "workdir")
    } else {
        ""
    };
    if raw.is_empty() {
        return run_dir
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    let p = Path::new(raw);
    fs::canonicalize(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn repo_display_name(repo: &str) -> String {
    let trimmed = repo.trim_end_matches('/');
    if trimmed.is_empty() {
        return "(unknown)".to_string();
    }
    Path::new(trimmed)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

fn parse_round_from_dir(dir: &Path) -> u64 {
    dir.file_name()
        .and_then(|n| n.to_str())
        .and_then(|name| name.strip_prefix("round-"))
        .and_then(|num| num.parse::<u64>().ok())
        .unwrap_or(1)
}

fn is_agy_meta(meta: &Value) -> bool {
    crate::agy::is_agy(s(meta, "model")) || s(meta, "agent") == crate::agy::BIN
}

/// Walk the multi-hop `meta.parent` chain (including archived ancestors under
/// `<cache>/delegate/archive/<id>/record`) to find the nearest ancestor run sharing `session_id`
/// and return its last cumulative token usage.
///
/// Single canonical baseline used by both `supervise::compute_tokens` (`summary.json["tokensDelta"]`)
/// and `LiveTracker` (`usage-rounds.jsonl` / `agy-usage.jsonl`).
pub fn ancestor_session_usage(run: &Path, meta: &Value, session_id: &str) -> Option<TokenCounts> {
    let mut parent_id = s(meta, "parent").to_string();
    if parent_id.is_empty() || session_id.is_empty() {
        return None;
    }
    let mut visited = BTreeSet::new();
    while !parent_id.is_empty() && visited.insert(parent_id.clone()) {
        let parent_dir = run
            .parent()
            .map(|p| p.join(&parent_id))
            .filter(|p| {
                p.join("summary.json").is_file()
                    || p.join("events.jsonl").is_file()
                    || p.join("meta.json").is_file()
            })
            .or_else(|| {
                let arch = cache_dir().join("archive").join(&parent_id).join("record");
                (arch.join("summary.json").is_file()
                    || arch.join("events.jsonl").is_file()
                    || arch.join("meta.json").is_file())
                .then_some(arch)
            });
        let pdir = parent_dir?;
        let psum = json(pdir.join("summary.json"));
        let psession = {
            let ps = s(&psum, "session");
            if !ps.is_empty() {
                ps.to_string()
            } else {
                runs::events(&pdir)
                    .iter()
                    .rev()
                    .find(|e| s(e, "e") == "session" && !s(e, "id").is_empty())
                    .map(|e| s(e, "id").to_string())
                    .unwrap_or_default()
            }
        };
        if psession == session_id {
            let parent_usage = runs::events(&pdir)
                .into_iter()
                .rev()
                .find(|e| s(e, "e") == "turn")
                .map(|e| e["usage"].clone())
                .filter(Value::is_object)
                .or_else(|| psum["tokens"].is_object().then(|| psum["tokens"].clone()));
            if let Some(pu) = parent_usage {
                return Some(TokenCounts::from_json(&pu));
            }
        }
        let pmeta = json(pdir.join("meta.json"));
        parent_id = s(&pmeta, "parent").to_string();
    }
    None
}

/// Read the cumulative high-water mark already recorded for `session` in `parent_run_dir`
/// (prior rework/reviewer rounds in this run), or for author reply runs continuing an ancestor's
/// same session, via `ancestor_session_usage`.
pub fn session_baseline_for_run(
    parent_run_dir: &Path,
    meta: &Value,
    session_id: &str,
    is_reviewer: bool,
) -> TokenCounts {
    if session_id.is_empty() {
        return TokenCounts::default();
    }
    // 1. Check already-recorded rounds in this run's `usage-rounds.jsonl`.
    let mut best = TokenCounts::default();
    let mut found_local = false;
    let rounds_file = parent_run_dir.join("usage-rounds.jsonl");
    if rounds_file.is_file() {
        for line in read(&rounds_file).lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if s(&v, "session") == session_id {
                let cum = if v["cumulativeTokens"].is_object() {
                    TokenCounts::from_json(&v["cumulativeTokens"])
                } else {
                    TokenCounts::from_json(&v["tokens"])
                };
                best = best.max_each(cum);
                found_local = true;
            }
        }
    }
    if found_local {
        return best;
    }

    // 2. For author reply runs continuing an ancestor's same agy session, use the shared ancestor lookup.
    if !is_reviewer {
        if let Some(anc) = ancestor_session_usage(parent_run_dir, meta, session_id) {
            return anc;
        }
    }

    TokenCounts::default()
}

/// Live usage tracker attached to a single `agents::run_agent_cancellable` execution
/// (author initial round, author rework round, retry/resume, or reviewer round).
pub struct LiveTracker {
    run_dir: PathBuf,
    parent_run_dir: PathBuf,
    run_id: String,
    repo: String,
    repo_name: String,
    role: &'static str,
    round: u64,
    rework: bool,
    attempt: i64,
    is_agy: bool,
    provider: String,
    model: String,
    account: Option<String>,
    session: String,
    started_at: String,
    meta: Value,
    baseline_cache: Option<(String, TokenCounts)>,
    last_observed_cum: Option<TokenCounts>,
    last_written_delta: Option<TokenCounts>,
    has_live_usage: bool,
    finished: bool,
}

impl Drop for LiveTracker {
    fn drop(&mut self) {
        if !self.finished {
            self.write_live(false, self.has_live_usage, self.last_written_delta, self.last_observed_cum);
        }
    }
}

impl LiveTracker {
    pub fn start(meta: &Value, run_dir: &Path, attempt: i64) -> Self {
        let is_reviewer = b(meta, "isReviewer");
        let role = if is_reviewer { "reviewer" } else { "author" };
        let parent_run_dir = if is_reviewer {
            run_dir
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| run_dir.to_path_buf())
        } else {
            run_dir.to_path_buf()
        };
        let round = if is_reviewer {
            parse_round_from_dir(run_dir)
        } else {
            meta["authorRound"].as_u64().unwrap_or(1)
        };
        let rework = !is_reviewer && (b(meta, "isRework") || round > 1);
        let is_agy = is_agy_meta(meta);
        let account = if is_agy {
            crate::agy_accounts::active_account_id()
        } else {
            None
        };
        let run_id = {
            let r = s(meta, "run");
            if !r.is_empty() {
                r.to_string()
            } else {
                parent_run_dir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }
        };
        let repo = repo_path_from_meta(meta, &parent_run_dir);
        let repo_name = repo_display_name(&repo);
        let model = s(meta, "model").to_string();
        let provider = if is_agy {
            crate::agy::BIN.to_string()
        } else {
            let p = s(meta, "provider");
            if !p.is_empty() {
                p.to_string()
            } else {
                model.split('/').next().unwrap_or("pi").to_string()
            }
        };
        let started_at = iso();
        let tracker = Self {
            run_dir: run_dir.to_path_buf(),
            parent_run_dir,
            run_id,
            repo,
            repo_name,
            role,
            round,
            rework,
            attempt,
            is_agy,
            provider,
            model,
            account,
            session: s(meta, "fork").to_string(),
            started_at,
            meta: meta.clone(),
            baseline_cache: None,
            last_observed_cum: None,
            last_written_delta: None,
            has_live_usage: false,
            finished: false,
        };
        tracker.write_live(true, false, None, None);
        tracker
    }

    pub fn observe_session(&mut self, session_id: &str) {
        if !session_id.is_empty() && self.session != session_id {
            self.session = session_id.to_string();
            self.baseline_cache = None;
        }
    }

    fn baseline(&mut self) -> TokenCounts {
        if !self.is_agy || self.session.is_empty() {
            return TokenCounts::default();
        }
        if let Some((ref sid, tok)) = self.baseline_cache {
            if sid == &self.session {
                return tok;
            }
        }
        let base = session_baseline_for_run(
            &self.parent_run_dir,
            &self.meta,
            &self.session,
            self.role == "reviewer",
        );
        self.baseline_cache = Some((self.session.clone(), base));
        base
    }

    /// Update live usage when an in-flight `agy` stream event carries cumulative usage.
    pub fn observe_live_usage(&mut self, raw: TokenCounts) {
        if raw.is_zero() {
            return;
        }
        self.last_observed_cum = Some(match self.last_observed_cum {
            Some(prev) => prev.max_each(raw),
            None => raw,
        });
        let base = self.baseline();
        let delta = if self.is_agy {
            raw.sub_clamped(base)
        } else {
            raw
        };
        if delta.is_zero() {
            return;
        }
        if !self.has_live_usage || self.last_written_delta != Some(delta) {
            self.has_live_usage = true;
            self.last_written_delta = Some(delta);
            self.write_live(true, true, Some(delta), Some(raw));
        }
    }

    fn write_live(
        &self,
        running: bool,
        has_usage: bool,
        delta: Option<TokenCounts>,
        cum: Option<TokenCounts>,
    ) {
        let mut doc = json!({
            "run": self.run_id,
            "repo": self.repo,
            "repoName": self.repo_name,
            "role": self.role,
            "round": self.round,
            "rework": self.rework,
            "attempt": self.attempt,
            "provider": self.provider,
            "model": self.model,
            "isAgy": self.is_agy,
            "agyAccount": self.account,
            "session": self.session,
            "running": running,
            "hasUsage": has_usage,
            "startedAt": self.started_at,
            "updatedAt": iso(),
        });
        if let Some(tok) = delta {
            doc["tokens"] = tok.to_json();
        }
        if let Some(c) = cum {
            doc["cumulativeTokens"] = c.to_json();
        }
        let _ = write_json(self.run_dir.join("live-usage.json"), &doc);
    }

    /// Finalize this attempt/round: write `live-usage.json` (`running: false`),
    /// write `review/round-N/usage.json` for reviewer rounds, append to `<run>/usage-rounds.jsonl`,
    /// and append to `<state>/agy-usage.jsonl`.
    pub fn finish(mut self, attempt_turns: &[Value], status: &str) {
        self.finished = true;
        let (delta, cumulative) = if self.is_agy {
            let raw = attempt_turns
                .last()
                .map(|t| TokenCounts::from_json(&t["usage"]))
                .filter(|t| !t.is_zero())
                .or(self.last_observed_cum);
            match raw {
                Some(cum) => {
                    let base = self.baseline();
                    (cum.sub_clamped(base), Some(cum))
                }
                None => (TokenCounts::default(), None),
            }
        } else {
            let mut sum = TokenCounts::default();
            for t in attempt_turns {
                sum.add(TokenCounts::from_json(&t["usage"]));
            }
            (sum, None)
        };

        let finished_at = iso();
        let now_ep = epoch() as i64;
        self.write_live(false, !delta.is_zero(), (!delta.is_zero()).then_some(delta), cumulative);

        // Reviewer round file: <run>/review/round-<N>/usage.json
        if self.role == "reviewer" {
            let mut rdoc = json!({
                "round": self.round,
                "role": "reviewer",
                "run": self.run_id,
                "model": self.model,
                "provider": self.provider,
                "isAgy": self.is_agy,
                "session": self.session,
                "tokens": delta.to_json(),
                "turns": attempt_turns.len(),
                "status": status,
                "startedAt": self.started_at,
                "finishedAt": finished_at,
            });
            if let Some(ref acct) = self.account {
                rdoc["agyAccount"] = json!(acct);
            }
            if let Some(cum) = cumulative {
                rdoc["cumulativeTokens"] = cum.to_json();
            }
            let _ = write_json(self.run_dir.join("usage.json"), &rdoc);
        }

        if delta.is_zero() && attempt_turns.is_empty() {
            return;
        }

        let mut rec = json!({
            "v": 1,
            "at": finished_at,
            "epoch": now_ep,
            "run": self.run_id,
            "repo": self.repo,
            "repoName": self.repo_name,
            "role": self.role,
            "round": self.round,
            "rework": self.rework,
            "attempt": self.attempt,
            "provider": self.provider,
            "model": self.model,
            "isAgy": self.is_agy,
            "account": self.account,
            "session": self.session,
            "tokens": delta.to_json(),
            "status": status,
        });
        if let Some(cum) = cumulative {
            rec["cumulativeTokens"] = cum.to_json();
        }

        // Per-run round log
        let _ = append(
            self.parent_run_dir.join("usage-rounds.jsonl"),
            &format!("{rec}\n"),
        );

        // Machine-wide append-only usage ledger
        if !delta.is_zero() {
            let path = ledger_path();
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = append(&path, &format!("{rec}\n"));
        }
    }
}

/// Attach `tokens` (and `agyAccount` when present) from `<round_dir>/usage.json` onto a
/// `review.json` `rounds[]` entry.
pub fn attach_reviewer_round_usage(round_dir: &Path, round_entry: &mut Value) {
    let u = json(round_dir.join("usage.json"));
    if !u.is_object() {
        return;
    }
    let tok = TokenCounts::from_json(&u["tokens"]);
    if !tok.is_zero() || u["tokens"].is_object() {
        round_entry["tokens"] = tok.to_json();
    }
    let acct = s(&u, "agyAccount");
    if !acct.is_empty() {
        round_entry["agyAccount"] = json!(acct);
    }
}

/// Aggregate reviewer round tokens across `rounds_history` onto `review.json` (`final_doc`).
pub fn attach_review_totals(rounds_history: &[Value], doc: &mut Value) {
    let mut total = TokenCounts::default();
    let mut last_acct = String::new();
    let mut any = false;
    for r in rounds_history {
        if r["tokens"].is_object() {
            total.add(TokenCounts::from_json(&r["tokens"]));
            any = true;
        }
        let a = s(r, "agyAccount");
        if !a.is_empty() {
            last_acct = a.to_string();
        }
    }
    if any {
        doc["tokens"] = total.to_json();
    }
    if !last_acct.is_empty() {
        doc["agyAccount"] = json!(last_acct);
    }
}

/// Scan `<run>/review/round-*/usage.json` sorted by round number.
fn read_reviewer_round_files(run: &Path) -> Vec<Value> {
    let review_dir = run.join("review");
    let Ok(entries) = fs::read_dir(&review_dir) else {
        return Vec::new();
    };
    let mut items: Vec<(u64, Value)> = Vec::new();
    for ent in entries.flatten() {
        let p = ent.path();
        if !p.is_dir() {
            continue;
        }
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(num_str) = name.strip_prefix("round-") else {
            continue;
        };
        let Ok(round_num) = num_str.parse::<u64>() else {
            continue;
        };
        let ufile = p.join("usage.json");
        if ufile.is_file() {
            let v = json(&ufile);
            if v.is_object() {
                items.push((round_num, v));
            }
        }
    }
    items.sort_by_key(|(n, _)| *n);
    items.into_iter().map(|(_, v)| v).collect()
}

/// Summarize author + reviewer per-round usage into `summary.json` right before `finish_run`.
/// Preserves existing `sum["tokens"]` / `sum["tokensDelta"]` untouched for backward compatibility.
pub fn summarize_run_into_summary(run: &Path, meta: &Value, sum: &mut Value) {
    let mut author_total = TokenCounts::default();
    let mut reviewer_total = TokenCounts::default();
    let mut rounds_compact: Vec<Value> = Vec::new();
    let mut last_agy_account: Option<String> = None;
    let mut seen_reviewer_rounds: HashSet<u64> = HashSet::new();
    let mut had_author_rounds = false;

    let rounds_file = run.join("usage-rounds.jsonl");
    if rounds_file.is_file() {
        for line in read(&rounds_file).lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let role = s(&v, "role");
            let round = v["round"].as_u64().unwrap_or(1);
            let tok = TokenCounts::from_json(&v["tokens"]);
            let acct = v["account"]
                .as_str()
                .filter(|x| !x.is_empty())
                .map(str::to_string);
            if acct.is_some() {
                last_agy_account = acct.clone();
            }
            if role == "reviewer" {
                reviewer_total.add(tok);
                seen_reviewer_rounds.insert(round);
            } else {
                author_total.add(tok);
                had_author_rounds = true;
            }
            let mut entry = json!({
                "role": if role.is_empty() { "author" } else { role },
                "round": round,
                "tokens": tok.to_json(),
                "finishedAt": s(&v, "at"),
            });
            if b(&v, "rework") {
                entry["rework"] = json!(true);
            }
            if let Some(a) = acct {
                entry["agyAccount"] = json!(a);
            }
            rounds_compact.push(entry);
        }
    }

    // Also include any `review/round-N/usage.json` not already represented in `usage-rounds.jsonl`.
    for rv in read_reviewer_round_files(run) {
        let round = rv["round"].as_u64().unwrap_or(1);
        if seen_reviewer_rounds.contains(&round) {
            continue;
        }
        let tok = TokenCounts::from_json(&rv["tokens"]);
        let acct = rv["agyAccount"]
            .as_str()
            .filter(|x| !x.is_empty())
            .map(str::to_string);
        if acct.is_some() && last_agy_account.is_none() {
            last_agy_account = acct.clone();
        }
        reviewer_total.add(tok);
        seen_reviewer_rounds.insert(round);
        let mut entry = json!({
            "role": "reviewer",
            "round": round,
            "tokens": tok.to_json(),
            "finishedAt": s(&rv, "finishedAt"),
        });
        if let Some(a) = acct {
            entry["agyAccount"] = json!(a);
        }
        rounds_compact.push(entry);
    }

    // Canonical author incremental tokens: `runs::effective_tokens_delta(sum)` is the single
    // source of truth (`tokensDelta` for same-session agy replies, else `tokens`), falling back
    // to summed `usage-rounds.jsonl` only if `summary.json` has no author tokens block yet.
    if let Some(canon) = runs::effective_tokens_delta(sum) {
        author_total = TokenCounts::from_json(&canon);
    } else if !had_author_rounds {
        author_total = TokenCounts::default();
    }

    // Determine active agy account slot ID (never email).
    if last_agy_account.is_none() && is_agy_meta(meta) {
        let live = json(run.join("live-usage.json"));
        last_agy_account = live["agyAccount"]
            .as_str()
            .filter(|x| !x.is_empty())
            .map(str::to_string)
            .or_else(crate::agy_accounts::active_account_id);
    }

    if let Some(ref acct) = last_agy_account {
        sum["agyAccount"] = json!(acct);
    }

    let has_reviewer = !seen_reviewer_rounds.is_empty() || !reviewer_total.is_zero();
    if has_reviewer {
        sum["reviewTokens"] = reviewer_total.to_json();
        if let Some(rev_obj) = sum["review"].as_object_mut() {
            rev_obj.insert("tokens".into(), reviewer_total.to_json());
        }
    }

    let mut grand = author_total;
    grand.add(reviewer_total);
    if !grand.is_zero() || has_reviewer || last_agy_account.is_some() || !rounds_compact.is_empty()
    {
        let mut usage_obj = json!({
            "author": author_total.to_json(),
            "reviewer": reviewer_total.to_json(),
            "total": grand.to_json(),
            "rounds": rounds_compact,
        });
        if let Some(ref acct) = last_agy_account {
            usage_obj["agyAccount"] = json!(acct);
        }
        sum["usage"] = usage_obj;
    }
}

// ---------------------------------------------------------------------------
// Query, Aggregation, Running Discovery, and Quota Estimation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct UsageEntry {
    pub epoch: i64,
    pub run: String,
    pub repo: String,
    pub repo_name: String,
    pub role: String, // "author" | "reviewer"
    pub account: Option<String>,
    pub tokens: TokenCounts,
    pub live: bool,
}

#[derive(Debug, Clone)]
pub struct RunningUsageItem {
    pub run: String,
    pub repo: String,
    pub repo_name: String,
    pub role: String,
    pub round: u64,
    pub account: Option<String>,
    pub model: String,
    pub started_at: String,
    pub updated_at: String,
    pub tokens: Option<TokenCounts>,
}

fn parse_ledger_entries(since_epoch: Option<i64>, include_non_agy: bool) -> (Vec<UsageEntry>, HashSet<String>) {
    let mut entries = Vec::new();
    let mut ledger_runs = HashSet::new();
    let path = ledger_path();
    if !path.is_file() {
        return (entries, ledger_runs);
    }
    for line in read(&path).lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let run = s(&v, "run").to_string();
        if !run.is_empty() {
            ledger_runs.insert(run.clone());
        }
        let is_agy = b(&v, "isAgy")
            || s(&v, "provider") == crate::agy::BIN
            || crate::agy::is_agy(s(&v, "model"));
        if !include_non_agy && !is_agy {
            continue;
        }
        let ep = v["epoch"]
            .as_i64()
            .or_else(|| iso_epoch(s(&v, "at")))
            .unwrap_or(0);
        if let Some(cutoff) = since_epoch {
            if ep < cutoff {
                continue;
            }
        }
        let tokens = TokenCounts::from_json(&v["tokens"]);
        if tokens.is_zero() {
            continue;
        }
        let repo = s(&v, "repo").to_string();
        let repo_name = {
            let rn = s(&v, "repoName");
            if !rn.is_empty() {
                rn.to_string()
            } else {
                repo_display_name(&repo)
            }
        };
        let role = match s(&v, "role") {
            "reviewer" => "reviewer".to_string(),
            _ => "author".to_string(),
        };
        let account = v["account"]
            .as_str()
            .filter(|a| !a.is_empty())
            .map(str::to_string);
        entries.push(UsageEntry {
            epoch: ep,
            run,
            repo,
            repo_name,
            role,
            account,
            tokens,
            live: false,
        });
    }
    (entries, ledger_runs)
}

/// Extract finished usage records from a single run directory (or archive `record/` directory)
/// for runs that predate `agy-usage.jsonl` or were written outside the ledger.
fn extract_disk_run_entries(
    run_dir: &Path,
    since_epoch: Option<i64>,
    include_non_agy: bool,
) -> Vec<UsageEntry> {
    let sum = json(run_dir.join("summary.json"));
    if !sum.is_object() {
        return Vec::new();
    }
    let meta = json(run_dir.join("meta.json"));
    let run_id = {
        let r = s(&meta, "run");
        if !r.is_empty() {
            r.to_string()
        } else {
            run_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }
    };
    let repo = repo_path_from_meta(&meta, run_dir);
    let repo_name = repo_display_name(&repo);
    let is_agy = is_agy_meta(&meta);

    // Canonical disk readout: author tokens via `runs::effective_tokens_delta(&sum)`,
    // reviewer tokens via `review/round-*/usage.json` (falling back to `sum["reviewTokens"]`).
    let finished_at = {
        let f = s(&sum, "finishedAt");
        if !f.is_empty() {
            f.to_string()
        } else {
            s(&meta, "startedAt").to_string()
        }
    };
    let ep = iso_epoch(&finished_at)
        .or_else(|| meta["startedEpoch"].as_i64())
        .unwrap_or(0);
    if let Some(cutoff) = since_epoch {
        if ep < cutoff {
            return Vec::new();
        }
    }

    let mut out = Vec::new();
    let acct = sum["agyAccount"]
        .as_str()
        .or_else(|| sum["usage"]["agyAccount"].as_str())
        .filter(|a| !a.is_empty())
        .map(str::to_string);

    if include_non_agy || is_agy {
        let author_tok = runs::effective_tokens_delta(&sum)
            .map(|v| TokenCounts::from_json(&v))
            .unwrap_or_default();
        if !author_tok.is_zero() {
            out.push(UsageEntry {
                epoch: ep,
                run: run_id.clone(),
                repo: repo.clone(),
                repo_name: repo_name.clone(),
                role: "author".into(),
                account: acct.clone(),
                tokens: author_tok,
                live: false,
            });
        }
    }

    let rev_rounds = read_reviewer_round_files(run_dir);
    if !rev_rounds.is_empty() {
        for rv in rev_rounds {
            let rv_agy = b(&rv, "isAgy")
                || s(&rv, "provider") == crate::agy::BIN
                || crate::agy::is_agy(s(&rv, "model"));
            if !include_non_agy && !rv_agy {
                continue;
            }
            let rtok = TokenCounts::from_json(&rv["tokens"]);
            if rtok.is_zero() {
                continue;
            }
            let rf = s(&rv, "finishedAt");
            let rep = iso_epoch(rf).unwrap_or(ep);
            out.push(UsageEntry {
                epoch: rep,
                run: run_id.clone(),
                repo: repo.clone(),
                repo_name: repo_name.clone(),
                role: "reviewer".into(),
                account: rv["agyAccount"]
                    .as_str()
                    .filter(|a| !a.is_empty())
                    .map(str::to_string)
                    .or_else(|| acct.clone()),
                tokens: rtok,
                live: false,
            });
        }
    } else if sum["reviewTokens"].is_object() {
        let rev_model = s(&sum["review"], "model");
        let rev_agy = if !rev_model.is_empty() {
            crate::agy::is_agy(rev_model)
        } else {
            is_agy
        };
        if include_non_agy || rev_agy {
            let rtok = TokenCounts::from_json(&sum["reviewTokens"]);
            if !rtok.is_zero() {
                out.push(UsageEntry {
                    epoch: ep,
                    run: run_id,
                    repo,
                    repo_name,
                    role: "reviewer".into(),
                    account: sum["review"]["agyAccount"]
                        .as_str()
                        .filter(|a| !a.is_empty())
                        .map(str::to_string)
                        .or(acct),
                    tokens: rtok,
                    live: false,
                });
            }
        }
    }

    out
}

/// Inspect active run directories for live (in-flight) usage or uncounted running runs.
fn inspect_running_dir(
    run_dir: &Path,
    include_non_agy: bool,
    live_out: &mut Vec<RunningUsageItem>,
    uncounted_out: &mut Vec<RunningUsageItem>,
) {
    if runs::state(run_dir) != "running" {
        return;
    }
    let meta = json(run_dir.join("meta.json"));
    if !meta.is_object() {
        return;
    }
    let run_id = {
        let r = s(&meta, "run");
        if !r.is_empty() {
            r.to_string()
        } else {
            run_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }
    };
    let repo = repo_path_from_meta(&meta, run_dir);
    let repo_name = repo_display_name(&repo);

    // Check reviewer round live-usage.json files first, and author live-usage.json.
    let mut active_live_files: Vec<Value> = Vec::new();
    let review_dir = run_dir.join("review");
    if let Ok(entries) = fs::read_dir(&review_dir) {
        for ent in entries.flatten() {
            let p = ent.path();
            if p.is_dir() {
                let lf = p.join("live-usage.json");
                if lf.is_file() {
                    let v = json(&lf);
                    if v.is_object() && b(&v, "running") {
                        active_live_files.push(v);
                    }
                }
            }
        }
    }
    let author_lf = run_dir.join("live-usage.json");
    if author_lf.is_file() {
        let v = json(&author_lf);
        if v.is_object() && b(&v, "running") {
            active_live_files.push(v);
        }
    }

    if !active_live_files.is_empty() {
        for lv in active_live_files {
            let lv_agy = b(&lv, "isAgy")
                || s(&lv, "provider") == crate::agy::BIN
                || crate::agy::is_agy(s(&lv, "model"));
            if !include_non_agy && !lv_agy {
                continue;
            }
            let tok = if b(&lv, "hasUsage") && lv["tokens"].is_object() {
                let t = TokenCounts::from_json(&lv["tokens"]);
                (!t.is_zero()).then_some(t)
            } else {
                None
            };
            let item = RunningUsageItem {
                run: run_id.clone(),
                repo: repo.clone(),
                repo_name: repo_name.clone(),
                role: if s(&lv, "role") == "reviewer" {
                    "reviewer".into()
                } else {
                    "author".into()
                },
                round: lv["round"].as_u64().unwrap_or(1),
                account: lv["agyAccount"]
                    .as_str()
                    .filter(|a| !a.is_empty())
                    .map(str::to_string),
                model: s(&lv, "model").to_string(),
                started_at: s(&lv, "startedAt").to_string(),
                updated_at: s(&lv, "updatedAt").to_string(),
                tokens: tok,
            };
            if item.tokens.is_some() {
                live_out.push(item);
            } else {
                uncounted_out.push(item);
            }
        }
        return;
    }

    // Running run without `live-usage.json` yet (e.g. just started or older binary).
    let is_agy = is_agy_meta(&meta);
    if !include_non_agy && !is_agy {
        return;
    }
    let acct = if is_agy {
        crate::agy_accounts::active_account_id()
    } else {
        None
    };
    uncounted_out.push(RunningUsageItem {
        run: run_id,
        repo,
        repo_name,
        role: "author".into(),
        round: 1,
        account: acct,
        model: s(&meta, "model").to_string(),
        started_at: s(&meta, "startedAt").to_string(),
        updated_at: String::new(),
        tokens: None,
    });
}

fn discover_active_run_dirs(state: &Path, include_repo_scan: bool) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut push_dir = |p: PathBuf| {
        if !p.join("meta.json").is_file() || p.join("exit_code").is_file() {
            return;
        }
        let canon = fs::canonicalize(&p).unwrap_or(p);
        if seen.insert(canon.clone()) {
            dirs.push(canon);
        }
    };

    // 1. Live slot files in <state>/*.slot (O(active_runs), <0.5ms)
    if let Ok(rd) = fs::read_dir(state) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("slot") {
                continue;
            }
            let raw = read(&p);
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                push_dir(PathBuf::from(trimmed));
            }
        }
    }

    // 2. Optionally check discovered repos' active runs (runs without `exit_code`)
    if include_repo_scan {
        let repos = crate::index::discover_snapshot_repos(state, &[], None);
        for (_, runs_dir) in repos {
            if let Ok(rd) = fs::read_dir(&runs_dir) {
                for ent in rd.flatten() {
                    let p = ent.path();
                    if p.is_dir() && p.join("meta.json").is_file() && !p.join("exit_code").is_file() {
                        push_dir(p);
                    }
                }
            }
        }
    }

    dirs
}

fn collect_all(
    since_epoch: Option<i64>,
    include_non_agy: bool,
    scan_disk_backfill: bool,
) -> (Vec<UsageEntry>, Vec<RunningUsageItem>, Vec<RunningUsageItem>) {
    let state = state_dir();
    let (mut entries, ledger_runs) = parse_ledger_entries(since_epoch, include_non_agy);
    let mut seen_disk_runs = ledger_runs;

    if scan_disk_backfill {
        // Backfill unarchived runs across discovered repos that are not in the ledger.
        let repos = crate::index::discover_snapshot_repos(&state, &[], None);
        for (_, runs_dir) in repos {
            for rdir in runs::all_runs_in(&runs_dir) {
                if !rdir.join("summary.json").is_file() {
                    continue;
                }
                let run_id = rdir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                if seen_disk_runs.contains(&run_id) {
                    continue;
                }
                seen_disk_runs.insert(run_id);
                entries.extend(extract_disk_run_entries(&rdir, since_epoch, include_non_agy));
            }
        }
        // Backfill archived runs under `<cache>/delegate/archive/*/record/`.
        let arch_root = cache_dir().join("archive");
        if let Ok(rd) = fs::read_dir(&arch_root) {
            for ent in rd.flatten() {
                let p = ent.path();
                if !p.is_dir() {
                    continue;
                }
                let run_id = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                if seen_disk_runs.contains(&run_id) {
                    continue;
                }
                let rec = p.join("record");
                if rec.join("summary.json").is_file() {
                    seen_disk_runs.insert(run_id);
                    entries.extend(extract_disk_run_entries(&rec, since_epoch, include_non_agy));
                }
            }
        }
    }

    let mut live_running = Vec::new();
    let mut uncounted_running = Vec::new();
    for rdir in discover_active_run_dirs(&state, scan_disk_backfill) {
        inspect_running_dir(&rdir, include_non_agy, &mut live_running, &mut uncounted_running);
    }

    (entries, live_running, uncounted_running)
}

#[derive(Debug, Default, Clone)]
struct BucketStats {
    author: TokenCounts,
    reviewer: TokenCounts,
    author_rounds: usize,
    reviewer_rounds: usize,
    author_runs: BTreeSet<String>,
    reviewer_runs: BTreeSet<String>,
    all_runs: BTreeSet<String>,
    live_counted: usize,
    latest_epoch: i64,
    account: Option<String>,
    repo: Option<String>,
    repo_name: Option<String>,
}

impl BucketStats {
    fn add_entry(&mut self, e: &UsageEntry) {
        if e.role == "reviewer" {
            self.reviewer.add(e.tokens);
            self.reviewer_rounds += 1;
            self.reviewer_runs.insert(e.run.clone());
        } else {
            self.author.add(e.tokens);
            self.author_rounds += 1;
            self.author_runs.insert(e.run.clone());
        }
        self.all_runs.insert(e.run.clone());
        if e.live {
            self.live_counted += 1;
        }
        if e.epoch > self.latest_epoch {
            self.latest_epoch = e.epoch;
        }
        if self.account.is_none() && e.account.is_some() {
            self.account = e.account.clone();
        }
        if self.repo.is_none() && !e.repo.is_empty() {
            self.repo = Some(e.repo.clone());
            self.repo_name = Some(e.repo_name.clone());
        }
    }

    fn total(&self) -> TokenCounts {
        let mut t = self.author;
        t.add(self.reviewer);
        t
    }

    fn to_json(&self) -> Value {
        let total = self.total();
        json!({
            "author": {
                "input": self.author.input,
                "output": self.author.output,
                "cacheRead": self.author.cache_read,
                "total": self.author.total(),
                "rounds": self.author_rounds,
                "runs": self.author_runs.len(),
            },
            "reviewer": {
                "input": self.reviewer.input,
                "output": self.reviewer.output,
                "cacheRead": self.reviewer.cache_read,
                "total": self.reviewer.total(),
                "rounds": self.reviewer_rounds,
                "runs": self.reviewer_runs.len(),
            },
            "total": {
                "input": total.input,
                "output": total.output,
                "cacheRead": total.cache_read,
                "total": total.total(),
                "rounds": self.author_rounds + self.reviewer_rounds,
                "runs": self.all_runs.len(),
            },
            "liveCounted": self.live_counted,
        })
    }
}

pub fn format_token_compact(n: i64) -> String {
    if n <= 0 {
        return "0".to_string();
    }
    let f = n as f64;
    if f >= 1_000_000_000.0 {
        format!("{:.2}B", f / 1_000_000_000.0)
    } else if f >= 1_000_000.0 {
        format!("{:.1}M", f / 1_000_000.0)
    } else if f >= 1_000.0 {
        format!("{:.1}K", f / 1_000.0)
    } else {
        format!("{n}")
    }
}

fn format_tok_cell(t: TokenCounts) -> String {
    if t.is_zero() {
        "-".to_string()
    } else {
        format!(
            "{} ({}/{}/{})",
            format_token_compact(t.total()),
            format_token_compact(t.input),
            format_token_compact(t.output),
            format_token_compact(t.cache_read),
        )
    }
}

pub fn format_hours_compact(hours: f64) -> String {
    if !hours.is_finite() || hours < 0.0 {
        return "-".to_string();
    }
    let total_mins = (hours * 60.0).round() as u64;
    if total_mins < 60 {
        format!("{total_mins}m")
    } else if total_mins < 48 * 60 {
        let h = total_mins / 60;
        let m = total_mins % 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m:02}m")
        }
    } else {
        let d = total_mins / (24 * 60);
        let h = (total_mins % (24 * 60)) / 60;
        if h == 0 {
            format!("{d}d")
        } else {
            format!("{d}d{h}h")
        }
    }
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

/// CLI entrypoint: `delegate usage [--since 24h|7d|all] [--by account|day|run|repo] [--account ID] [--repo PATH] [--all] [--json]`
pub fn command(args: &[String]) -> Res<i32> {
    let (pos, flags, kv) = parse_simple(
        args,
        &["--json", "--all"],
        &["--since", "--by", "--account", "--repo"],
    )?;
    if !pos.is_empty() {
        return Err(format!("unrecognized arguments: {}", pos.join(" ")));
    }
    let json_out = has(&flags, "--json");
    let include_non_agy = has(&flags, "--all");
    let since_str = value(&kv, "--since").unwrap_or("24h");
    let by = value(&kv, "--by").unwrap_or("account");
    if !matches!(by, "account" | "day" | "run" | "repo") {
        return Err(format!(
            "--by must be one of account|day|run|repo, got {by:?}"
        ));
    }
    let account_filter = value(&kv, "--account");
    let repo_filter = value(&kv, "--repo");

    let now_ep = epoch() as i64;
    let (since_epoch, since_iso) = if since_str.eq_ignore_ascii_case("all") {
        (None, String::new())
    } else {
        let dur = seconds(since_str)? as i64;
        let cutoff = now_ep - dur;
        (Some(cutoff), format_epoch(cutoff, false, "%Y-%m-%dT%H:%M:%SZ"))
    };

    let (mut entries, mut live_running, mut uncounted_running) =
        collect_all(since_epoch, include_non_agy, true);

    // Apply optional --account / --repo filters.
    if let Some(af) = account_filter {
        entries.retain(|e| e.account.as_deref().unwrap_or(UNASSIGNED_ACCOUNT) == af);
        live_running.retain(|r| r.account.as_deref().unwrap_or(UNASSIGNED_ACCOUNT) == af);
        uncounted_running.retain(|r| r.account.as_deref().unwrap_or(UNASSIGNED_ACCOUNT) == af);
    }
    if let Some(rf) = repo_filter {
        entries.retain(|e| repo_matches_filter(&e.repo, &e.repo_name, rf));
        live_running.retain(|r| repo_matches_filter(&r.repo, &r.repo_name, rf));
        uncounted_running.retain(|r| repo_matches_filter(&r.repo, &r.repo_name, rf));
    }

    // Fold live-counted running entries into `entries` so totals and groups include them.
    for lr in &live_running {
        if let Some(tok) = lr.tokens {
            entries.push(UsageEntry {
                epoch: now_ep,
                run: lr.run.clone(),
                repo: lr.repo.clone(),
                repo_name: lr.repo_name.clone(),
                role: lr.role.clone(),
                account: lr.account.clone(),
                tokens: tok,
                live: true,
            });
        }
    }

    let mut grand = BucketStats::default();
    let mut buckets: BTreeMap<String, BucketStats> = BTreeMap::new();
    for e in &entries {
        grand.add_entry(e);
        let key = match by {
            "account" => e
                .account
                .clone()
                .unwrap_or_else(|| UNASSIGNED_ACCOUNT.to_string()),
            "day" => format_epoch(e.epoch, true, "%Y-%m-%d"),
            "run" => e.run.clone(),
            "repo" => e.repo_name.clone(),
            _ => unreachable!(),
        };
        buckets.entry(key).or_default().add_entry(e);
    }

    let mut rows: Vec<(String, BucketStats)> = buckets.into_iter().collect();
    match by {
        "day" => rows.sort_by(|a, b| a.0.cmp(&b.0)),
        "run" => rows.sort_by(|a, b| {
            b.1.latest_epoch
                .cmp(&a.1.latest_epoch)
                .then_with(|| b.0.cmp(&a.0))
        }),
        _ => rows.sort_by(|a, b| {
            b.1.total()
                .total()
                .cmp(&a.1.total().total())
                .then_with(|| a.0.cmp(&b.0))
        }),
    }

    uncounted_running.sort_by(|a, b| a.run.cmp(&b.run).then_with(|| a.role.cmp(&b.role)));
    live_running.sort_by(|a, b| a.run.cmp(&b.run).then_with(|| a.role.cmp(&b.role)));

    if json_out {
        let groups_json: Vec<Value> = rows
            .iter()
            .map(|(k, st)| {
                let mut obj = st.to_json();
                obj["key"] = json!(k);
                if by == "run" {
                    if let Some(ref a) = st.account {
                        obj["account"] = json!(a);
                    }
                    if let Some(ref r) = st.repo_name {
                        obj["repo"] = json!(r);
                    }
                } else if by == "repo" {
                    if let Some(ref p) = st.repo {
                        obj["repoPath"] = json!(p);
                    }
                }
                obj
            })
            .collect();
        let live_json: Vec<Value> = live_running
            .iter()
            .map(|r| {
                json!({
                    "run": r.run,
                    "role": r.role,
                    "round": r.round,
                    "account": r.account,
                    "repo": r.repo_name,
                    "model": r.model,
                    "tokens": r.tokens.unwrap_or_default().to_json_with_total(),
                    "startedAt": r.started_at,
                    "updatedAt": r.updated_at,
                })
            })
            .collect();
        let uncounted_json: Vec<Value> = uncounted_running
            .iter()
            .map(|r| {
                json!({
                    "run": r.run,
                    "role": r.role,
                    "round": r.round,
                    "account": r.account,
                    "repo": r.repo_name,
                    "model": r.model,
                    "startedAt": r.started_at,
                    "reason": "运行中未计",
                })
            })
            .collect();
        println!(
            "{}",
            json!({
                "since": since_str,
                "sinceIso": since_iso,
                "by": by,
                "provider": if include_non_agy { "all" } else { "agy" },
                "generatedAt": iso(),
                "totals": grand.to_json(),
                "groups": groups_json,
                "liveRunning": live_json,
                "uncountedRunning": uncounted_json,
            })
        );
        return Ok(0);
    }

    let scope = if include_non_agy { "全部模型" } else { "agy" };
    println!(
        "{scope} 用量统计（范围: {since_str}，分组: {by}，仅本地记录）:"
    );
    if rows.is_empty() {
        println!("  （所选时间段内无已记录用量）");
    } else {
        let key_header = match by {
            "account" => "账号槽位",
            "day" => "日期",
            "run" => "Run",
            "repo" => "仓库",
            _ => "分组",
        };
        let key_w = rows
            .iter()
            .map(|(k, _)| k.len())
            .max()
            .unwrap_or(10)
            .clamp(12, 34);
        println!(
            "  {:<key_w$}  {:<28}  {:<28}  {:<10}  轮次",
            key_header, "作者 total(in/out/cache)", "审查 total(in/out/cache)", "合计"
        );
        for (key, st) in &rows {
            let live_tag = if st.live_counted > 0 { "*" } else { "" };
            let rounds_str = format!(
                "{}+{}r",
                st.author_rounds, st.reviewer_rounds
            );
            println!(
                "  {:<key_w$}  {:<28}  {:<28}  {:<10}  {}{}",
                clip(key, key_w),
                format_tok_cell(st.author),
                format_tok_cell(st.reviewer),
                format_token_compact(st.total().total()),
                rounds_str,
                live_tag,
            );
        }
        println!(
            "  {:<key_w$}  {:<28}  {:<28}  {:<10}  {}+{}r",
            "合计",
            format_tok_cell(grand.author),
            format_tok_cell(grand.reviewer),
            format_token_compact(grand.total().total()),
            grand.author_rounds,
            grand.reviewer_rounds,
        );
        if grand.live_counted > 0 {
            println!("  * 含 {} 个运行中实时计入（来自 agy 输出累计用量）", grand.live_counted);
        }
    }

    if !uncounted_running.is_empty() {
        println!("\n运行中未计 ({}):", uncounted_running.len());
        for r in &uncounted_running {
            let acct = r.account.as_deref().unwrap_or(UNASSIGNED_ACCOUNT);
            let started = if r.started_at.is_empty() {
                "-"
            } else {
                r.started_at.as_str()
            };
            println!(
                "  - {}  role={} round={} account={} repo={} model={} started={}",
                r.run, r.role, r.round, acct, r.repo_name, r.model, started
            );
        }
    }

    println!("Next: delegate usage --since 7d --by account | delegate usage --by run");
    Ok(0)
}

// ---------------------------------------------------------------------------
// Fast Per-Account 5h / 7d Usage + Manual Quota Snapshot Estimation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct QuotaNotePoint {
    pub epoch: i64,
    pub weekly: Option<f64>,
    pub five_hour: Option<f64>,
}

struct ParsedQuotaNotes {
    points: Vec<QuotaNotePoint>,
    latest_weekly: Option<(f64, String, i64)>,
    latest_five_hour: Option<(f64, String, i64)>,
}

fn parse_quota_notes(account_entry: &Value) -> ParsedQuotaNotes {
    let mut pts = Vec::new();
    let mut latest_weekly: Option<(f64, String, i64)> = None;
    let mut latest_five_hour: Option<(f64, String, i64)> = None;

    if let Some(arr) = account_entry["quotaNotes"].as_array() {
        for item in arr {
            let at = s(item, "at").to_string();
            let ep = item["epoch"]
                .as_i64()
                .or_else(|| iso_epoch(&at))
                .unwrap_or(0);
            let weekly = item["weekly"].as_f64().filter(|v| v.is_finite());
            let five_hour = item["fiveHour"].as_f64().filter(|v| v.is_finite());
            if weekly.is_some() || five_hour.is_some() {
                pts.push(QuotaNotePoint {
                    epoch: ep,
                    weekly,
                    five_hour,
                });
            }
            if let Some(w) = weekly {
                if latest_weekly.as_ref().is_none_or(|(_, _, prev_ep)| ep >= *prev_ep) {
                    latest_weekly = Some((w, at.clone(), ep));
                }
            }
            if let Some(fh) = five_hour {
                if latest_five_hour.as_ref().is_none_or(|(_, _, prev_ep)| ep >= *prev_ep) {
                    latest_five_hour = Some((fh, at.clone(), ep));
                }
            }
        }
    }
    pts.sort_by_key(|p| p.epoch);
    ParsedQuotaNotes {
        points: pts,
        latest_weekly,
        latest_five_hour,
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Compute local-only quota estimate for one window (`weekly` or `fiveHour`) from
/// recorded manual snapshots and local usage records.
/// Never divides by zero or produces NaN/Inf; purely advisory (never triggers switching).
pub fn estimate_quota_window(
    notes: &[QuotaNotePoint],
    extract: impl Fn(&QuotaNotePoint) -> Option<f64>,
    window_secs: i64,
    now_ep: i64,
    acct_entries: &[&UsageEntry],
    window_total_tokens: i64,
    burn_rate_per_hour: f64,
) -> Option<Value> {
    let mut samples: Vec<(i64, f64)> = notes
        .iter()
        .filter_map(|p| extract(p).map(|v| (p.epoch, v)))
        .collect();
    samples.sort_by_key(|(ep, _)| *ep);
    let &(latest_ep, latest_pct) = samples.last()?;

    // 1. Prefer delta calibration between the latest snapshot and an earlier snapshot in the same window.
    let mut tokens_per_pct: Option<(f64, &'static str)> = None;
    for &(prev_ep, prev_pct) in samples.iter().rev().skip(1) {
        if latest_ep - prev_ep > window_secs || latest_ep <= prev_ep {
            continue;
        }
        let drop_pct = prev_pct - latest_pct;
        if drop_pct < 0.05 {
            continue;
        }
        let delta_tokens: i64 = acct_entries
            .iter()
            .filter(|e| e.epoch > prev_ep && e.epoch <= latest_ep)
            .map(|e| e.tokens.total())
            .sum();
        if delta_tokens > 0 {
            tokens_per_pct = Some((delta_tokens as f64 / drop_pct, "delta"));
            break;
        }
    }

    // 2. Fallback to window calibration (`(100 - latest_pct)` vs local usage recorded in this window).
    if tokens_per_pct.is_none() {
        let used_pct = 100.0 - latest_pct;
        if used_pct >= 0.05 && window_total_tokens > 0 {
            tokens_per_pct = Some((window_total_tokens as f64 / used_pct, "window"));
        }
    }

    let (tpp, method) = tokens_per_pct?;
    if !tpp.is_finite() || tpp <= 0.0 {
        return None;
    }

    let tokens_since_note: i64 = acct_entries
        .iter()
        .filter(|e| e.epoch > latest_ep && e.epoch <= now_ep)
        .map(|e| e.tokens.total())
        .sum();
    let est_remaining_tokens = (latest_pct * tpp - tokens_since_note as f64).max(0.0);
    let est_remaining_hours = if burn_rate_per_hour > 0.0 && burn_rate_per_hour.is_finite() {
        Some(round2(est_remaining_tokens / burn_rate_per_hour))
    } else {
        None
    };

    Some(json!({
        "remainingPct": round2(latest_pct),
        "tokensPerPct": tpp.round() as i64,
        "estimatedRemainingTokens": est_remaining_tokens.round() as i64,
        "estimatedRemainingHours": est_remaining_hours,
        "burnRatePerHour": burn_rate_per_hour.round() as i64,
        "method": method,
    }))
}

#[derive(Debug, Default)]
pub struct AccountUsageView {
    pub usage: Value,
    pub quota_note: Value,
    pub quota_estimate: Value,
}

#[derive(Debug, Default)]
pub struct AccountsUsageSnapshot {
    pub per_account: HashMap<String, AccountUsageView>,
    pub unassigned: Value,
}

fn window_json(st: &BucketStats, uncounted: usize) -> Value {
    let total = st.total();
    json!({
        "input": total.input,
        "output": total.output,
        "cacheRead": total.cache_read,
        "total": total.total(),
        "author": st.author.to_json_with_total(),
        "reviewer": st.reviewer.to_json_with_total(),
        "rounds": st.author_rounds + st.reviewer_rounds,
        "runs": st.all_runs.len(),
        "liveCounted": st.live_counted,
        "uncountedRunning": uncounted,
    })
}

/// Lock-free, zero-subprocess local 5h & 7d usage + quota snapshot estimates for
/// `agy_accounts::snapshot_json_opts` and `delegate-tui`.
pub fn accounts_usage_snapshot(accounts_map: &Map<String, Value>) -> AccountsUsageSnapshot {
    let now_ep = epoch() as i64;
    let cutoff_5h = now_ep - FIVE_HOURS_SECS;
    let cutoff_7d = now_ep - SEVEN_DAYS_SECS;

    // Fast path: read ledger (`<state>/agy-usage.jsonl`) + active `.slot` runs only.
    let (mut entries, live_running, uncounted_running) =
        collect_all(Some(cutoff_7d), false, false);

    for lr in &live_running {
        if let Some(tok) = lr.tokens {
            entries.push(UsageEntry {
                epoch: now_ep,
                run: lr.run.clone(),
                repo: lr.repo.clone(),
                repo_name: lr.repo_name.clone(),
                role: lr.role.clone(),
                account: lr.account.clone(),
                tokens: tok,
                live: true,
            });
        }
    }

    let mut stats_5h: HashMap<String, BucketStats> = HashMap::new();
    let mut stats_7d: HashMap<String, BucketStats> = HashMap::new();
    let mut uncounted_by_acct: HashMap<String, usize> = HashMap::new();

    for e in &entries {
        let key = e
            .account
            .clone()
            .unwrap_or_else(|| UNASSIGNED_ACCOUNT.to_string());
        stats_7d.entry(key.clone()).or_default().add_entry(e);
        if e.epoch >= cutoff_5h {
            stats_5h.entry(key).or_default().add_entry(e);
        }
    }
    for u in &uncounted_running {
        let key = u
            .account
            .clone()
            .unwrap_or_else(|| UNASSIGNED_ACCOUNT.to_string());
        *uncounted_by_acct.entry(key).or_default() += 1;
    }

    let mut per_account = HashMap::new();
    for (id, entry) in accounts_map {
        let s5 = stats_5h.get(id).cloned().unwrap_or_default();
        let s7 = stats_7d.get(id).cloned().unwrap_or_default();
        let unc = uncounted_by_acct.get(id).copied().unwrap_or(0);

        let usage = json!({
            "fiveHour": window_json(&s5, unc),
            "sevenDay": window_json(&s7, unc),
        });

        let ParsedQuotaNotes {
            points: pts,
            latest_weekly: latest_w,
            latest_five_hour: latest_fh,
        } = parse_quota_notes(entry);
        let quota_note = if latest_w.is_some() || latest_fh.is_some() {
            let mut obj = Map::new();
            if let Some((w, at, _)) = &latest_w {
                obj.insert("weekly".into(), json!(round2(*w)));
                obj.insert("weeklyAt".into(), json!(at));
            }
            if let Some((fh, at, _)) = &latest_fh {
                obj.insert("fiveHour".into(), json!(round2(*fh)));
                obj.insert("fiveHourAt".into(), json!(at));
            }
            Value::Object(obj)
        } else {
            Value::Null
        };

        let acct_entries: Vec<&UsageEntry> = entries
            .iter()
            .filter(|e| e.account.as_deref() == Some(id.as_str()))
            .collect();
        let t5 = s5.total().total();
        let t7 = s7.total().total();
        let burn_rate_per_hour = if t5 > 0 {
            t5 as f64 / 5.0
        } else if t7 > 0 {
            t7 as f64 / 168.0
        } else {
            0.0
        };

        let est_weekly = estimate_quota_window(
            &pts,
            |p| p.weekly,
            SEVEN_DAYS_SECS,
            now_ep,
            &acct_entries,
            t7,
            burn_rate_per_hour,
        );
        let est_five_hour = estimate_quota_window(
            &pts,
            |p| p.five_hour,
            FIVE_HOURS_SECS,
            now_ep,
            &acct_entries,
            t5,
            burn_rate_per_hour,
        );
        let quota_estimate = if est_weekly.is_some() || est_five_hour.is_some() {
            let mut obj = Map::new();
            if let Some(w) = est_weekly {
                obj.insert("weekly".into(), w);
            }
            if let Some(fh) = est_five_hour {
                obj.insert("fiveHour".into(), fh);
            }
            Value::Object(obj)
        } else {
            Value::Null
        };

        per_account.insert(
            id.clone(),
            AccountUsageView {
                usage,
                quota_note,
                quota_estimate,
            },
        );
    }

    let u5 = stats_5h
        .get(UNASSIGNED_ACCOUNT)
        .cloned()
        .unwrap_or_default();
    let u7 = stats_7d
        .get(UNASSIGNED_ACCOUNT)
        .cloned()
        .unwrap_or_default();
    let u_unc = uncounted_by_acct
        .get(UNASSIGNED_ACCOUNT)
        .copied()
        .unwrap_or(0);
    let unassigned = json!({
        "fiveHour": window_json(&u5, u_unc),
        "sevenDay": window_json(&u7, u_unc),
    });

    AccountsUsageSnapshot {
        per_account,
        unassigned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_arithmetic_and_formatting() {
        let a = TokenCounts::new(100, 50, 1000);
        let b = TokenCounts::new(130, 70, 1500);
        assert_eq!(b.sub_clamped(a), TokenCounts::new(30, 20, 500));
        assert_eq!(a.sub_clamped(b), TokenCounts::new(0, 0, 0));
        assert_eq!(format_token_compact(0), "0");
        assert_eq!(format_token_compact(850), "850");
        assert_eq!(format_token_compact(12_500), "12.5K");
        assert_eq!(format_token_compact(4_200_000), "4.2M");
        assert_eq!(format_hours_compact(0.5), "30m");
        assert_eq!(format_hours_compact(5.25), "5h15m");
        assert_eq!(format_hours_compact(72.0), "3d");
    }

    #[test]
    fn quota_estimation_supports_window_and_delta_modes_without_nan() {
        let now = 1_800_000_000i64;
        let e1 = UsageEntry {
            epoch: now - 3600,
            run: "r1".into(),
            repo: "/repo".into(),
            repo_name: "repo".into(),
            role: "author".into(),
            account: Some("work".into()),
            tokens: TokenCounts::new(1_000_000, 500_000, 8_500_000), // 10M total
            live: false,
        };
        // Single snapshot: 95.0% remaining -> 5.0% used -> 10M / 5 = 2M per 1%.
        let p1 = QuotaNotePoint {
            epoch: now - 1800,
            weekly: Some(95.0),
            five_hour: Some(80.0),
        };
        let est1 = estimate_quota_window(
            &[p1],
            |p| p.weekly,
            SEVEN_DAYS_SECS,
            now,
            &[&e1],
            10_000_000,
            2_000_000.0,
        )
        .unwrap();
        assert_eq!(est1["method"], "window");
        assert_eq!(est1["tokensPerPct"], 2_000_000);
        assert_eq!(est1["estimatedRemainingTokens"], 190_000_000);
        assert_eq!(est1["estimatedRemainingHours"], 95.0);

        // Two snapshots with 6M tokens spent between them and 2% drop -> 3M per 1% (delta mode).
        let e2 = UsageEntry {
            epoch: now - 600,
            tokens: TokenCounts::new(1_000_000, 0, 5_000_000), // 6M total
            ..e1.clone()
        };
        let p2 = QuotaNotePoint {
            epoch: now - 300,
            weekly: Some(93.0),
            five_hour: Some(70.0),
        };
        let est2 = estimate_quota_window(
            &[p1, p2],
            |p| p.weekly,
            SEVEN_DAYS_SECS,
            now,
            &[&e1, &e2],
            16_000_000,
            3_200_000.0,
        )
        .unwrap();
        assert_eq!(est2["method"], "delta");
        assert_eq!(est2["tokensPerPct"], 3_000_000);
        assert_eq!(est2["estimatedRemainingTokens"], 279_000_000);

        // 100% remaining (0% used) -> returns None instead of dividing by zero.
        let p_full = QuotaNotePoint {
            epoch: now,
            weekly: Some(100.0),
            five_hour: Some(100.0),
        };
        assert!(estimate_quota_window(
            &[p_full],
            |p| p.weekly,
            SEVEN_DAYS_SECS,
            now,
            &[&e1],
            10_000_000,
            2_000_000.0,
        )
        .is_none());
    }
}
