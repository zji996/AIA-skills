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

pub const ONE_HOUR_SECS: i64 = 3600;
pub const FIVE_HOURS_SECS: i64 = 5 * 3600;
pub const TWENTY_FOUR_HOURS_SECS: i64 = 24 * 3600;
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
    mode: String,
    round: u64,
    rework: bool,
    attempt: i64,
    is_agy: bool,
    provider: String,
    model: String,
    account: Option<String>,
    agy_version: Option<String>,
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
    #[allow(dead_code)]
    pub fn start(meta: &Value, run_dir: &Path, attempt: i64) -> Self {
        let acct = if is_agy_meta(meta) {
            crate::agy_accounts::active_account_id()
        } else {
            None
        };
        Self::start_with_account(meta, run_dir, attempt, acct)
    }

    pub fn start_with_account(
        meta: &Value,
        run_dir: &Path,
        attempt: i64,
        account_override: Option<String>,
    ) -> Self {
        let is_reviewer = b(meta, "isReviewer");
        let role = if is_reviewer { "reviewer" } else { "author" };
        let mode = {
            let m = s(meta, "mode");
            if m.is_empty() {
                if is_reviewer {
                    "read-only".to_string()
                } else {
                    "write".to_string()
                }
            } else {
                m.to_string()
            }
        };
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
            account_override.or_else(crate::agy_accounts::active_account_id)
        } else {
            None
        };
        let agy_version = if is_agy {
            crate::agy::cached_version()
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
            mode,
            round,
            rework,
            attempt,
            is_agy,
            provider,
            model,
            account,
            agy_version,
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
            "mode": self.mode,
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
        if let Some(ref ver) = self.agy_version {
            doc["agyVersion"] = json!(ver);
        }
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
            if let Some(ref ver) = self.agy_version {
                rdoc["agyVersion"] = json!(ver);
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
            "mode": self.mode,
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
        if let Some(ref ver) = self.agy_version {
            rec["agyVersion"] = json!(ver);
        }
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

    // Determine agy account slot ID (never email).
    if last_agy_account.is_none() && is_agy_meta(meta) {
        let live = json(run.join("live-usage.json"));
        last_agy_account = live["agyAccount"]
            .as_str()
            .filter(|x| !x.is_empty())
            .map(str::to_string)
            .or_else(|| {
                let pinned = s(meta, "agyAccount");
                (!pinned.is_empty()).then(|| pinned.to_string())
            })
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
    pub mode: String, // "write" | "read-only" | ""
    pub rework: bool,
    pub model: String,
    pub is_agy: bool,
    pub account: Option<String>,
    pub agy_version: Option<String>,
    pub tokens: TokenCounts,
    pub live: bool,
}

#[derive(Debug, Clone)]
pub struct RunningUsageItem {
    pub run: String,
    pub repo: String,
    pub repo_name: String,
    pub role: String,
    pub mode: String,
    pub rework: bool,
    pub round: u64,
    pub is_agy: bool,
    pub account: Option<String>,
    pub model: String,
    pub started_at: String,
    pub updated_at: String,
    pub tokens: Option<TokenCounts>,
}

fn parse_ledger_entries(since_epoch: Option<i64>, include_non_agy: bool) -> (Vec<UsageEntry>, HashSet<String>) {
    parse_ledger_entries_from(&ledger_path(), since_epoch, include_non_agy)
}

pub(crate) fn parse_ledger_entries_from(
    path: &Path,
    since_epoch: Option<i64>,
    include_non_agy: bool,
) -> (Vec<UsageEntry>, HashSet<String>) {
    let mut entries = Vec::new();
    let mut ledger_runs = HashSet::new();
    if !path.is_file() {
        return (entries, ledger_runs);
    }
    for line in read(path).lines() {
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
        let mode = s(&v, "mode").to_string();
        let rework = b(&v, "rework") || (role != "reviewer" && v["round"].as_u64().unwrap_or(1) > 1);
        let model = s(&v, "model").to_string();
        let account = v["account"]
            .as_str()
            .filter(|a| !a.is_empty())
            .map(str::to_string);
        let agy_version = v["agyVersion"]
            .as_str()
            .map(str::trim)
            .filter(|ver| !ver.is_empty())
            .map(str::to_string);
        entries.push(UsageEntry {
            epoch: ep,
            run,
            repo,
            repo_name,
            role,
            mode,
            rework,
            model,
            is_agy,
            account,
            agy_version,
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
        } else if run_dir.file_name().and_then(|n| n.to_str()) == Some("record") {
            run_dir
                .parent()
                .and_then(|p| p.file_name())
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
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

    let mode = s(&meta, "mode").to_string();
    let author_rework = b(&meta, "isRework") || !s(&meta, "parent").is_empty();
    let author_model = s(&meta, "model").to_string();

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
                mode: mode.clone(),
                rework: author_rework,
                model: author_model.clone(),
                is_agy,
                account: acct.clone(),
                agy_version: None,
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
                mode: "read-only".into(),
                rework: false,
                model: s(&rv, "model").to_string(),
                is_agy: rv_agy,
                account: rv["agyAccount"]
                    .as_str()
                    .filter(|a| !a.is_empty())
                    .map(str::to_string)
                    .or_else(|| acct.clone()),
                agy_version: rv["agyVersion"]
                    .as_str()
                    .map(str::trim)
                    .filter(|ver| !ver.is_empty())
                    .map(str::to_string),
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
                    mode: "read-only".into(),
                    rework: false,
                    model: rev_model.to_string(),
                    is_agy: rev_agy,
                    account: sum["review"]["agyAccount"]
                        .as_str()
                        .filter(|a| !a.is_empty())
                        .map(str::to_string)
                        .or(acct),
                    agy_version: None,
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
        } else if run_dir.file_name().and_then(|n| n.to_str()) == Some("record") {
            run_dir
                .parent()
                .and_then(|p| p.file_name())
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
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
                mode: {
                    let m = s(&lv, "mode");
                    if m.is_empty() {
                        s(&meta, "mode").to_string()
                    } else {
                        m.to_string()
                    }
                },
                rework: b(&lv, "rework"),
                round: lv["round"].as_u64().unwrap_or(1),
                is_agy: lv_agy,
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
        mode: s(&meta, "mode").to_string(),
        rework: b(&meta, "isRework"),
        round: 1,
        is_agy,
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

/// Return total tokens (`input + output + cacheRead`, across all models including agy and Pi,
/// author + reviewer, ledger + disk/archive backfill + live) grouped by `run_id`, with no window cutoff.
pub fn all_run_tokens_map() -> HashMap<String, TokenCounts> {
    let (entries, live_running, _) = collect_all(None, true, true);
    let mut by_run: HashMap<String, TokenCounts> = HashMap::new();
    for e in entries {
        if !e.run.is_empty() {
            by_run.entry(e.run).or_default().add(e.tokens);
        }
    }
    for lr in live_running {
        if let Some(tok) = lr.tokens {
            if !lr.run.is_empty() {
                by_run.entry(lr.run).or_default().add(tok);
            }
        }
    }
    by_run
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

struct CmdOutRunInfo {
    group: String,
    triggers: u64,
    truncated_bytes: u64,
    author_steps: u64,
    compile_test_steps: u64,
    author_input_tokens: i64,
    has_accept: bool,
    accept_first_pass: bool,
}

fn inspect_cmd_out_run(rdir: &Path) -> Option<(String, CmdOutRunInfo)> {
    let meta = json(rdir.join("meta.json"));
    let sum = json(rdir.join("summary.json"));
    let group = {
        let g = s(&meta, "cmdOutGroup");
        if !g.is_empty() {
            g.to_string()
        } else {
            s(&sum["cmdOut"], "group").to_string()
        }
    };
    if group.is_empty() {
        return None;
    }
    let run_id = {
        let r = s(&meta, "run");
        if !r.is_empty() {
            r.to_string()
        } else if rdir.file_name().and_then(|n| n.to_str()) == Some("record") {
            rdir.parent()
                .and_then(|p| p.file_name())
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        } else {
            rdir.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }
    };
    let (triggers, truncated_bytes) = if sum["cmdOut"].is_object() {
        (
            sum["cmdOut"]["triggers"].as_u64().unwrap_or(0),
            sum["cmdOut"]["truncatedBytes"].as_u64().unwrap_or(0),
        )
    } else {
        let st = crate::cmd_out::count_stats(rdir);
        (st.triggers, st.truncated_bytes)
    };
    let (author_steps, compile_test_steps) =
        if sum["cmdOut"]["authorSteps"].as_u64().is_some() {
            (
                sum["cmdOut"]["authorSteps"].as_u64().unwrap_or(0),
                sum["cmdOut"]["compileTestSteps"].as_u64().unwrap_or(0),
            )
        } else {
            let configured = crate::cmd_out::parse_commands(&meta["cmdOut"]["commands"]);
            crate::cmd_out::count_author_steps(rdir, &configured)
        };
    let author_input_tokens = runs::effective_tokens_delta(&sum)
        .map(|tok| n(&tok, "input").saturating_add(n(&tok, "cacheRead")))
        .unwrap_or(0);
    let is_first_round = s(&meta, "parent").is_empty() && meta["rework"].is_null();
    let has_accept =
        is_first_round && sum["accept"].is_object() && !b(&sum["accept"], "notExecuted");
    let accept_first_pass = has_accept
        && b(&sum["accept"], "ok")
        && sum["review"]["reworkRounds"].as_u64().unwrap_or(0) == 0;
    Some((
        run_id,
        CmdOutRunInfo {
            group,
            triggers,
            truncated_bytes,
            author_steps,
            compile_test_steps,
            author_input_tokens,
            has_accept,
            accept_first_pass,
        },
    ))
}

fn collect_cmd_out_run_info(state: &Path) -> HashMap<String, CmdOutRunInfo> {
    let mut map = HashMap::new();
    let repos = crate::index::discover_snapshot_repos(state, &[], None);
    for (_, runs_dir) in repos {
        for rdir in runs::all_runs_in(&runs_dir) {
            if let Some((id, info)) = inspect_cmd_out_run(&rdir) {
                map.entry(id).or_insert(info);
            }
        }
    }
    let arch_root = cache_dir().join("archive");
    if let Ok(rd) = fs::read_dir(&arch_root) {
        for ent in rd.flatten() {
            let rec = ent.path().join("record");
            if rec.is_dir() {
                if let Some((id, info)) = inspect_cmd_out_run(&rec) {
                    map.entry(id).or_insert(info);
                }
            }
        }
    }
    map
}

/// CLI entrypoint: `delegate usage [--since 24h|7d|all] [--by account|day|run|repo|cmd-out] [--account ID] [--repo PATH] [--all] [--json]`
pub fn command(args: &[String]) -> Res<i32> {
    let (pos, flags, kv) = parse_simple(
        "usage",
        args,
        &["--json", "--all"],
        &["--since", "--by", "--account", "--repo"],
    )?;
    if !pos.is_empty() {
        return Err(unrecognized_arg(
            &pos.join(" "),
            &["--json", "--all", "--since", "--by", "--account", "--repo"],
            "usage",
        ));
    }
    let json_out = has(&flags, "--json");
    let include_non_agy = has(&flags, "--all");
    let by = value(&kv, "--by").unwrap_or("account");
    if !matches!(by, "account" | "day" | "run" | "repo" | "cmd-out" | "quota-rate") {
        return Err(format!(
            "--by must be one of account|day|run|repo|cmd-out|quota-rate, got {by:?}"
        ));
    }
    let since_str = value(&kv, "--since").unwrap_or(if by == "quota-rate" { "7d" } else { "24h" });
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

    if by == "quota-rate" {
        return crate::quota_rate::run_cli_quota_rate(
            since_str,
            since_epoch,
            &since_iso,
            account_filter,
            json_out,
            now_ep,
        );
    }

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
                mode: lr.mode.clone(),
                rework: lr.rework,
                model: lr.model.clone(),
                is_agy: lr.is_agy,
                account: lr.account.clone(),
                agy_version: None,
                tokens: tok,
                live: true,
            });
        }
    }

    let cmd_out_info = if by == "cmd-out" {
        collect_cmd_out_run_info(&state_dir())
    } else {
        HashMap::new()
    };

    let mut grand = BucketStats::default();
    let mut buckets: BTreeMap<String, BucketStats> = BTreeMap::new();
    let mut author_input_by_run: HashMap<String, i64> = HashMap::new();
    for e in &entries {
        grand.add_entry(e);
        if e.role != "reviewer" {
            *author_input_by_run.entry(e.run.clone()).or_insert(0) +=
                e.tokens.input.saturating_add(e.tokens.cache_read);
        }
        let key = match by {
            "account" => e
                .account
                .clone()
                .unwrap_or_else(|| UNASSIGNED_ACCOUNT.to_string()),
            "day" => format_epoch(e.epoch, true, "%Y-%m-%d"),
            "run" => e.run.clone(),
            "repo" => e.repo_name.clone(),
            "cmd-out" => cmd_out_info
                .get(&e.run)
                .map(|i| i.group.clone())
                .unwrap_or_else(|| "(none)".to_string()),
            _ => unreachable!(),
        };
        buckets.entry(key).or_default().add_entry(e);
    }

    let mut cmd_out_ab: BTreeMap<String, crate::cmd_out::CmdOutGroupMetrics> = BTreeMap::new();
    if by == "cmd-out" {
        let mut seen_runs: HashSet<String> = HashSet::new();
        for e in &entries {
            if !seen_runs.insert(e.run.clone()) {
                continue;
            }
            if let Some(info) = cmd_out_info.get(&e.run) {
                let m = cmd_out_ab.entry(info.group.clone()).or_default();
                m.runs += 1;
                m.triggers = m.triggers.saturating_add(info.triggers);
                m.truncated_bytes = m.truncated_bytes.saturating_add(info.truncated_bytes);
                m.author_steps = m.author_steps.saturating_add(info.author_steps);
                m.compile_test_steps = m.compile_test_steps.saturating_add(info.compile_test_steps);
                let run_in = author_input_by_run
                    .get(&e.run)
                    .copied()
                    .unwrap_or(info.author_input_tokens);
                m.author_input_tokens = m.author_input_tokens.saturating_add(run_in);
                if info.has_accept {
                    m.accept_total += 1;
                    if info.accept_first_pass {
                        m.accept_first_pass += 1;
                    }
                }
            }
        }
    }

    let mut rows: Vec<(String, BucketStats)> = buckets.into_iter().collect();
    match by {
        "day" | "cmd-out" => rows.sort_by(|a, b| a.0.cmp(&b.0)),
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
                } else if by == "cmd-out" {
                    if let Some(m) = cmd_out_ab.get(k) {
                        obj["cmdOut"] = m.to_json();
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
        let mut out_doc = json!({
            "since": since_str,
            "sinceIso": since_iso,
            "by": by,
            "provider": if include_non_agy { "all" } else { "agy" },
            "generatedAt": iso(),
            "totals": grand.to_json(),
            "groups": groups_json,
            "liveRunning": live_json,
            "uncountedRunning": uncounted_json,
        });
        if by == "cmd-out" {
            let ab_map: Map<String, Value> = cmd_out_ab
                .iter()
                .map(|(k, v)| (k.clone(), v.to_json()))
                .collect();
            out_doc["cmdOutAb"] = Value::Object(ab_map);
        }
        println!("{}", out_doc);
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
            "cmd-out" => "cmdOut分组",
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
    if by == "cmd-out" && !cmd_out_ab.is_empty() {
        print!("{}", crate::cmd_out::format_ab_summary_human(&cmd_out_ab));
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
// Fast Per-Account 1h / 5h / 7d Token Usage + Manual Quota Notes
// (Quota depletion forecasting lives solely in `agy_accounts.rs` using `/usage`
// same-cycle percentage history, never local token calibration.)
// ---------------------------------------------------------------------------

struct ParsedQuotaNotes {
    latest_weekly: Option<(f64, String, i64)>,
    latest_five_hour: Option<(f64, String, i64)>,
}

fn parse_quota_notes(account_entry: &Value) -> ParsedQuotaNotes {
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
    ParsedQuotaNotes {
        latest_weekly,
        latest_five_hour,
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[derive(Debug, Default)]
pub struct AccountUsageView {
    pub usage: Value,
    pub quota_note: Value,
    pub last_used_at: Option<String>,
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

fn fill_hourly_24h(entries: &[&UsageEntry], now_ep: i64) -> [u64; 24] {
    let mut buckets = [0u64; 24];
    for e in entries {
        let age = now_ep - e.epoch;
        if !(0..TWENTY_FOUR_HOURS_SECS).contains(&age) {
            continue;
        }
        let hours_ago = (age / ONE_HOUR_SECS) as usize;
        if hours_ago < 24 {
            let idx = 23 - hours_ago;
            buckets[idx] = buckets[idx].saturating_add(e.tokens.total().max(0) as u64);
        }
    }
    buckets
}

/// Single-pass 7-day usage collection shared between `agy_accounts::snapshot_json_fast_with_usage`
/// and `tui_analytics_and_deliveries_with_usage` so `agy-usage.jsonl` is read only once per snapshot.
pub struct PrecollectedUsage {
    pub now_ep: i64,
    entries: Vec<UsageEntry>,
    uncounted_running: Vec<RunningUsageItem>,
}

pub fn collect_snapshot_usage_7d(now_ep: i64) -> PrecollectedUsage {
    let cutoff_7d = now_ep - SEVEN_DAYS_SECS;
    let (mut entries, live_running, uncounted_running) = collect_all(Some(cutoff_7d), true, false);
    for lr in &live_running {
        if let Some(tok) = lr.tokens {
            entries.push(UsageEntry {
                epoch: now_ep,
                run: lr.run.clone(),
                repo: lr.repo.clone(),
                repo_name: lr.repo_name.clone(),
                role: lr.role.clone(),
                mode: lr.mode.clone(),
                rework: lr.rework,
                model: lr.model.clone(),
                is_agy: lr.is_agy,
                account: lr.account.clone(),
                agy_version: None,
                tokens: tok,
                live: true,
            });
        }
    }
    PrecollectedUsage {
        now_ep,
        entries,
        uncounted_running,
    }
}

/// Lock-free, zero-subprocess local 1h / 5h / 7d token usage + 24h hourly series + manual notes
/// for `agy_accounts` and `delegate-tui` charts/statistics.
pub fn accounts_usage_snapshot(accounts_map: &Map<String, Value>) -> AccountsUsageSnapshot {
    let now_ep = epoch() as i64;
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
                mode: lr.mode.clone(),
                rework: lr.rework,
                model: lr.model.clone(),
                is_agy: lr.is_agy,
                account: lr.account.clone(),
                agy_version: None,
                tokens: tok,
                live: true,
            });
        }
    }
    let entry_refs: Vec<&UsageEntry> = entries.iter().collect();
    let unc_refs: Vec<&RunningUsageItem> = uncounted_running.iter().collect();
    accounts_usage_snapshot_from_entries(accounts_map, now_ep, &entry_refs, &unc_refs)
}

pub fn accounts_usage_snapshot_from_precollected(
    accounts_map: &Map<String, Value>,
    pre: &PrecollectedUsage,
) -> AccountsUsageSnapshot {
    let entry_refs: Vec<&UsageEntry> = pre.entries.iter().filter(|e| e.is_agy).collect();
    let unc_refs: Vec<&RunningUsageItem> =
        pre.uncounted_running.iter().filter(|u| u.is_agy).collect();
    accounts_usage_snapshot_from_entries(accounts_map, pre.now_ep, &entry_refs, &unc_refs)
}

fn accounts_usage_snapshot_from_entries(
    accounts_map: &Map<String, Value>,
    now_ep: i64,
    entries: &[&UsageEntry],
    uncounted_running: &[&RunningUsageItem],
) -> AccountsUsageSnapshot {
    let cutoff_1h = now_ep - ONE_HOUR_SECS;
    let cutoff_5h = now_ep - FIVE_HOURS_SECS;

    let mut stats_1h: HashMap<String, BucketStats> = HashMap::new();
    let mut stats_5h: HashMap<String, BucketStats> = HashMap::new();
    let mut stats_7d: HashMap<String, BucketStats> = HashMap::new();
    let mut uncounted_by_acct: HashMap<String, usize> = HashMap::new();

    for &e in entries {
        let key = e
            .account
            .clone()
            .unwrap_or_else(|| UNASSIGNED_ACCOUNT.to_string());
        stats_7d.entry(key.clone()).or_default().add_entry(e);
        if e.epoch >= cutoff_5h {
            stats_5h.entry(key.clone()).or_default().add_entry(e);
        }
        if e.epoch >= cutoff_1h {
            stats_1h.entry(key).or_default().add_entry(e);
        }
    }
    for &u in uncounted_running {
        let key = u
            .account
            .clone()
            .unwrap_or_else(|| UNASSIGNED_ACCOUNT.to_string());
        *uncounted_by_acct.entry(key).or_default() += 1;
    }

    let mut per_account = HashMap::new();
    for (id, entry) in accounts_map {
        let s1 = stats_1h.get(id).cloned().unwrap_or_default();
        let s5 = stats_5h.get(id).cloned().unwrap_or_default();
        let s7 = stats_7d.get(id).cloned().unwrap_or_default();
        let unc = uncounted_by_acct.get(id).copied().unwrap_or(0);

        let acct_entries: Vec<&UsageEntry> = entries
            .iter()
            .copied()
            .filter(|e| e.account.as_deref() == Some(id.as_str()))
            .collect();
        let hourly_24h = fill_hourly_24h(&acct_entries, now_ep);

        let usage = json!({
            "oneHour": window_json(&s1, unc),
            "fiveHour": window_json(&s5, unc),
            "sevenDay": window_json(&s7, unc),
            "hourly24h": hourly_24h,
        });

        let ParsedQuotaNotes {
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

        let last_used_at = acct_entries
            .iter()
            .map(|e| e.epoch)
            .filter(|&ep| ep > 0)
            .max()
            .map(|ep| format_epoch(ep, false, "%Y-%m-%dT%H:%M:%SZ"));

        per_account.insert(
            id.clone(),
            AccountUsageView {
                usage,
                quota_note,
                last_used_at,
            },
        );
    }

    let u1 = stats_1h
        .get(UNASSIGNED_ACCOUNT)
        .cloned()
        .unwrap_or_default();
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
    let unassigned_entries: Vec<&UsageEntry> = entries
        .iter()
        .copied()
        .filter(|e| e.account.is_none())
        .collect();
    let unassigned = json!({
        "oneHour": window_json(&u1, u_unc),
        "fiveHour": window_json(&u5, u_unc),
        "sevenDay": window_json(&u7, u_unc),
        "hourly24h": fill_hourly_24h(&unassigned_entries, now_ep),
    });

    AccountsUsageSnapshot {
        per_account,
        unassigned,
    }
}

// ---------------------------------------------------------------------------
// Lock-Free TUI Analytics Snapshot (24h Hourly, Purpose/Model Distribution, 7d Trend)
// ---------------------------------------------------------------------------

fn short_model_label(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "unknown".to_string();
    }
    let stripped = trimmed
        .strip_prefix("agy/")
        .or_else(|| trimmed.strip_prefix("anthropic/"))
        .or_else(|| trimmed.strip_prefix("openai/"))
        .or_else(|| trimmed.strip_prefix("google/"))
        .unwrap_or(trimmed);
    stripped.to_string()
}

fn load_recent_history_records(
    state_root: &Path,
    all_seen_runs: &BTreeMap<String, Value>,
    cutoff_7d: i64,
    now_ep: i64,
) -> HashMap<String, crate::history::RecentHistoryRecord> {
    // 1. Lock-free single-pass tail-chunk read of `<state>/history/*/history.jsonl`.
    let mut map = crate::history::read_recent_window_all_repos(state_root, cutoff_7d);

    // 2. Overlay currently known live/uncleaned runs from `snapshot_command`.
    for (rid, st) in all_seen_runs {
        if rid.is_empty() {
            continue;
        }
        let started = iso_epoch(s(st, "startedAt"))
            .or_else(|| st["startedEpoch"].as_i64())
            .unwrap_or_else(|| {
                let age = st["ageSeconds"].as_i64().unwrap_or(0);
                if age > 0 {
                    now_ep - age
                } else {
                    0
                }
            });
        let finished = iso_epoch(s(st, "finishedAt")).unwrap_or(0);
        let applied_str = s(st, "appliedAt");
        let applied = iso_epoch(applied_str).or_else(|| {
            if b(st, "applied") {
                Some(finished.max(started))
            } else {
                None
            }
        });
        let rw = st["reworkCount"]
            .as_u64()
            .unwrap_or(0)
            .max(st["review"]["reworkRounds"].as_u64().unwrap_or(0));
        let rec = map
            .entry(rid.clone())
            .or_insert_with(|| crate::history::RecentHistoryRecord {
                run: rid.clone(),
                ..crate::history::RecentHistoryRecord::default()
            });
        let mode = s(st, "mode");
        let state = s(st, "state");
        let parent = s(st, "parent");
        let name = s(st, "name");
        if !name.is_empty() {
            rec.name = name.to_string();
        }
        if !mode.is_empty() {
            rec.mode = mode.to_string();
        }
        if !state.is_empty() {
            rec.state = state.to_string();
        }
        if !parent.is_empty() {
            rec.parent = parent.to_string();
        }
        if started > 0 {
            rec.started_epoch = started;
        }
        if finished > 0 {
            rec.finished_epoch = finished;
        } else if rec.finished_epoch == 0 && started > 0 {
            rec.finished_epoch = started;
        }
        if let Some(app_ep) = applied {
            rec.applied_epoch = Some(app_ep);
            if !applied_str.is_empty() {
                rec.applied_at = applied_str.to_string();
            } else if rec.applied_at.is_empty() && app_ep > 0 {
                rec.applied_at = format_epoch(app_ep, false, "%Y-%m-%dT%H:%M:%SZ");
            }
        }
        let app_head = s(st, "appliedHead");
        if !app_head.is_empty() {
            rec.applied_head = app_head.to_string();
        }
        let elapsed = st["elapsedSeconds"].as_i64().unwrap_or(0);
        if elapsed > 0 {
            rec.elapsed_seconds = elapsed;
        }
        if st["changes"].is_object() {
            let cf = st["changes"]["files"].as_u64().unwrap_or(0) as usize;
            let add = n(&st["changes"], "added");
            let del = n(&st["changes"], "deleted");
            if cf > 0 || add > 0 || del > 0 {
                rec.changes_files = cf;
                rec.added = add;
                rec.deleted = del;
            }
        }
        let src = s(st, "worktreeSource");
        if !src.is_empty() {
            rec.source_root = src.to_string();
            if let Some(base) = Path::new(src).file_name().and_then(|x| x.to_str()) {
                if !base.is_empty() {
                    rec.repo_label = base.to_string();
                }
            }
        }
        rec.rework_count = rec.rework_count.max(rw);
    }

    map
}

fn compute_daily_7d_and_deliveries(
    records: &HashMap<String, crate::history::RecentHistoryRecord>,
    work_items: &[crate::work::WorkItem],
    now_ep: i64,
    cutoff_7d: i64,
) -> (Vec<Value>, Vec<Value>, usize) {
    // Build 7 local calendar days: D-6 ..= D-0
    let mut days: Vec<(String, String)> = Vec::with_capacity(7);
    for d in (0..7).rev() {
        let ep = now_ep - d * 86400;
        let ymd = format_epoch(ep, true, "%Y-%m-%d");
        let md = format_epoch(ep, true, "%m-%d");
        days.push((ymd, md));
    }
    let today_ymd = days.last().map(|(y, _)| y.clone()).unwrap_or_default();
    let day_index: HashMap<String, usize> = days
        .iter()
        .enumerate()
        .map(|(i, (ymd, _))| (ymd.clone(), i))
        .collect();

    // Map run_id -> work_id so recent deliveries can show `[Wxx]` badges.
    let mut run_to_work: HashMap<&str, &str> = HashMap::new();
    for w in work_items {
        for rid in &w.runs {
            run_to_work.entry(rid.as_str()).or_insert(w.id.as_str());
        }
    }

    // Identify parent runs so we count each reply chain once (at its head/latest run).
    let mut has_child: HashSet<&str> = HashSet::new();
    for rec in records.values() {
        if !rec.parent.is_empty() {
            has_child.insert(rec.parent.as_str());
        }
    }

    let mut merge_secs_by_day: Vec<Vec<u64>> = vec![Vec::new(); 7];
    let mut total_chains_by_day = [0u64; 7];
    let mut first_pass_by_day = [0u64; 7];

    // Deduplicate recent applied deliveries by chain root ID (no hard count cap; 7d window only).
    struct DeliveryAcc {
        run: String,
        name: String,
        repo: String,
        source_root: String,
        applied_at: String,
        applied_epoch: i64,
        applied_head: String,
        changes_files: usize,
        added: i64,
        deleted: i64,
        elapsed_seconds: i64,
        rounds: usize,
        conclusion: String,
        work_id: Option<String>,
        cleaned: bool,
    }
    let mut deliveries_by_root: HashMap<String, DeliveryAcc> = HashMap::new();
    let mut merged_today_roots: HashSet<String> = HashSet::new();

    for rec in records.values() {
        if has_child.contains(rec.run.as_str()) {
            continue;
        }
        if rec.mode != "write" && !rec.mode.is_empty() {
            continue;
        }
        // Walk up to root of this chain to get initial dispatch timestamp, root name, work link & rounds.
        let mut root_id = rec.run.as_str();
        let mut root_name = crate::notes::strip_reply_prefixes(&rec.name).0.to_string();
        let mut root_started = rec.started_epoch;
        let mut chain_len = 1usize;
        let mut total_rework = rec.rework_count;
        let mut work_id: Option<String> = run_to_work.get(rec.run.as_str()).map(|w| (*w).to_string());
        let mut best_conclusion = rec.conclusion.clone();
        let mut best_repo = rec.repo_label.clone();
        let mut best_source = rec.source_root.clone();
        let mut best_files = rec.changes_files;
        let mut best_added = rec.added;
        let mut best_deleted = rec.deleted;
        let mut total_elapsed = rec.elapsed_seconds.max(0);
        let mut cur_parent = rec.parent.as_str();
        let mut visited = HashSet::new();
        visited.insert(rec.run.as_str());
        while !cur_parent.is_empty() && visited.insert(cur_parent) {
            chain_len += 1;
            root_id = cur_parent;
            if work_id.is_none() {
                work_id = run_to_work.get(cur_parent).map(|w| (*w).to_string());
            }
            if let Some(p) = records.get(cur_parent) {
                if !p.name.is_empty() {
                    let stripped = crate::notes::strip_reply_prefixes(&p.name).0;
                    if !stripped.is_empty() {
                        root_name = stripped.to_string();
                    }
                }
                if p.started_epoch > 0 && (root_started == 0 || p.started_epoch < root_started) {
                    root_started = p.started_epoch;
                }
                if best_conclusion.is_empty() && !p.conclusion.is_empty() {
                    best_conclusion = p.conclusion.clone();
                }
                if best_repo.is_empty() && !p.repo_label.is_empty() {
                    best_repo = p.repo_label.clone();
                }
                if best_source.is_empty() && !p.source_root.is_empty() {
                    best_source = p.source_root.clone();
                }
                if best_files == 0 && p.changes_files > 0 {
                    best_files = p.changes_files;
                    best_added = p.added;
                    best_deleted = p.deleted;
                }
                if p.elapsed_seconds > 0 {
                    total_elapsed = total_elapsed.saturating_add(p.elapsed_seconds);
                }
                total_rework = total_rework.saturating_add(p.rework_count);
                cur_parent = p.parent.as_str();
            } else {
                break;
            }
        }

        // 1. Dispatch -> merge latency (bucketed by local date of appliedAt) + recentDeliveries
        if let Some(app_ep) = rec.applied_epoch {
            let ymd = format_epoch(app_ep, true, "%Y-%m-%d");
            if let Some(&idx) = day_index.get(&ymd) {
                if root_started > 0 && app_ep >= root_started {
                    merge_secs_by_day[idx].push((app_ep - root_started) as u64);
                }
            }
            if !today_ymd.is_empty() && ymd == today_ymd {
                merged_today_roots.insert(root_id.to_string());
            }
            if app_ep >= cutoff_7d {
                let rounds = chain_len.saturating_add(total_rework as usize).max(1);
                let elapsed = if total_elapsed > 0 {
                    total_elapsed
                } else if root_started > 0 && rec.finished_epoch >= root_started {
                    rec.finished_epoch - root_started
                } else if root_started > 0 && app_ep >= root_started {
                    app_ep - root_started
                } else {
                    0
                };
                let display_name = if root_name.is_empty() {
                    rec.run.clone()
                } else {
                    root_name.clone()
                };
                let applied_at = if !rec.applied_at.is_empty() {
                    rec.applied_at.clone()
                } else {
                    format_epoch(app_ep, false, "%Y-%m-%dT%H:%M:%SZ")
                };
                let acc = DeliveryAcc {
                    run: rec.run.clone(),
                    name: display_name,
                    repo: best_repo,
                    source_root: best_source,
                    applied_at,
                    applied_epoch: app_ep,
                    applied_head: rec.applied_head.clone(),
                    changes_files: best_files,
                    added: best_added,
                    deleted: best_deleted,
                    elapsed_seconds: elapsed,
                    rounds,
                    conclusion: best_conclusion,
                    work_id,
                    cleaned: rec.cleaned,
                };
                let replace = deliveries_by_root
                    .get(root_id)
                    .is_none_or(|prev| acc.applied_epoch >= prev.applied_epoch);
                if replace {
                    deliveries_by_root.insert(root_id.to_string(), acc);
                }
            }
        }

        // 2. First-round pass rate (finished write chains bucketed by finishedAt/appliedAt)
        let is_terminal = matches!(
            rec.state.as_str(),
            "delivered" | "rejected" | "failed" | "timeout" | "stalled" | "crashed" | "malformed"
        ) || rec.applied_epoch.is_some();
        if !is_terminal {
            continue;
        }
        let ref_ep = rec
            .applied_epoch
            .unwrap_or(rec.finished_epoch.max(rec.started_epoch));
        if ref_ep <= 0 {
            continue;
        }
        let ymd = format_epoch(ref_ep, true, "%Y-%m-%d");
        if let Some(&idx) = day_index.get(&ymd) {
            total_chains_by_day[idx] += 1;
            let passed = rec.state == "delivered" || rec.applied_epoch.is_some();
            if passed && chain_len == 1 && total_rework == 0 {
                first_pass_by_day[idx] += 1;
            }
        }
    }

    let daily_7d: Vec<Value> = days
        .into_iter()
        .enumerate()
        .map(|(i, (ymd, md))| {
            let list = &mut merge_secs_by_day[i];
            list.sort_unstable();
            let merged_count = list.len() as u64;
            let median_merge_secs = if list.is_empty() {
                None
            } else {
                Some(list[list.len() / 2])
            };
            let avg_merge_secs = if list.is_empty() {
                None
            } else {
                Some(list.iter().sum::<u64>() / merged_count)
            };
            let total_chains = total_chains_by_day[i];
            let first_pass = first_pass_by_day[i];
            let pass_pct = (first_pass * 100 + total_chains / 2).checked_div(total_chains);
            json!({
                "ymd": ymd,
                "date": md,
                "mergedCount": merged_count,
                "medianMergeSecs": median_merge_secs,
                "avgMergeSecs": avg_merge_secs,
                "totalChains": total_chains,
                "firstRoundPass": first_pass,
                "firstRoundPassPct": pass_pct,
            })
        })
        .collect();

    let mut deliveries: Vec<DeliveryAcc> = deliveries_by_root.into_values().collect();
    deliveries.sort_by(|a, b| {
        b.applied_epoch
            .cmp(&a.applied_epoch)
            .then_with(|| b.run.cmp(&a.run))
    });
    let recent_deliveries: Vec<Value> = deliveries
        .into_iter()
        .map(|d| {
            let mut obj = json!({
                "run": d.run,
                "name": d.name,
                "repo": d.repo,
                "sourceRoot": d.source_root,
                "appliedAt": d.applied_at,
                "appliedEpoch": d.applied_epoch,
                "appliedHead": d.applied_head,
                "changesFiles": d.changes_files,
                "added": d.added,
                "deleted": d.deleted,
                "elapsedSeconds": d.elapsed_seconds,
                "rounds": d.rounds,
                "conclusion": d.conclusion,
                "cleaned": d.cleaned,
            });
            if let Some(wid) = d.work_id {
                obj["workId"] = json!(wid);
            }
            obj
        })
        .collect();

    (daily_7d, recent_deliveries, merged_today_roots.len())
}

/// Combined single-pass output of 7d analytics, 7d recent deliveries (no hard count cap),
/// and local-today merged chain count.
pub struct AnalyticsAndDeliveries {
    pub analytics: Value,
    pub recent_deliveries: Vec<Value>,
    pub merged_today: usize,
}

/// Lock-free, zero-subprocess analytics + recent-deliveries bundle embedded in `delegate snapshot --json`.
#[allow(dead_code)]
pub fn tui_analytics_and_deliveries(
    state_root: &Path,
    all_seen_runs: &BTreeMap<String, Value>,
    work_items: &[crate::work::WorkItem],
    now_ep: i64,
) -> AnalyticsAndDeliveries {
    tui_analytics_and_deliveries_with_usage(state_root, all_seen_runs, work_items, now_ep, None)
}

pub fn tui_analytics_and_deliveries_with_usage(
    state_root: &Path,
    all_seen_runs: &BTreeMap<String, Value>,
    work_items: &[crate::work::WorkItem],
    now_ep: i64,
    precollected: Option<&PrecollectedUsage>,
) -> AnalyticsAndDeliveries {
    let cutoff_7d = now_ep - SEVEN_DAYS_SECS;

    let owned_pre;
    let entries: &[UsageEntry] = if let Some(pre) = precollected {
        &pre.entries
    } else {
        owned_pre = collect_snapshot_usage_7d(now_ep);
        &owned_pre.entries
    };

    let entry_refs: Vec<&UsageEntry> = entries.iter().collect();
    let total_hourly_24h = fill_hourly_24h(&entry_refs, now_ep);

    let mut by_acct_refs: BTreeMap<String, Vec<&UsageEntry>> = BTreeMap::new();
    let mut purpose_write = 0u64;
    let mut purpose_review = 0u64;
    let mut purpose_rework = 0u64;
    let mut purpose_readonly = 0u64;
    let mut tok_input = 0u64;
    let mut tok_output = 0u64;
    let mut tok_cache_read = 0u64;
    let mut by_model_map: HashMap<String, u64> = HashMap::new();

    for e in entries {
        let tok = e.tokens.total().max(0) as u64;
        tok_input = tok_input.saturating_add(e.tokens.input.max(0) as u64);
        tok_output = tok_output.saturating_add(e.tokens.output.max(0) as u64);
        tok_cache_read = tok_cache_read.saturating_add(e.tokens.cache_read.max(0) as u64);
        if e.is_agy {
            let acct = e
                .account
                .clone()
                .unwrap_or_else(|| UNASSIGNED_ACCOUNT.to_string());
            by_acct_refs.entry(acct).or_default().push(e);
        }
        let eff_mode = if !e.mode.is_empty() {
            e.mode.as_str()
        } else {
            all_seen_runs
                .get(&e.run)
                .map(|st| s(st, "mode"))
                .unwrap_or("")
        };
        if e.role == "reviewer" {
            purpose_review = purpose_review.saturating_add(tok);
        } else if e.rework {
            purpose_rework = purpose_rework.saturating_add(tok);
        } else if eff_mode == "read-only" {
            purpose_readonly = purpose_readonly.saturating_add(tok);
        } else {
            purpose_write = purpose_write.saturating_add(tok);
        }
        let mlabel = short_model_label(&e.model);
        *by_model_map.entry(mlabel).or_default() += tok;
    }

    let mut by_acct_json = Map::new();
    for (acct, list) in by_acct_refs {
        by_acct_json.insert(acct, json!(fill_hourly_24h(&list, now_ep)));
    }

    // Output all models sorted by tokens descending (no hard truncation cap; UI folds overflow).
    let mut models_vec: Vec<(String, u64)> = by_model_map.into_iter().collect();
    models_vec.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let by_model_json: Vec<Value> = models_vec
        .into_iter()
        .map(|(m, t)| json!({"model": m, "tokens": t}))
        .collect();

    let recent_records = load_recent_history_records(state_root, all_seen_runs, cutoff_7d, now_ep);
    let (daily_7d, recent_deliveries, merged_today) =
        compute_daily_7d_and_deliveries(&recent_records, work_items, now_ep, cutoff_7d);

    let analytics = json!({
        "hourly24h": {
            "total": total_hourly_24h,
            "byAccount": Value::Object(by_acct_json),
        },
        "byPurpose": {
            "write": purpose_write,
            "review": purpose_review,
            "rework": purpose_rework,
            "readOnly": purpose_readonly,
        },
        "tokenBreakdown": {
            "input": tok_input,
            "output": tok_output,
            "cacheRead": tok_cache_read,
        },
        "byModel": by_model_json,
        "daily7d": daily_7d,
    });

    AnalyticsAndDeliveries {
        analytics,
        recent_deliveries,
        merged_today,
    }
}

/// Backward-compatible wrapper returning only the analytics JSON bundle.
#[allow(dead_code)]
pub fn tui_analytics_snapshot(state_root: &Path, all_seen_runs: &BTreeMap<String, Value>) -> Value {
    tui_analytics_and_deliveries(state_root, all_seen_runs, &[], epoch() as i64).analytics
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
    }

    #[test]
    fn accounts_usage_snapshot_aggregates_1h_5h_7d_and_quota_notes() {
        let now = 1_800_000_000i64;
        let make_entry = |epoch: i64, total_tokens: i64, live: bool| UsageEntry {
            epoch,
            run: format!("r-{epoch}"),
            repo: "/repo".into(),
            repo_name: "repo".into(),
            role: "author".into(),
            mode: "write".into(),
            rework: false,
            model: "agy/gemini-4-neon".into(),
            is_agy: true,
            account: Some("work".into()),
            agy_version: None,
            tokens: TokenCounts::new(total_tokens, 0, 0),
            live,
        };
        let mut accounts_map = Map::new();
        accounts_map.insert(
            "work".into(),
            json!({
                "quotaNotes": [
                    {"at": "2026-11-24T01:00:00Z", "epoch": now - 1800, "weekly": 95.0, "fiveHour": 80.0},
                    {"at": "2026-11-24T01:30:00Z", "epoch": now - 300, "weekly": 93.25}
                ]
            }),
        );
        let entries = vec![
            make_entry(now - 600, 2_000_000, true),          // 1h + 5h + 7d (live)
            make_entry(now - 3 * 3600, 4_000_000, false),    // 5h + 7d
            make_entry(now - 48 * 3600, 10_000_000, false),  // 7d only
        ];
        let entry_refs: Vec<&UsageEntry> = entries.iter().collect();
        let snap = accounts_usage_snapshot_from_entries(&accounts_map, now, &entry_refs, &[]);
        let view = snap.per_account.get("work").expect("work view");
        assert_eq!(view.usage["oneHour"]["total"], 2_000_000);
        assert_eq!(view.usage["oneHour"]["liveCounted"], 1);
        assert_eq!(view.usage["fiveHour"]["total"], 6_000_000);
        assert_eq!(view.usage["sevenDay"]["total"], 16_000_000);
        assert_eq!(view.quota_note["weekly"], 93.25);
        assert_eq!(view.quota_note["fiveHour"], 80.0);
        assert!(view.last_used_at.is_some());
    }

    #[test]
    fn hourly_24h_and_daily_7d_trend_compute_expected_metrics() {
        let now = iso_epoch("2026-11-24T12:00:00Z").unwrap();
        let e_recent = UsageEntry {
            epoch: now - 300, // current hour -> bucket 23
            run: "r1".into(),
            repo: "/repo".into(),
            repo_name: "repo".into(),
            role: "author".into(),
            mode: "write".into(),
            rework: false,
            model: "agy/gemini-4-neon".into(),
            is_agy: true,
            account: Some("alpha".into()),
            agy_version: None,
            tokens: TokenCounts::new(1000, 0, 0),
            live: false,
        };
        let e_prev = UsageEntry {
            epoch: now - 23 * 3600 - 10, // oldest hour -> bucket 0
            tokens: TokenCounts::new(2500, 0, 0),
            ..e_recent.clone()
        };
        let hourly = fill_hourly_24h(&[&e_recent, &e_prev], now);
        assert_eq!(hourly[23], 1000);
        assert_eq!(hourly[0], 2500);
        assert_eq!(hourly[12], 0);

        let mut recs = HashMap::new();
        // First-round pass merged in 600s today
        recs.insert(
            "run-a".to_string(),
            crate::history::RecentHistoryRecord {
                run: "run-a".into(),
                mode: "write".into(),
                state: "delivered".into(),
                parent: String::new(),
                started_epoch: now - 1200,
                finished_epoch: now - 800,
                applied_epoch: Some(now - 600),
                rework_count: 0,
                ..crate::history::RecentHistoryRecord::default()
            },
        );
        // Reworked chain merged in 1800s today (NOT first-round pass)
        recs.insert(
            "run-b-r1".to_string(),
            crate::history::RecentHistoryRecord {
                run: "run-b-r1".into(),
                mode: "write".into(),
                state: "rejected".into(),
                parent: String::new(),
                started_epoch: now - 2400,
                finished_epoch: now - 1800,
                applied_epoch: None,
                rework_count: 0,
                ..crate::history::RecentHistoryRecord::default()
            },
        );
        recs.insert(
            "run-b-r2".to_string(),
            crate::history::RecentHistoryRecord {
                run: "run-b-r2".into(),
                mode: "write".into(),
                state: "delivered".into(),
                parent: "run-b-r1".into(),
                started_epoch: now - 1700,
                finished_epoch: now - 900,
                applied_epoch: Some(now - 600),
                rework_count: 1,
                ..crate::history::RecentHistoryRecord::default()
            },
        );
        let (daily, deliveries, merged_today) =
            compute_daily_7d_and_deliveries(&recs, &[], now, now - SEVEN_DAYS_SECS);
        assert_eq!(daily.len(), 7);
        assert_eq!(deliveries.len(), 2);
        assert_eq!(merged_today, 2);
        let today = daily.last().unwrap();
        assert_eq!(today["mergedCount"], 2);
        assert_eq!(today["totalChains"], 2);
        assert_eq!(today["firstRoundPass"], 1);
        assert_eq!(today["firstRoundPassPct"], 50);
        assert_eq!(today["avgMergeSecs"], 1200);
    }

    #[test]
    fn load_trend_records_folds_finish_apply_and_clean_events() {
        let base = std::env::temp_dir().join(format!(
            "delegate-trend-fold-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let hdir = base.join("history/norvane-test");
        fs::create_dir_all(&hdir).unwrap();
        let hist = concat!(
            r#"{"event":"finish","run":"20261124-100000-feat-a","mode":"write","state":"delivered","startedAt":"2026-11-24T02:00:00Z","finishedAt":"2026-11-24T02:30:00Z","reworkCount":0}"#, "\n",
            r#"{"event":"apply","run":"20261124-100000-feat-a","appliedAt":"2026-11-24T03:00:00Z"}"#, "\n",
            r#"{"event":"clean","run":"20261124-100000-feat-a","cleanedAt":"2026-11-24T04:00:00Z"}"#, "\n"
        );
        fs::write(hdir.join("history.jsonl"), hist).unwrap();

        let now_ep = iso_epoch("2026-11-25T04:00:00Z").unwrap();
        let records = load_recent_history_records(
            &base,
            &BTreeMap::new(),
            now_ep - SEVEN_DAYS_SECS,
            now_ep,
        );
        let rec = records.get("20261124-100000-feat-a").expect("folded record");
        assert_eq!(rec.mode, "write");
        assert_eq!(rec.state, "delivered");
        assert!(rec.started_epoch > 0);
        assert!(rec.applied_epoch.is_some());

        let (daily, deliveries, _) =
            compute_daily_7d_and_deliveries(&records, &[], now_ep, now_ep - SEVEN_DAYS_SECS);
        let d24 = daily.iter().find(|d| s(d, "ymd") == "2026-11-24").unwrap();
        assert_eq!(d24["mergedCount"].as_u64(), Some(1));
        assert_eq!(d24["medianMergeSecs"].as_u64(), Some(3600));
        assert_eq!(d24["firstRoundPass"].as_u64(), Some(1));
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0]["cleaned"], true);

        let _ = fs::remove_dir_all(&base);
    }
}
