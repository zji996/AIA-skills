//! `delegate fit bench review` — replay historical write runs with known review outcomes
//! against a candidate reviewer alias using fail-closed archived snapshots.
use crate::common::*;
use crate::history::{self, RecentHistoryRecord};
use crate::launch;
use crate::review::{self, ParsedVerdict};
use crate::usage::TokenCounts;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const BENCH_USAGE: &str =
    "use delegate fit bench review <alias> [--since 30d] [-n 10] [--estimate] [--yes] [--json]";
const BENCH_VALID_FLAGS: [&str; 6] = ["--since", "-n", "--limit", "--estimate", "--yes", "--json"];
static BENCH_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroundTruth {
    Missed,
    Caught,
}

impl GroundTruth {
    fn as_str(self) -> &'static str {
        match self {
            Self::Missed => "missed",
            Self::Caught => "caught",
        }
    }

    fn expected_verdict(self) -> &'static str {
        match self {
            Self::Missed | Self::Caught => "REWORK",
        }
    }
}

#[derive(Debug, Clone)]
struct BenchCandidate {
    run: String,
    repo_label: String,
    source_root: String,
    record_dir: PathBuf,
    prior_meta: Value,
    review_doc: Value,
    ground_truth: GroundTruth,
    orig_verdict: String,
    sort_epoch: i64,
    orig_tokens: TokenCounts,
    orig_duration_secs: i64,
    has_orig_usage: bool,
}

struct BenchSnapshot {
    origin_repo: PathBuf,
    tree_sha: String,
    base_commit: Option<String>,
    diff_base: Option<String>,
    archive_dir: Option<PathBuf>,
    round_total_diff: Option<PathBuf>,
    from_rework_round: bool,
}

struct BenchArgs {
    alias: String,
    since_raw: String,
    limit: usize,
    estimate_only: bool,
    yes: bool,
    json_out: bool,
}

fn parse_args(args: &[String]) -> Res<BenchArgs> {
    let Some((class, rest)) = args.split_first() else {
        return Err(BENCH_USAGE.into());
    };
    if class != "review" {
        return Err(format!("unsupported bench class '{class}'; {BENCH_USAGE}"));
    }
    let mut alias: Option<String> = None;
    let mut since_raw = "30d".to_string();
    let mut limit: usize = 10;
    let mut estimate_only = false;
    let mut yes = false;
    let mut json_out = false;

    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        if a == "--since" {
            let Some(val) = rest.get(i + 1) else {
                return Err(format!("--since requires a value; {BENCH_USAGE}"));
            };
            since_raw = val.clone();
            i += 2;
        } else if let Some(val) = a.strip_prefix("--since=") {
            since_raw = val.to_string();
            i += 1;
        } else if a == "-n" || a == "--limit" {
            let Some(val) = rest.get(i + 1) else {
                return Err(format!("{a} requires a positive integer; {BENCH_USAGE}"));
            };
            limit = val
                .parse::<usize>()
                .ok()
                .filter(|&v| v > 0)
                .ok_or_else(|| format!("{a} must be a positive integer, got {val:?}"))?;
            i += 2;
        } else if let Some(val) = a.strip_prefix("-n=").or_else(|| a.strip_prefix("--limit=")) {
            limit = val
                .parse::<usize>()
                .ok()
                .filter(|&v| v > 0)
                .ok_or_else(|| format!("limit must be a positive integer, got {val:?}"))?;
            i += 1;
        } else if a == "--estimate" {
            estimate_only = true;
            i += 1;
        } else if a == "--yes" || a == "-y" {
            yes = true;
            i += 1;
        } else if a == "--json" {
            json_out = true;
            i += 1;
        } else if a.starts_with('-') {
            return Err(unrecognized_arg(a, &BENCH_VALID_FLAGS, BENCH_USAGE));
        } else if alias.is_none() {
            alias = Some(a.to_string());
            i += 1;
        } else {
            return Err(format!("unexpected argument '{a}'; {BENCH_USAGE}"));
        }
    }

    let alias = alias.ok_or_else(|| format!("missing model alias; {BENCH_USAGE}"))?;
    Ok(BenchArgs {
        alias,
        since_raw,
        limit,
        estimate_only,
        yes,
        json_out,
    })
}

fn locate_record_dir(rec: &RecentHistoryRecord) -> Option<PathBuf> {
    if !rec.archive.is_empty() {
        let p = PathBuf::from(&rec.archive).join("record");
        if p.join("meta.json").is_file() {
            return Some(p);
        }
    }
    let cached = cache_dir().join("archive").join(&rec.run).join("record");
    if cached.join("meta.json").is_file() {
        return Some(cached);
    }
    if !rec.source_root.is_empty() {
        let live = PathBuf::from(&rec.source_root)
            .join(".local/run/delegate")
            .join(&rec.run);
        if live.join("meta.json").is_file() {
            return Some(live);
        }
    }
    let from_env = runs_root().join(&rec.run);
    if from_env.join("meta.json").is_file() {
        return Some(from_env);
    }
    None
}

fn read_single_round_usage_file(dir: &Path, round_num: Option<u64>) -> (TokenCounts, bool) {
    let review_dir = dir.join("review");
    if let Some(rn) = round_num {
        let ufile = review_dir.join(format!("round-{rn}")).join("usage.json");
        if ufile.is_file() {
            let u = json(&ufile);
            if u["tokens"].is_object() {
                return (TokenCounts::from_json(&u["tokens"]), true);
            }
        }
    }
    let Ok(entries) = fs::read_dir(&review_dir) else {
        return (TokenCounts::default(), false);
    };
    let mut total = TokenCounts::default();
    let mut count = 0i64;
    for ent in entries.flatten() {
        let ufile = ent.path().join("usage.json");
        if !ufile.is_file() {
            continue;
        }
        let u = json(&ufile);
        if u["tokens"].is_object() {
            total.add(TokenCounts::from_json(&u["tokens"]));
            count += 1;
        }
    }
    if count > 0 {
        (
            TokenCounts::new(
                total.input / count,
                total.output / count,
                total.cache_read / count,
            ),
            true,
        )
    } else {
        (TokenCounts::default(), false)
    }
}

fn target_round_entry(review_doc: &Value, gt: GroundTruth) -> Option<&Value> {
    let rounds = review_doc["rounds"].as_array()?;
    match gt {
        GroundTruth::Caught => rounds
            .iter()
            .find(|r| s(r, "verdict").eq_ignore_ascii_case("REWORK"))
            .or_else(|| rounds.first()),
        GroundTruth::Missed => rounds.last(),
    }
}

fn extract_original_review_usage(
    review_doc: &Value,
    gt: GroundTruth,
    record_dir: &Path,
    archive_dir: Option<&Path>,
) -> (TokenCounts, i64, bool) {
    let target_round = target_round_entry(review_doc, gt);
    let target_round_num = target_round.and_then(|r| r["round"].as_u64());
    let round_count = review_doc["rounds"]
        .as_array()
        .map(|a| a.len().max(1) as i64)
        .unwrap_or(1);

    let mut tokens = TokenCounts::default();
    let mut has_usage = false;

    if let Some(r) = target_round {
        if r["tokens"].is_object() {
            tokens = TokenCounts::from_json(&r["tokens"]);
            has_usage = true;
        }
    }
    if !has_usage && review_doc["tokens"].is_object() {
        let tot = TokenCounts::from_json(&review_doc["tokens"]);
        tokens = TokenCounts::new(
            tot.input / round_count,
            tot.output / round_count,
            tot.cache_read / round_count,
        );
        has_usage = !tot.is_zero() || review_doc["tokens"].is_object();
    }
    if !has_usage {
        let (t, ok) = read_single_round_usage_file(record_dir, target_round_num);
        if ok {
            tokens = t;
            has_usage = true;
        } else if let Some(arch) = archive_dir {
            let (t2, ok2) = read_single_round_usage_file(&arch.join("record"), target_round_num);
            if ok2 {
                tokens = t2;
                has_usage = true;
            }
        }
    }

    // Prefer the target single review round's duration so multi-round auto-rework runs do not
    // include author rework or slow acceptance wait time in the single-round estimate.
    let duration_secs = target_round
        .and_then(|r| {
            let st = iso_epoch(s(r, "startedAt"))?;
            let fin = iso_epoch(s(r, "finishedAt"))?;
            Some((fin - st).max(0))
        })
        .or_else(|| {
            let rounds = review_doc["rounds"].as_array()?;
            let mut sum = 0i64;
            let mut cnt = 0i64;
            for r in rounds {
                if let (Some(st), Some(fin)) =
                    (iso_epoch(s(r, "startedAt")), iso_epoch(s(r, "finishedAt")))
                {
                    sum += (fin - st).max(0);
                    cnt += 1;
                }
            }
            (cnt > 0).then_some(sum / cnt)
        })
        .or_else(|| {
            let st = iso_epoch(s(review_doc, "startedAt"))?;
            let fin = iso_epoch(s(review_doc, "finishedAt"))?;
            Some((fin - st).max(0) / round_count)
        })
        .unwrap_or(0);

    (tokens, duration_secs, has_usage)
}

fn classify_ground_truth(
    rec: &RecentHistoryRecord,
    review_doc: &Value,
    child_write_parents: &HashSet<String>,
) -> Option<(GroundTruth, String)> {
    if !review_doc.is_object() {
        return None;
    }
    if b(review_doc, "syncSkip")
        || s(review_doc, "scope") == "sync_skip"
        || matches!(s(review_doc, "status"), "env_error" | "cancelled")
    {
        return None;
    }
    let final_verdict = s(review_doc, "verdict").to_ascii_uppercase();
    if final_verdict.is_empty() {
        return None;
    }
    let had_rework_round = review_doc["rounds"].as_array().is_some_and(|rs| {
        rs.iter()
            .any(|r| s(r, "verdict").eq_ignore_ascii_case("REWORK"))
    }) || n(review_doc, "reworkRounds") > 0;
    let has_child_reply = child_write_parents.contains(&rec.run);
    let controller_reworked = has_child_reply || (rec.parent.is_empty() && rec.rework_count > 0);
    let adopted = rec.applied_epoch.is_some() || rec.state == "delivered" || controller_reworked;

    if final_verdict == "PASS" && controller_reworked {
        Some((GroundTruth::Missed, final_verdict))
    } else if (final_verdict == "REWORK" || had_rework_round) && adopted {
        let orig = if final_verdict == "REWORK" {
            final_verdict
        } else {
            "REWORK".to_string()
        };
        Some((GroundTruth::Caught, orig))
    } else {
        None
    }
}

fn order_recent_diverse(mut items: Vec<BenchCandidate>) -> Vec<BenchCandidate> {
    items.sort_by(|a, b| {
        b.sort_epoch
            .cmp(&a.sort_epoch)
            .then_with(|| b.run.cmp(&a.run))
    });
    let mut by_repo: BTreeMap<String, Vec<BenchCandidate>> = BTreeMap::new();
    for item in items {
        by_repo
            .entry(item.repo_label.clone())
            .or_default()
            .push(item);
    }
    let mut repo_order: Vec<(i64, String)> = by_repo
        .iter()
        .map(|(repo, list)| {
            let top_ep = list.first().map(|c| c.sort_epoch).unwrap_or(0);
            (top_ep, repo.clone())
        })
        .collect();
    repo_order.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let mut queues: Vec<std::collections::VecDeque<BenchCandidate>> = repo_order
        .into_iter()
        .filter_map(|(_, repo)| by_repo.remove(&repo).map(std::collections::VecDeque::from))
        .collect();

    let mut out = Vec::new();
    loop {
        let mut progressed = false;
        for q in &mut queues {
            if let Some(item) = q.pop_front() {
                out.push(item);
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    out
}

fn select_candidates(
    all_records: &HashMap<String, RecentHistoryRecord>,
    cutoff_epoch: i64,
    limit: usize,
) -> Vec<BenchCandidate> {
    let child_write_parents: HashSet<String> = all_records
        .values()
        .filter(|r| !r.parent.is_empty() && r.mode != "read-only")
        .map(|r| r.parent.clone())
        .collect();

    let mut missed_pool = Vec::new();
    let mut caught_pool = Vec::new();

    for rec in all_records.values() {
        if rec.mode == "read-only" {
            continue;
        }
        let sort_epoch = rec
            .applied_epoch
            .unwrap_or(0)
            .max(rec.finished_epoch)
            .max(rec.started_epoch);
        if cutoff_epoch > 0 && sort_epoch < cutoff_epoch {
            continue;
        }
        let Some(record_dir) = locate_record_dir(rec) else {
            continue;
        };
        let prior_meta = json(record_dir.join("meta.json"));
        if !prior_meta.is_object() {
            continue;
        }
        let archive_dir = launch::resolve_upstream_archive_dir(&record_dir, &prior_meta);
        let Some(review_path) =
            launch::find_upstream_record_file(&record_dir, archive_dir.as_deref(), "review.json")
        else {
            continue;
        };
        let review_doc = json(&review_path);
        let Some((gt, orig_verdict)) =
            classify_ground_truth(rec, &review_doc, &child_write_parents)
        else {
            continue;
        };
        let (orig_tokens, orig_duration_secs, has_orig_usage) =
            extract_original_review_usage(&review_doc, gt, &record_dir, archive_dir.as_deref());
        let cand = BenchCandidate {
            run: rec.run.clone(),
            repo_label: if rec.repo_label.is_empty() {
                "unknown".to_string()
            } else {
                rec.repo_label.clone()
            },
            source_root: rec.source_root.clone(),
            record_dir,
            prior_meta,
            review_doc,
            ground_truth: gt,
            orig_verdict,
            sort_epoch,
            orig_tokens,
            orig_duration_secs,
            has_orig_usage,
        };
        match gt {
            GroundTruth::Missed => missed_pool.push(cand),
            GroundTruth::Caught => caught_pool.push(cand),
        }
    }

    let missed_ordered = order_recent_diverse(missed_pool);
    let caught_ordered = order_recent_diverse(caught_pool);

    let target_missed = limit.div_ceil(2);
    let target_caught = limit / 2;
    let take_missed = missed_ordered.len().min(target_missed);
    let take_caught = caught_ordered.len().min(target_caught);
    let rem = limit.saturating_sub(take_missed + take_caught);
    let extra_missed = (missed_ordered.len() - take_missed).min(rem);
    let rem = rem.saturating_sub(extra_missed);
    let extra_caught = (caught_ordered.len() - take_caught).min(rem);

    let mut selected: Vec<BenchCandidate> = missed_ordered
        .into_iter()
        .take(take_missed + extra_missed)
        .chain(caught_ordered.into_iter().take(take_caught + extra_caught))
        .collect();
    selected.sort_by(|a, b| {
        b.sort_epoch
            .cmp(&a.sort_epoch)
            .then_with(|| b.run.cmp(&a.run))
    });
    selected
}

fn is_overwritten_by_auto_rework(cand: &BenchCandidate) -> bool {
    if cand.ground_truth != GroundTruth::Caught {
        return false;
    }
    let final_verdict = s(&cand.review_doc, "verdict").to_ascii_uppercase();
    let had_rework_round = cand.review_doc["rounds"].as_array().is_some_and(|rs| {
        rs.iter()
            .any(|r| s(r, "verdict").eq_ignore_ascii_case("REWORK"))
    }) || n(&cand.review_doc, "reworkRounds") > 0;
    had_rework_round && (final_verdict != "REWORK" || b(&cand.review_doc, "stoppedDuringRework"))
}

fn reconstruct_pre_rework_caught_snapshot(
    cand: &BenchCandidate,
    fallback_workdir: &Path,
) -> Res<BenchSnapshot> {
    let origin_repo = {
        let src = s(&cand.prior_meta["worktree"], "source");
        if !src.is_empty() && Path::new(src).is_dir() && git_top_fast(Path::new(src)).is_some() {
            git_top_fast(Path::new(src)).unwrap_or_else(|| PathBuf::from(src))
        } else if let Some(top) = git_top_fast(fallback_workdir) {
            top
        } else {
            return Err(format!("{}: 找不到源 git 仓库目录", cand.run));
        }
    };
    let origin_git_dir = git_text(
        &origin_repo,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    )
    .map(PathBuf::from)
    .map_err(|e| format!("{}: 源仓库 git-dir 不可读: {e}", cand.run))?;

    let archive_dir = launch::resolve_upstream_archive_dir(&cand.record_dir, &cand.prior_meta);
    let rework_round = cand.review_doc["rounds"]
        .as_array()
        .and_then(|rs| {
            rs.iter()
                .find(|r| s(r, "verdict").eq_ignore_ascii_case("REWORK"))
        });
    let round_num = rework_round
        .and_then(|r| r["round"].as_u64())
        .unwrap_or(1);
    let round_tree = rework_round
        .map(|r| s(r, "reviewedTree").to_string())
        .unwrap_or_default();
    let round_base = rework_round
        .map(|r| s(r, "reviewedBaseTree").to_string())
        .unwrap_or_default();

    let round_total_diff = launch::find_upstream_record_file(
        &cand.record_dir,
        archive_dir.as_deref(),
        &format!("review/round-{round_num}/total.diff"),
    );

    let wt_base_commit = launch::find_upstream_record_file(
        &cand.record_dir,
        archive_dir.as_deref(),
        "worktree-base-commit",
    )
    .map(|p| read(&p).trim().to_string())
    .unwrap_or_default();

    let raw_bases: Vec<String> = vec![
        round_base.clone(),
        s(&cand.prior_meta["base"], "tree").to_string(),
        s(&cand.prior_meta, "chainBase").to_string(),
        s(&cand.prior_meta, "netBase").to_string(),
        wt_base_commit,
        s(&cand.prior_meta["base"], "head").to_string(),
    ];
    let mut seen_bases = HashSet::new();
    let mut valid_bases: Vec<(String, String)> = Vec::new();
    let mut first_base_commit: Option<String> = None;
    for sha in raw_bases {
        if sha.is_empty() || !seen_bases.insert(sha.clone()) {
            continue;
        }
        if let Some(ty) = launch::git_object_type(&origin_repo, &sha) {
            if matches!(ty.as_str(), "tree" | "commit") {
                if ty == "commit" && first_base_commit.is_none() {
                    first_base_commit = Some(sha.clone());
                }
                valid_bases.push((sha, ty));
            }
        }
    }

    // 1. Prefer the REWORK round's recorded `reviewedTree` directly if still reachable in git.
    if !round_tree.is_empty()
        && launch::git_object_type(&origin_repo, &round_tree).as_deref() == Some("tree")
    {
        let diff_base = if !round_base.is_empty()
            && launch::git_object_type(&origin_repo, &round_base).is_some()
        {
            Some(round_base)
        } else {
            valid_bases.first().map(|(sha, _)| sha.clone())
        };
        return Ok(BenchSnapshot {
            origin_repo,
            tree_sha: round_tree,
            base_commit: first_base_commit,
            diff_base,
            archive_dir,
            round_total_diff,
            from_rework_round: true,
        });
    }

    // 2. Fall back to applying archived/local `review/round-<k>/total.diff` onto base.
    if let Some(ref diff_path) = round_total_diff {
        let patch_bytes = fs::read(diff_path)
            .map_err(|e| format!("{}: 读取 round-{round_num}/total.diff 失败: {e}", cand.run))?;
        let mut last_err = String::new();
        for (base_sha, obj_type) in &valid_bases {
            match launch::apply_patch_onto_base(&origin_git_dir, base_sha, &patch_bytes) {
                Ok(tree_sha) => {
                    let base_commit = if obj_type == "commit" {
                        Some(base_sha.clone())
                    } else {
                        first_base_commit.clone()
                    };
                    return Ok(BenchSnapshot {
                        origin_repo,
                        tree_sha,
                        base_commit,
                        diff_base: Some(base_sha.clone()),
                        archive_dir,
                        round_total_diff: Some(diff_path.clone()),
                        from_rework_round: true,
                    });
                }
                Err(e) => {
                    last_err = format!("{}({}): {e}", obj_type, &base_sha[..base_sha.len().min(7)]);
                }
            }
        }
        return Err(format!(
            "{}: 该检出样本已被后续自动返工覆盖，round-{round_num}/total.diff 无法应用到基线（{last_err}），已失败关闭跳过",
            cand.run
        ));
    }

    // 3. Fail-closed: never fall back to post-rework `applied.patch` / `changes.patch`.
    Err(format!(
        "{}: 该检出样本已被后续自动返工覆盖，且归档缺少 round-{round_num} 的 reviewedTree 与 total.diff，已失败关闭跳过",
        cand.run
    ))
}

fn reconstruct_bench_snapshot(
    cand: &BenchCandidate,
    fallback_workdir: &Path,
) -> Res<BenchSnapshot> {
    if is_overwritten_by_auto_rework(cand) {
        return reconstruct_pre_rework_caught_snapshot(cand, fallback_workdir);
    }
    let snap = launch::reconstruct_in_snapshot(&cand.record_dir, &cand.prior_meta, fallback_workdir)?;
    Ok(BenchSnapshot {
        origin_repo: snap.origin_repo,
        tree_sha: snap.tree_sha,
        diff_base: snap.base_commit.clone(),
        base_commit: snap.base_commit,
        archive_dir: snap.archive_dir,
        round_total_diff: None,
        from_rework_round: false,
    })
}

struct TempBenchWorkspace {
    tmp_root: PathBuf,
    wt_dir: PathBuf,
    exec_dir: PathBuf,
    origin_repo: PathBuf,
    worktree_added: bool,
}

impl TempBenchWorkspace {
    fn create(snap: &BenchSnapshot) -> Res<Self> {
        let seq = BENCH_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_root = std::env::temp_dir().join(format!(
            "delegate-fit-bench-{}-{}-{}",
            std::process::id(),
            (epoch() * 1000.0) as u64,
            seq
        ));
        let wt_dir = tmp_root.join("wt");
        let exec_dir = tmp_root.join("run").join("review").join("round-1");
        fs::create_dir_all(&exec_dir).map_err(|e| format!("create bench temp dir: {e}"))?;

        let mut c = git_command();
        c.arg("-C")
            .arg(&snap.origin_repo)
            .arg("commit-tree")
            .arg(&snap.tree_sha);
        if let Some(base) = &snap.base_commit {
            if !base.is_empty() {
                c.args(["-p", base]);
            }
        }
        c.arg("-m").arg("delegate: fit bench read-only snapshot");
        for who in ["AUTHOR", "COMMITTER"] {
            c.env(format!("GIT_{who}_NAME"), "delegate")
                .env(format!("GIT_{who}_EMAIL"), "delegate@localhost");
        }
        let out = c.output().map_err(|e| format!("git commit-tree: {e}"))?;
        if !out.status.success() {
            let _ = fs::remove_dir_all(&tmp_root);
            return Err(format!(
                "commit-tree 失败: {}",
                clip(String::from_utf8_lossy(&out.stderr).trim(), 240)
            ));
        }
        let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();

        if let Err(e) = git(
            &snap.origin_repo,
            &[
                "worktree",
                "add",
                "--detach",
                &wt_dir.to_string_lossy(),
                &commit,
            ],
        ) {
            let _ = fs::remove_dir_all(&tmp_root);
            return Err(format!("worktree add 失败: {}", clip(&e, 240)));
        }
        if let Some(base) = &snap.base_commit {
            if !base.is_empty() {
                let _ = git(&wt_dir, &["reset", "-q", "--soft", base]);
            }
        }
        Ok(Self {
            tmp_root,
            wt_dir,
            exec_dir,
            origin_repo: snap.origin_repo.clone(),
            worktree_added: true,
        })
    }
}

impl Drop for TempBenchWorkspace {
    fn drop(&mut self) {
        if self.worktree_added {
            let _ = git(
                &self.origin_repo,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    &self.wt_dir.to_string_lossy(),
                ],
            );
        }
        let _ = fs::remove_dir_all(&self.tmp_root);
    }
}

fn build_diff_text(cand: &BenchCandidate, snap: &BenchSnapshot) -> String {
    if let Some(base) = snap.diff_base.as_deref().or(snap.base_commit.as_deref()) {
        if !base.is_empty() {
            if let Ok(text) = git_text(
                &snap.origin_repo,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--color=never",
                    "-M",
                    "-C",
                    "-C",
                    base,
                    &snap.tree_sha,
                ],
            ) {
                if !text.trim().is_empty() {
                    return text;
                }
            }
        }
    }
    if snap.from_rework_round {
        if let Some(ref p) = snap.round_total_diff {
            let text = read(p);
            if !text.trim().is_empty() {
                return text;
            }
        }
        return String::new();
    }
    for name in ["applied.patch", "changes.patch"] {
        if let Some(p) =
            launch::find_upstream_record_file(&cand.record_dir, snap.archive_dir.as_deref(), name)
        {
            let text = read(&p);
            if !text.trim().is_empty() {
                return text;
            }
        }
    }
    String::new()
}

fn load_rubric(cand: &BenchCandidate, origin_repo: &Path) -> (String, String) {
    let raw = s(&cand.review_doc, "rubric").trim().to_string();
    if !raw.is_empty() && raw != "默认 rubric" && raw != "default" {
        let p = PathBuf::from(&raw);
        let full = if p.is_absolute() {
            p
        } else {
            origin_repo.join(&p)
        };
        if full.is_file() {
            let content = read(&full);
            if !content.trim().is_empty() {
                return (raw, content);
            }
        }
    }
    ("默认 rubric".to_string(), review::DEFAULT_RUBRIC.to_string())
}

fn build_review_prompt(
    cand: &BenchCandidate,
    snap: &BenchSnapshot,
    wt_dir: &Path,
    diff_file: &Path,
    diff_text: &str,
) -> String {
    let task_brief = {
        let extracted = history::extract_user_prompt(&cand.record_dir, &cand.prior_meta);
        if !extracted.trim().is_empty() {
            extracted
        } else if let Some(p) = launch::find_upstream_record_file(
            &cand.record_dir,
            snap.archive_dir.as_deref(),
            "prompt.md",
        ) {
            read(&p)
        } else {
            s(&cand.prior_meta, "name").to_string()
        }
    };
    let (rubric_source, rubric_text) = load_rubric(cand, &snap.origin_repo);
    let from_tree = snap
        .diff_base
        .as_deref()
        .or(snap.base_commit.as_deref())
        .unwrap_or("");
    review::build_full_review_prompt(
        true,
        wt_dir,
        diff_file,
        diff_text,
        &snap.origin_repo,
        from_tree,
        &snap.tree_sha,
        &rubric_source,
        &rubric_text,
        &task_brief,
    )
}

fn read_exec_usage(exec_dir: &Path) -> TokenCounts {
    for name in ["usage.json", "live-usage.json"] {
        let p = exec_dir.join(name);
        if p.is_file() {
            let v = json(&p);
            if v["tokens"].is_object() {
                return TokenCounts::from_json(&v["tokens"]);
            }
        }
    }
    TokenCounts::default()
}

fn format_fit_note(
    valid: usize,
    skipped: usize,
    errors: usize,
    hits: usize,
    missed: usize,
    false_positives: usize,
) -> String {
    let pct = if valid > 0 {
        ((hits as f64 / valid as f64) * 100.0).round() as u64
    } else {
        0
    };
    let meta_tag = match (skipped > 0, errors > 0) {
        (true, true) => format!("(跳{skipped}异{errors})"),
        (true, false) => format!("(跳{skipped})"),
        (false, true) => format!("(异{errors})"),
        (false, false) => String::new(),
    };
    let raw = format!(
        "bench {valid}条{meta_tag}：命中{pct}%({hits}/{valid})，漏检{missed}，误报{false_positives}"
    );
    if raw.chars().count() > 80 {
        raw.chars().take(80).collect()
    } else {
        raw
    }
}

fn pick_fit_verdict(
    existing_verdict: &str,
    hits: usize,
    valid: usize,
    false_positives: usize,
) -> &'static str {
    let hit_rate = if valid > 0 {
        hits as f64 / valid as f64
    } else {
        0.0
    };
    if hit_rate >= 0.75 && false_positives == 0 {
        if existing_verdict == "first" {
            "first"
        } else {
            "ok"
        }
    } else if hit_rate >= 0.5 {
        "ok"
    } else {
        "avoid"
    }
}

pub fn command(args: &[String], config: &Value) -> Res<i32> {
    let parsed = parse_args(args)?;
    let aliases = crate::fit::aliases(config);
    if !aliases.contains(&parsed.alias) {
        return Err(format!("unknown alias {}; see delegate tiers", parsed.alias));
    }

    let (since_label, since_secs) = crate::fill::parse_since_arg(&parsed.since_raw)?;
    let cutoff_epoch = since_secs
        .map(|secs| (epoch() as i64).saturating_sub(secs))
        .unwrap_or(0);

    let resolved_model = crate::models::resolve_model(&parsed.alias, config, &[])?;
    let thinking = crate::models::thinking(None, &json!(parsed.alias), config)?
        .or_else(|| crate::fit::default_thinking("review").map(str::to_string));

    let all_records = history::read_recent_window_all_repos(&state_dir(), cutoff_epoch);
    let selected = select_candidates(&all_records, cutoff_epoch, parsed.limit);

    let selected_missed = selected
        .iter()
        .filter(|c| c.ground_truth == GroundTruth::Missed)
        .count();
    let selected_caught = selected
        .iter()
        .filter(|c| c.ground_truth == GroundTruth::Caught)
        .count();
    let repos: Vec<String> = selected
        .iter()
        .map(|c| c.repo_label.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let mut est_tokens = TokenCounts::default();
    let mut est_duration_secs: i64 = 0;
    let mut runs_with_usage: usize = 0;
    let mut runs_without_usage: usize = 0;
    for c in &selected {
        if c.has_orig_usage {
            est_tokens.add(c.orig_tokens);
            runs_with_usage += 1;
        } else {
            runs_without_usage += 1;
        }
        est_duration_secs = est_duration_secs.saturating_add(c.orig_duration_secs.max(0));
    }

    let estimate_json = json!({
        "tokens": est_tokens.to_json_with_total(),
        "durationSeconds": est_duration_secs,
        "runsWithUsage": runs_with_usage,
        "runsWithoutUsage": runs_without_usage,
    });

    if selected.is_empty() {
        if parsed.json_out {
            println!(
                "{}",
                json!({
                    "alias": parsed.alias,
                    "model": resolved_model,
                    "since": since_label,
                    "limit": parsed.limit,
                    "estimateOnly": parsed.estimate_only,
                    "dispatched": false,
                    "requiresYes": false,
                    "selectedCount": 0,
                    "selectedMissed": 0,
                    "selectedCaught": 0,
                    "evaluated": 0,
                    "skipped": 0,
                    "hits": 0,
                    "hitRate": Value::Null,
                    "missed": 0,
                    "falsePositives": 0,
                    "errors": 0,
                    "estimate": estimate_json,
                    "fitRecorded": false,
                    "runs": [],
                })
            );
        } else {
            println!(
                "窗口 {since_label} 内没有符合条件的历史审查样本（需已有 review.json 且有确定结论的写入任务）。"
            );
        }
        return Ok(0);
    }

    if parsed.estimate_only || !parsed.yes {
        let requires_yes = !parsed.estimate_only && !parsed.yes;
        if parsed.json_out {
            let runs_json: Vec<Value> = selected
                .iter()
                .map(|c| {
                    json!({
                        "run": c.run,
                        "repo": c.repo_label,
                        "groundTruth": c.ground_truth.as_str(),
                        "expected": c.ground_truth.expected_verdict(),
                        "origVerdict": c.orig_verdict,
                        "origTokens": c.orig_tokens.to_json_with_total(),
                        "origDurationSeconds": c.orig_duration_secs,
                    })
                })
                .collect();
            println!(
                "{}",
                json!({
                    "alias": parsed.alias,
                    "model": resolved_model,
                    "since": since_label,
                    "limit": parsed.limit,
                    "estimateOnly": parsed.estimate_only,
                    "dispatched": false,
                    "requiresYes": requires_yes,
                    "selectedCount": selected.len(),
                    "selectedMissed": selected_missed,
                    "selectedCaught": selected_caught,
                    "repos": repos,
                    "estimate": estimate_json,
                    "fitRecorded": false,
                    "runs": runs_json,
                })
            );
        } else {
            println!(
                "fit bench review {}（模型 {}）",
                parsed.alias, resolved_model
            );
            println!(
                "样本：{} 条（漏检 {} / 检出 {}，窗口 {}，覆盖 {} 个仓库）",
                selected.len(),
                selected_missed,
                selected_caught,
                since_label,
                repos.len()
            );
            println!(
                "预计用量：~{} tokens（input {} / output {} / cacheRead {}），预计时长 ~{}s（基于 {}/{} 条原审查记录）",
                est_tokens.total(),
                est_tokens.input,
                est_tokens.output,
                est_tokens.cache_read,
                est_duration_secs,
                runs_with_usage,
                selected.len()
            );
            if parsed.estimate_only {
                println!("仅估算（--estimate），未派发任何模型调用。");
            } else {
                println!(
                    "未传 --yes，未派发模型调用；确认执行请加 --yes：delegate fit bench review {} --since {} -n {} --yes",
                    parsed.alias, since_label, parsed.limit
                );
            }
        }
        return Ok(if requires_yes { 2 } else { 0 });
    }

    // Confirmed execution (`--yes`): resolve agent binary and replay each selected run.
    let colleague = crate::agy::colleague(&resolved_model);
    let (agent_bin, agent_version) = crate::agents::agent_identity(colleague)?;
    let review_cfg = json!({
        "tier": "review",
        "model": resolved_model,
        "modelName": parsed.alias,
        "thinking": thinking,
        "agent": colleague,
        "agentBin": agent_bin,
        "agentVersion": agent_version,
    });

    if !parsed.json_out {
        println!(
            "fit bench review {}（模型 {}）",
            parsed.alias, resolved_model
        );
        println!(
            "样本：{} 条（漏检 {} / 检出 {}，窗口 {}，覆盖 {} 个仓库）",
            selected.len(),
            selected_missed,
            selected_caught,
            since_label,
            repos.len()
        );
        println!(
            "预计用量：~{} tokens，预计时长 ~{}s；开始只读重审…",
            est_tokens.total(),
            est_duration_secs
        );
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut skipped: usize = 0;
    let mut hits: usize = 0;
    let mut missed: usize = 0;
    let mut false_positives: usize = 0;
    let mut errors: usize = 0;
    let mut total_usage = TokenCounts::default();
    let mut total_elapsed_secs: f64 = 0.0;
    let mut run_results: Vec<Value> = Vec::new();

    for cand in &selected {
        let fallback_workdir = if !cand.source_root.is_empty() {
            PathBuf::from(&cand.source_root)
        } else {
            cwd.clone()
        };
        let snap = match reconstruct_bench_snapshot(cand, &fallback_workdir) {
            Ok(s) => s,
            Err(reason) => {
                skipped += 1;
                let short_reason = clip(reason.lines().next().unwrap_or(&reason), 400);
                if !parsed.json_out {
                    println!(
                        "- {} [{}] 跳过（{}）",
                        cand.run,
                        cand.ground_truth.as_str(),
                        short_reason
                    );
                }
                run_results.push(json!({
                    "run": cand.run,
                    "repo": cand.repo_label,
                    "groundTruth": cand.ground_truth.as_str(),
                    "expected": cand.ground_truth.expected_verdict(),
                    "status": "skipped",
                    "skipReason": short_reason,
                }));
                continue;
            }
        };

        let ws = match TempBenchWorkspace::create(&snap) {
            Ok(w) => w,
            Err(reason) => {
                skipped += 1;
                let short_reason = clip(&reason, 400);
                if !parsed.json_out {
                    println!(
                        "- {} [{}] 跳过（{}）",
                        cand.run,
                        cand.ground_truth.as_str(),
                        short_reason
                    );
                }
                run_results.push(json!({
                    "run": cand.run,
                    "repo": cand.repo_label,
                    "groundTruth": cand.ground_truth.as_str(),
                    "expected": cand.ground_truth.expected_verdict(),
                    "status": "skipped",
                    "skipReason": short_reason,
                }));
                continue;
            }
        };

        let diff_text = build_diff_text(cand, &snap);
        let diff_file = ws.exec_dir.join("diff.patch");
        let _ = write(&diff_file, &diff_text);

        let prompt_body = build_review_prompt(cand, &snap, &ws.wt_dir, &diff_file, &diff_text);
        let prompt_file = ws.exec_dir.join("prompt.md");
        if let Err(e) = write(&prompt_file, &prompt_body) {
            skipped += 1;
            let short_reason = clip(&e, 240);
            run_results.push(json!({
                "run": cand.run,
                "repo": cand.repo_label,
                "groundTruth": cand.ground_truth.as_str(),
                "expected": cand.ground_truth.expected_verdict(),
                "status": "skipped",
                "skipReason": short_reason,
            }));
            continue;
        }

        let mut author_meta = cand.prior_meta.clone();
        if let Some(obj) = author_meta.as_object_mut() {
            // Redirect ownerRunDir to our temp exec_dir so bench never mutates historical runs.
            obj.insert("dir".into(), json!(ws.exec_dir));
            obj.insert(
                "worktree".into(),
                json!({
                    "source": snap.origin_repo,
                    "path": ws.wt_dir,
                }),
            );
        }
        let mut review_meta = review::build_reviewer_meta(
            &author_meta,
            &review_cfg,
            &ws.exec_dir,
            &ws.wt_dir,
            &prompt_file,
            None,
        );
        review_meta["freshAgySession"] = json!(true);
        if let Err(e) = crate::deny::prepare(&review_meta, &ws.exec_dir) {
            skipped += 1;
            let short_reason = clip(&e, 240);
            run_results.push(json!({
                "run": cand.run,
                "repo": cand.repo_label,
                "groundTruth": cand.ground_truth.as_str(),
                "expected": cand.ground_truth.expected_verdict(),
                "status": "skipped",
                "skipReason": short_reason,
            }));
            continue;
        }

        let started = Instant::now();
        let holder = Arc::new(AtomicI32::new(0));
        let grace = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let attempt = review::run_reviewer_with_loop_stop_retry(
            &review_meta,
            &ws.exec_dir,
            holder,
            grace,
            cancel,
        );
        let elapsed_secs = started.elapsed().as_secs_f64();
        let tok = read_exec_usage(&ws.exec_dir);
        total_usage.add(tok);
        total_elapsed_secs += elapsed_secs;

        let expected = cand.ground_truth.expected_verdict();
        let (cand_verdict, outcome_label, err_detail) = match attempt.res {
            Ok((outcome, text)) if outcome == "ok" => match review::parse_verdict(&text) {
                ParsedVerdict::Pass => ("PASS".to_string(), None, None),
                ParsedVerdict::Rework => ("REWORK".to_string(), None, None),
                ParsedVerdict::NeedsController => ("NEEDS_CONTROLLER".to_string(), None, None),
                ParsedVerdict::Invalid(sample) => (
                    "INVALID".to_string(),
                    Some("error"),
                    Some(format!("unparsed verdict: {sample}")),
                ),
            },
            Ok((outcome, _)) => (
                "ERROR".to_string(),
                Some("error"),
                Some(format!("agent outcome: {outcome}")),
            ),
            Err(e) => ("ERROR".to_string(), Some("error"), Some(clip(&e, 160))),
        };

        let outcome = if let Some(forced) = outcome_label {
            errors += 1;
            forced
        } else if expected == "REWORK" {
            if matches!(cand_verdict.as_str(), "REWORK" | "NEEDS_CONTROLLER") {
                hits += 1;
                "hit"
            } else {
                missed += 1;
                "missed"
            }
        } else if cand_verdict == "PASS" {
            hits += 1;
            "hit"
        } else {
            false_positives += 1;
            "false_positive"
        };

        if !parsed.json_out {
            let zh_outcome = match outcome {
                "hit" => "命中",
                "missed" => "漏检",
                "false_positive" => "误报",
                _ => "异常",
            };
            println!(
                "- {} [{}] 预期={} 候选={} → {}（{} tokens, {:.1}s）",
                cand.run,
                cand.ground_truth.as_str(),
                expected,
                cand_verdict,
                zh_outcome,
                tok.total(),
                elapsed_secs
            );
        }

        let mut item = json!({
            "run": cand.run,
            "repo": cand.repo_label,
            "groundTruth": cand.ground_truth.as_str(),
            "expected": expected,
            "status": if outcome == "error" { "error" } else { "evaluated" },
            "candidateVerdict": cand_verdict,
            "outcome": outcome,
            "tokens": tok.to_json_with_total(),
            "elapsedSeconds": (elapsed_secs * 10.0).round() / 10.0,
        });
        if let Some(detail) = err_detail {
            item["error"] = json!(detail);
        }
        run_results.push(item);
        // `ws` drops here, removing the temporary read-only worktree immediately.
    }

    let valid = hits + missed + false_positives;
    let hit_rate = if valid > 0 {
        Some(hits as f64 / valid as f64)
    } else {
        None
    };

    let mut fit_recorded = false;
    let mut fit_verdict: Option<String> = None;
    let mut fit_note: Option<String> = None;
    let mut fit_sample: Option<String> = None;

    if valid > 0 {
        let fit_path = crate::fit::path();
        if let Some(parent) = fit_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let _guard = crate::locks::acquire(
            crate::locks::LockKind::RunLocal,
            &fit_path.with_extension("lock"),
            true,
        )
        .map_err(|e| e.to_string())?;
        let mut card = crate::fit::load()?;
        let existing_verdict = s(&card["review"][parsed.alias.as_str()], "verdict").to_string();
        let verdict = pick_fit_verdict(&existing_verdict, hits, valid, false_positives);
        let note = format_fit_note(valid, skipped, errors, hits, missed, false_positives);
        let sample = format!("fit-bench:review:{since_label}({valid})");
        crate::fit::set(
            &mut card,
            "review",
            &parsed.alias,
            verdict,
            &note,
            Some(&sample),
        )?;
        write_json(&fit_path, &card)?;
        fit_recorded = true;
        fit_verdict = Some(verdict.to_string());
        fit_note = Some(note);
        fit_sample = Some(sample);
    }

    if parsed.json_out {
        println!(
            "{}",
            json!({
                "alias": parsed.alias,
                "model": resolved_model,
                "since": since_label,
                "limit": parsed.limit,
                "estimateOnly": false,
                "dispatched": true,
                "requiresYes": false,
                "selectedCount": selected.len(),
                "selectedMissed": selected_missed,
                "selectedCaught": selected_caught,
                "repos": repos,
                "evaluated": valid,
                "skipped": skipped,
                "hits": hits,
                "hitRate": hit_rate,
                "missed": missed,
                "falsePositives": false_positives,
                "errors": errors,
                "estimate": estimate_json,
                "usage": {
                    "tokens": total_usage.to_json_with_total(),
                    "elapsedSeconds": (total_elapsed_secs * 10.0).round() / 10.0,
                },
                "fitRecorded": fit_recorded,
                "fitVerdict": fit_verdict,
                "fitNote": fit_note,
                "fitSample": fit_sample,
                "runs": run_results,
            })
        );
    } else {
        let pct_str = hit_rate
            .map(|r| format!("{:.0}%", r * 100.0))
            .unwrap_or_else(|| "—".to_string());
        if errors > 0 {
            println!("结果：有效 {valid} 条（异常 {errors} 条，跳过 {skipped} 条）");
        } else {
            println!("结果：已评 {valid} 条（跳过 {skipped} 条）");
        }
        println!(
            "命中率：{pct_str}（{hits}/{valid}） | 漏检：{missed} | 误报：{false_positives} | 异常：{errors}"
        );
        println!(
            "总用量：{} tokens（input {} / output {} / cacheRead {}），总耗时 {:.1}s",
            total_usage.total(),
            total_usage.input,
            total_usage.output,
            total_usage.cache_read,
            total_elapsed_secs
        );
        if let (Some(v), Some(n)) = (&fit_verdict, &fit_note) {
            println!("已写入适配卡：review × {} = {} · {}", parsed.alias, v, n);
        } else if valid == 0 {
            println!("无有效结论，未写入适配卡。");
        }
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_fit_note_stays_within_80_chars() {
        let note = format_fit_note(10, 2, 1, 8, 2, 0);
        assert!(note.chars().count() <= 80, "note too long: {note}");
        assert!(!note.contains('\n'));
        assert!(note.contains("跳2异1"), "note missing skip/err tag: {note}");
    }

    #[test]
    fn order_recent_diverse_round_robins_repos() {
        let make = |run: &str, repo: &str, ep: i64| BenchCandidate {
            run: run.to_string(),
            repo_label: repo.to_string(),
            source_root: String::new(),
            record_dir: PathBuf::new(),
            prior_meta: json!({}),
            review_doc: json!({}),
            ground_truth: GroundTruth::Missed,
            orig_verdict: "PASS".to_string(),
            sort_epoch: ep,
            orig_tokens: TokenCounts::default(),
            orig_duration_secs: 0,
            has_orig_usage: false,
        };
        let ordered = order_recent_diverse(vec![
            make("r1", "repo-a", 100),
            make("r2", "repo-a", 90),
            make("r3", "repo-b", 80),
        ]);
        let ids: Vec<&str> = ordered.iter().map(|c| c.run.as_str()).collect();
        assert_eq!(ids, vec!["r1", "r3", "r2"]);
    }

    #[test]
    fn bench_review_prompt_matches_review_rs_verbatim() {
        let seq = BENCH_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!(
            "delegate-fit-bench-prompt-eq-{}-{seq}",
            std::process::id()
        ));
        let rec_dir = tmp.join("record");
        let wt_dir = tmp.join("wt");
        let diff_file = tmp.join("diff.patch");
        let _ = fs::create_dir_all(&rec_dir);
        let _ = fs::create_dir_all(&wt_dir);

        let task_brief = "修复边界校验并补充单元测试";
        write(rec_dir.join("prompt.md"), task_brief).unwrap();
        // Use a 48KB diff (> old 32KB threshold, < review.rs 200KB threshold) to confirm
        // fit_bench shares review.rs's exact inline diff threshold and formatting.
        let diff_text = format!(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n+{}\n",
            "x".repeat(48 * 1024)
        );
        write(&diff_file, &diff_text).unwrap();

        let cand = BenchCandidate {
            run: "20261127-120000-sample".to_string(),
            repo_label: "demo".to_string(),
            source_root: tmp.to_string_lossy().into_owned(),
            record_dir: rec_dir,
            prior_meta: json!({"run": "20261127-120000-sample", "name": "sample"}),
            review_doc: json!({"verdict": "PASS"}),
            ground_truth: GroundTruth::Missed,
            orig_verdict: "PASS".to_string(),
            sort_epoch: 100,
            orig_tokens: TokenCounts::default(),
            orig_duration_secs: 0,
            has_orig_usage: false,
        };
        let snap = BenchSnapshot {
            origin_repo: tmp.clone(),
            tree_sha: "after-tree-sha".to_string(),
            base_commit: Some("base-commit-sha".to_string()),
            diff_base: Some("base-commit-sha".to_string()),
            archive_dir: None,
            round_total_diff: None,
            from_rework_round: false,
        };

        let bench_prompt = build_review_prompt(&cand, &snap, &wt_dir, &diff_file, &diff_text);
        let review_prompt = review::build_full_review_prompt(
            true,
            &wt_dir,
            &diff_file,
            &diff_text,
            &tmp,
            "base-commit-sha",
            "after-tree-sha",
            "默认 rubric",
            review::DEFAULT_RUBRIC,
            task_brief,
        );
        assert_eq!(bench_prompt, review_prompt);
        let _ = fs::remove_dir_all(&tmp);
    }
}
