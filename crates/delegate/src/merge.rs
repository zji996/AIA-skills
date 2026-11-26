//! Serial integration merger (`delegate merge`):
//! Merges delivered runs into an isolated integration worktree based on source HEAD,
//! regenerates `generated.paths` when `generated.inputs` change, runs per-run `accept`
//! and final `mergeAccept` through the lane, preserves original commit titles with a
//! `Delegate-Run: <run>` trailer, evaluates `autoMerge` whitelist (with `!` exclusions
//! and `add:` add-only globs), skips & rolls back on conflicts/red tests/unsigned or
//! invalidated `controllerReviewPaths`/protected paths, writes `merge-report.md`, and
//! emits `merge_started` / `merge_item` / `merge_done` events.
use crate::changes;
use crate::common::*;
use crate::config;
use crate::events;
use crate::lane;
use crate::launch;
use crate::notes;
use crate::output;
use crate::refs;
use crate::review;
use crate::runs;
use crate::worktree;
use crate::{has, parse_simple, value as opt};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

pub fn validate_merge_accept(value: Option<&Value>) -> Res<()> {
    let Some(v) = value else { return Ok(()) };
    if v.is_null() {
        return Ok(());
    }
    if !v.as_str().is_some_and(|s| !s.contains('\0')) {
        return Err("mergeAccept must be a string without NUL or null".into());
    }
    Ok(())
}

/// Validate `autoMerge` glob rules.
/// Supported rule syntax:
/// - `<glob>` — matches added, modified, or deleted paths
/// - `add:<glob>` — matches ONLY newly added files (not modified or deleted)
/// - `!<glob>` / `!add:<glob>` — excludes matching paths (evaluated in order)
pub fn validate_auto_merge(value: Option<&Value>) -> Res<()> {
    let Some(v) = value else { return Ok(()) };
    if v.is_null() {
        return Ok(());
    }
    let arr = v
        .as_array()
        .ok_or("autoMerge must be a list of non-empty glob strings")?;
    for (idx, item) in arr.iter().enumerate() {
        let raw = item
            .as_str()
            .ok_or_else(|| format!("autoMerge[{idx}] must be a string"))?;
        if raw.contains('\0') {
            return Err(format!("autoMerge[{idx}] must not contain NUL"));
        }
        let trimmed = raw.trim();
        let after_neg = trimmed.strip_prefix('!').unwrap_or(trimmed);
        let pattern = after_neg.strip_prefix("add:").unwrap_or(after_neg);
        if pattern.trim().is_empty() {
            return Err(format!(
                "autoMerge[{idx}] ({raw:?}) has an empty glob pattern"
            ));
        }
    }
    Ok(())
}

/// Evaluate whether a single changed file `(path, kind)` is allowed by `autoMerge` rules.
/// Rules are evaluated in order (gitignore-style): later `!` exclusions override earlier
/// includes, and later positive rules can re-include. `add:` matches only `ChangeKind::Added`.
pub fn auto_merge_matches_file(rules: &[String], path: &str, kind: ChangeKind) -> bool {
    if rules.is_empty() {
        return false;
    }
    let mut allowed = false;
    for raw in rules {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (negated, rest) = match trimmed.strip_prefix('!') {
            Some(r) => (true, r),
            None => (false, trimmed),
        };
        let (add_only, pattern) = match rest.strip_prefix("add:") {
            Some(r) => (true, r),
            None => (false, rest),
        };
        if add_only && kind != ChangeKind::Added {
            continue;
        }
        if review::glob_match(pattern, path) {
            allowed = !negated;
        }
    }
    allowed
}

/// A run qualifies as 「可自主合入」 if and only if:
/// - `autoMerge` whitelist is non-empty,
/// - the run has at least one committed file change,
/// - every changed file matches `autoMerge`,
/// - review verdict is `PASS`,
/// - no `controllerReviewPaths` or `protect` rules were hit.
pub fn auto_merge_eligible(
    rules: &[String],
    files: &[(String, ChangeKind)],
    review_verdict: &str,
    has_controller_hit: bool,
    has_protect_hit: bool,
) -> bool {
    if rules.is_empty()
        || files.is_empty()
        || review_verdict != "PASS"
        || has_controller_hit
        || has_protect_hit
    {
        return false;
    }
    files
        .iter()
        .all(|(path, kind)| auto_merge_matches_file(rules, path, *kind))
}

pub fn merges_state_dir() -> PathBuf {
    state_dir().join("merges")
}

fn canonical_repo(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn load_review_doc(run: &Path, sum: &Value) -> Value {
    let f = json(run.join("review.json"));
    if f.is_object() {
        f
    } else {
        sum["review"].clone()
    }
}

/// Evaluate `controllerReviewPaths` against BOTH the run's snapshotted patterns AND the
/// current repository's `.delegate.json` patterns, so newly added sensitive paths are never bypassed.
fn effective_controller_hit(
    run: &Path,
    meta: &Value,
    repo_patterns: &[String],
) -> Res<Option<review::ControllerReviewHit>> {
    let mut merged = strings(&meta["controllerReviewPaths"]);
    for p in repo_patterns {
        if !merged.contains(p) {
            merged.push(p.clone());
        }
    }
    if merged.is_empty() {
        return Ok(None);
    }
    let mut eval_meta = meta.clone();
    eval_meta["controllerReviewPaths"] = json!(merged);
    review::evaluate_controller_paths(run, &eval_meta, true)
}

#[derive(Debug, Clone)]
struct CheckVerdict {
    review_verdict: String,
    signed_hit: Option<review::ControllerReviewHit>,
    skip_reason: Option<String>,
}

/// Check whether a candidate run satisfies all merge preconditions (delivered, accept passed,
/// review PASS or validly signed `controllerReviewPaths`, no `defaults.protect` hits).
fn evaluate_run_for_merge(
    run: &Path,
    repo_cfg: &Value,
    protect_rules: &[String],
    controller_patterns: &[String],
    require_review_if_configured: bool,
) -> CheckVerdict {
    let meta = json(run.join("meta.json"));
    let st = runs::state(run);
    if runs::active(&st) {
        return CheckVerdict {
            review_verdict: String::new(),
            signed_hit: None,
            skip_reason: Some(format!("任务仍在运行（{st}）")),
        };
    }
    if s(&meta, "mode") == "read-only" || !meta["worktree"].is_object() {
        return CheckVerdict {
            review_verdict: String::new(),
            signed_hit: None,
            skip_reason: Some("非工作树写入任务".into()),
        };
    }
    if b(&meta, "isMerge") {
        return CheckVerdict {
            review_verdict: String::new(),
            signed_hit: None,
            skip_reason: Some("不能嵌套合入另一个合入任务".into()),
        };
    }
    let sum = json(run.join("summary.json"));
    if b(&sum, "question") {
        return CheckVerdict {
            review_verdict: String::new(),
            signed_hit: None,
            skip_reason: Some("任务仍在等待主控回答提问".into()),
        };
    }
    if st != "delivered" {
        return CheckVerdict {
            review_verdict: String::new(),
            signed_hit: None,
            skip_reason: Some(format!("未交付（状态：{st}）")),
        };
    }
    let accept_obj = &sum["accept"];
    let meta_accept = s(&meta, "accept");
    if accept_obj.is_object() {
        if !b(accept_obj, "ok") {
            return CheckVerdict {
                review_verdict: String::new(),
                signed_hit: None,
                skip_reason: Some("原任务验收未通过".into()),
            };
        }
    } else if !meta_accept.is_empty() {
        return CheckVerdict {
            review_verdict: String::new(),
            signed_hit: None,
            skip_reason: Some("原任务验收结果缺失".into()),
        };
    }

    // Check repository protect rules (e.g. ADR docs/decision/, apps/console/).
    // The merger strictly refuses protected paths regardless of per-run --allow.
    if !protect_rules.is_empty() {
        if let Some(changed_paths) = worktree::pending_changed_paths(run, &meta) {
            let hits: Vec<String> = changed_paths
                .into_iter()
                .filter(|p| worktree::matches_rule(p, protect_rules))
                .collect();
            if !hits.is_empty() {
                return CheckVerdict {
                    review_verdict: String::new(),
                    signed_hit: None,
                    skip_reason: Some(format!("命中受保护路径：{}", hits.join("、"))),
                };
            }
        }
    }

    let review_doc = load_review_doc(run, &sum);
    let review_verdict = if review_doc.is_object() {
        s(&review_doc, "verdict").to_string()
    } else {
        String::new()
    };
    let review_status = if review_doc.is_object() {
        s(&review_doc, "status").to_string()
    } else {
        String::new()
    };

    // Check live `controllerReviewPaths` + signature validity (`sign --check`).
    let hit = match effective_controller_hit(run, &meta, controller_patterns) {
        Ok(h) => h,
        Err(e) => {
            return CheckVerdict {
                review_verdict,
                signed_hit: None,
                skip_reason: Some(format!("检查 controllerReviewPaths 失败：{e}")),
            };
        }
    };
    let mut signed_hit = None;
    if let Some(hit) = hit {
        let sign_doc = json(run.join("sign.json"));
        let signed_hash = s(&sign_doc, "hash");
        if signed_hash.is_empty() {
            return CheckVerdict {
                review_verdict,
                signed_hit: None,
                skip_reason: Some(format!(
                    "命中 controllerReviewPaths 未签字：{}",
                    hit.paths.join("、")
                )),
            };
        }
        if signed_hash != hit.hash {
            return CheckVerdict {
                review_verdict,
                signed_hit: None,
                skip_reason: Some(format!(
                    "签字已失效（sign --check 不通过，哈希不匹配）：{}",
                    hit.paths.join("、")
                )),
            };
        }
        signed_hit = Some(hit);
    }

    // Check review verdict if review is present or enabled on the repository.
    if review_doc.is_object() {
        if review_status == "reviewing" || review_status == "reworking" {
            return CheckVerdict {
                review_verdict,
                signed_hit,
                skip_reason: Some(format!("审查尚未完成（{review_status}）")),
            };
        }
        match review_verdict.as_str() {
            "PASS" => {}
            "NEEDS_CONTROLLER" => {
                let upgraded_by = s(&review_doc, "upgradedBy");
                let orig = s(&review_doc, "originalVerdict");
                let signed_ok = signed_hit.is_some()
                    && upgraded_by == "controllerReviewPaths"
                    && orig != "REWORK";
                if !signed_ok {
                    return CheckVerdict {
                        review_verdict,
                        signed_hit,
                        skip_reason: Some(format!(
                            "审查判定 NEEDS_CONTROLLER（{}），待主控处理",
                            if upgraded_by.is_empty() {
                                "未签字"
                            } else {
                                upgraded_by
                            }
                        )),
                    };
                }
            }
            "ENV_ERROR" => {
                return CheckVerdict {
                    review_verdict,
                    signed_hit,
                    skip_reason: Some("审查环境错误，需重试审查".into()),
                };
            }
            "REWORK" => {
                return CheckVerdict {
                    review_verdict,
                    signed_hit,
                    skip_reason: Some("审查判定 REWORK，需先返工".into()),
                };
            }
            other => {
                let reason = format!("审查状态未通过（{other}）");
                return CheckVerdict {
                    review_verdict,
                    signed_hit,
                    skip_reason: Some(reason),
                };
            }
        }
    } else if require_review_if_configured
        && review::is_enabled(&repo_cfg["review"])
        && signed_hit.is_none()
    {
        return CheckVerdict {
            review_verdict,
            signed_hit,
            skip_reason: Some("仓库开启了自动审查，但该任务尚无审查记录".into()),
        };
    }

    CheckVerdict {
        review_verdict,
        signed_hit,
        skip_reason: None,
    }
}

/// Discover all ready runs in the current repository for `delegate merge --ready`.
fn discover_ready_runs(
    source: &Path,
    repo_cfg: &Value,
    protect_rules: &[String],
    controller_patterns: &[String],
) -> Vec<PathBuf> {
    let source_canon = canonical_repo(source);
    let all = runs::all_runs();
    // Map every run to its chain head so reply chains are considered once at their latest round.
    let mut heads: BTreeMap<String, PathBuf> = BTreeMap::new();
    for run in all {
        let head = if runs::continues_conversation(&run) {
            runs::latest(run)
        } else {
            run
        };
        let id = head
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        heads.entry(id).or_insert(head);
    }

    let mut ready: Vec<(u64, String, PathBuf)> = Vec::new();
    for (id, run) in heads {
        let meta = json(run.join("meta.json"));
        if !meta.is_object() || b(&meta, "isMerge") {
            continue;
        }
        if s(&meta, "mode") != "write" || !meta["worktree"].is_object() {
            continue;
        }
        let run_src = s(&meta["worktree"], "source");
        if run_src.is_empty() || canonical_repo(Path::new(run_src)) != source_canon {
            continue;
        }
        if run.join(".applied").exists() || run.join(".applied-cleaned").exists() {
            continue;
        }
        let verdict = evaluate_run_for_merge(
            &run,
            repo_cfg,
            protect_rules,
            controller_patterns,
            true,
        );
        if verdict.skip_reason.is_none() {
            let ns = meta["startedNs"].as_u64().unwrap_or(0);
            ready.push((ns, id, run));
        }
    }
    ready.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    ready.into_iter().map(|(_, _, p)| p).collect()
}

fn prepare_integration_worktree(
    source: &Path,
    base_head: &str,
    wt_path: &Path,
    work_cfg: &Value,
    env_cfg: &Value,
    merge_run: &Path,
) -> Res<Vec<String>> {
    fs::create_dir_all(wt_path.parent().unwrap_or(Path::new("/"))).map_err(|e| e.to_string())?;
    if wt_path.exists() {
        let _ = git(
            source,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
        let _ = fs::remove_dir_all(wt_path);
        let _ = git(source, &["worktree", "prune"]);
    }
    git(
        source,
        &[
            "worktree",
            "add",
            "--detach",
            &wt_path.to_string_lossy(),
            base_head,
        ],
    )?;

    let copies = strings(&work_cfg["copy"]);
    let links = strings(&work_cfg["link"]);
    let managed: Vec<String> = copies.iter().chain(links.iter()).cloned().collect();
    for item in &managed {
        let origin = source.join(item);
        let target = wt_path.join(item);
        if target.is_dir()
            && !target.is_symlink()
            && fs::read_dir(&target).is_ok_and(|mut d| d.next().is_none())
        {
            let _ = fs::remove_dir(&target);
        }
        if !origin.exists() && !origin.is_symlink() {
            continue;
        }
        if target.exists() || target.is_symlink() {
            continue;
        }
        fs::create_dir_all(target.parent().unwrap_or(wt_path)).map_err(|e| e.to_string())?;
        if links.contains(item) {
            symlink(&origin, &target).map_err(|e| e.to_string())?;
        } else {
            worktree::copy_rec(&origin, &target)?;
        }
    }
    for item in &managed {
        let _ = git(wt_path, &["update-index", "--skip-worktree", "--", item]);
    }

    let mut setup = strings(&work_cfg["setup"]);
    setup.extend(strings(&work_cfg["writeSetup"]));
    if !setup.is_empty() {
        let timeout = seconds(&setting("SETUP_TIMEOUT", "10m"))?;
        let setup_cmds: Vec<&str> = setup.iter().map(String::as_str).collect();
        let _slot = lane::acquire_shell_for_commands(
            &format!(
                "merge-setup {}",
                merge_run.file_name().unwrap_or_default().to_string_lossy()
            ),
            &setup_cmds,
            None,
            Some(merge_run),
            |_, _| {},
        )?;
        let log = merge_run.join("setup.log");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .map_err(|e| e.to_string())?;
        for (idx, cmd) in setup.iter().enumerate() {
            writeln!(file, "$ {cmd}").map_err(|e| e.to_string())?;
            file.flush().map_err(|e| e.to_string())?;
            let (code, _) = run_shell(
                (cmd, &format!("setup-{}", idx + 1)),
                wt_path,
                merge_run,
                env_cfg,
                Some(timeout),
                &mut file,
                None,
            )?;
            writeln!(file, "[exit {code}]").map_err(|e| e.to_string())?;
            if code != 0 {
                return Err(format!(
                    "integration worktree setup failed (exit {code}): {cmd}; see {}",
                    log.display()
                ));
            }
        }
    }
    Ok(managed)
}

fn rollback_integration_worktree(wt_path: &Path, last_good_head: &str, managed: &[String]) {
    let _ = git(wt_path, &["reset", "--hard", "-q", last_good_head]);
    let mut clean_args: Vec<String> = vec!["clean".into(), "-fd".into(), "-q".into()];
    for m in managed {
        clean_args.push("-e".into());
        clean_args.push(m.clone());
    }
    let refs: Vec<&str> = clean_args.iter().map(String::as_str).collect();
    let _ = git(wt_path, &refs);
}

fn run_lane_command(
    label: &str,
    command: &str,
    workdir: &Path,
    merge_run: &Path,
    env_cfg: &Value,
    timeout: Option<f64>,
    log_path: &Path,
) -> Res<(i32, bool)> {
    let _slot = lane::acquire_shell_for_commands(label, &[command], None, Some(merge_run), |_, _| {})?;
    let mut file = File::create(log_path)
        .map_err(|e| format!("create {}: {e}", log_path.display()))?;
    writeln!(file, "$ {command}").map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    let (code, timed) = run_shell(
        (command, label),
        workdir,
        merge_run,
        env_cfg,
        timeout,
        &mut file,
        None,
    )?;
    writeln!(file, "\n[exit {code}{}]", if timed { " timeout" } else { "" })
        .map_err(|e| e.to_string())?;
    Ok((code, timed))
}

fn stage_paths_excluding_managed(
    wt_path: &Path,
    managed: &[String],
) -> Res<Vec<(String, ChangeKind)>> {
    // Stage all non-managed changes so tracked modifications, deletions, and newly added
    // files (including regenerated outputs) are captured accurately.
    let status_raw = git(
        wt_path,
        &["status", "--porcelain=v1", "-z", "-uall"],
    )?;
    let mut to_stage: Vec<String> = Vec::new();
    let mut parts = status_raw.split(|b| *b == 0);
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        let xy = &entry[..2];
        let path = String::from_utf8_lossy(&entry[3..]).into_owned();
        if xy[0] == b'R' || xy[0] == b'C' {
            let _ = parts.next();
        }
        if worktree::matches_rule(&path, managed) {
            continue;
        }
        to_stage.push(path);
    }
    if to_stage.is_empty() {
        return Ok(Vec::new());
    }
    let mut add_cmd = git_command();
    add_cmd.arg("-C").arg(wt_path).args(["add", "-A", "--"]);
    for p in &to_stage {
        add_cmd.arg(p);
    }
    let out = add_cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "git add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let ns_raw = git(
        wt_path,
        &["diff", "--cached", "--name-status", "--no-renames", "-z"],
    )?;
    let tokens: Vec<&[u8]> = ns_raw
        .split(|b| *b == 0)
        .filter(|t| !t.is_empty())
        .collect();
    let mut committed = Vec::new();
    let mut idx = 0;
    while idx + 1 < tokens.len() {
        let code = String::from_utf8_lossy(tokens[idx]);
        let path = String::from_utf8_lossy(tokens[idx + 1]).into_owned();
        let kind = match code.chars().next() {
            Some('A') => ChangeKind::Added,
            Some('D') => ChangeKind::Deleted,
            _ => ChangeKind::Modified,
        };
        committed.push((path, kind));
        idx += 2;
    }
    Ok(committed)
}

fn verify_signed_paths_after_merge(
    wt_path: &Path,
    last_good_head: &str,
    signed_hit: &review::ControllerReviewHit,
) -> Res<bool> {
    let mut args: Vec<&str> = vec![
        "diff",
        "--cached",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--color=never",
        "--binary",
        last_good_head,
        "--",
    ];
    for p in &signed_hit.paths {
        args.push(p.as_str());
    }
    let diff_bytes = git(wt_path, &args)?;
    let actual_hash = format!("sha256:{}", review::sha256_hex(&diff_bytes));
    Ok(actual_hash == signed_hit.hash)
}

fn commit_staged(wt_path: &Path, title: &str, source_run_id: &str) -> Res<String> {
    let clean_title = title
        .lines()
        .next()
        .unwrap_or(source_run_id)
        .trim();
    let subject = if clean_title.is_empty() {
        source_run_id
    } else {
        clean_title
    };
    let message = format!("{subject}\n\nDelegate-Run: {source_run_id}\n");
    let mut cmd = git_command();
    cmd.arg("-C").arg(wt_path).args(["commit", "-m", &message]);
    // Only supply fallback identity if git has none configured.
    if git_text(wt_path, &["config", "user.name"]).is_err() {
        cmd.env("GIT_AUTHOR_NAME", "delegate")
            .env("GIT_COMMITTER_NAME", "delegate");
    }
    if git_text(wt_path, &["config", "user.email"]).is_err() {
        cmd.env("GIT_AUTHOR_EMAIL", "delegate@localhost")
            .env("GIT_COMMITTER_EMAIL", "delegate@localhost");
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        return Err(format!("git commit failed: {detail}"));
    }
    git_text(wt_path, &["rev-parse", "HEAD"])
}

fn pick_original_title(run: &Path, meta: &Value, run_id: &str) -> String {
    if let Ok(commits) = worktree::collect_commits(run, meta) {
        if let Some(first) = commits.first() {
            let t = s(first, "title").trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    let name = s(meta, "name").trim();
    if !name.is_empty() {
        name.to_string()
    } else {
        run_id.to_string()
    }
}

fn md_cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ").trim().to_string()
}

fn render_merge_report_md(report: &Value) -> String {
    let merge_run = s(report, "mergeRun");
    let repo = s(report, "repo");
    let base_head = s(report, "baseHead");
    let base_short = &base_head[..base_head.len().min(7)];
    let merged_count = n(report, "mergedCount");
    let skipped_count = n(report, "skippedCount");
    let auto_count = n(report, "autoMergeCount");
    let merge_accept_cmd = s(&report["mergeAccept"], "command");
    let merge_accept_label = if !report["mergeAccept"].is_object() {
        "未配置".to_string()
    } else if b(&report["mergeAccept"], "skipped") {
        "未执行（无已合入 run）".to_string()
    } else if b(&report["mergeAccept"], "ok") {
        format!("PASS (`{merge_accept_cmd}`)")
    } else {
        format!(
            "FAIL (exit {}, `{merge_accept_cmd}`)",
            n(&report["mergeAccept"], "exitCode")
        )
    };

    let mut md = String::new();
    md.push_str(&format!("# 合入报告：{merge_run}\n\n"));
    md.push_str(&format!("- 仓库：`{repo}`\n"));
    md.push_str(&format!("- 基线 HEAD：`{base_short}`\n"));
    md.push_str(&format!(
        "- 结果：合入 **{merged_count}** · 跳过 **{skipped_count}** · 可自主合入 **{auto_count}**\n"
    ));
    md.push_str(&format!("- 总验收（mergeAccept）：{merge_accept_label}\n\n"));
    md.push_str("| Run | 状态 | 提交 | 验收 | 自主合入 | 原因 / 说明 |\n");
    md.push_str("|---|---|---|---|---|---|\n");
    for item in report["items"].as_array().into_iter().flatten() {
        let run_id = s(item, "run");
        let status = s(item, "status");
        let commit_sha = s(item, "commit");
        let title = s(item, "title");
        let commit_col = if commit_sha.is_empty() {
            "-".to_string()
        } else {
            let short = &commit_sha[..commit_sha.len().min(7)];
            format!("`{short}` {}", md_cell(title))
        };
        let accept_col = match item["acceptOk"].as_bool() {
            Some(true) => "PASS",
            Some(false) => "FAIL",
            None => "-",
        };
        let auto_col = if status == "MERGED" {
            if b(item, "autoMerge") {
                "可自主合入"
            } else {
                "待主控点头"
            }
        } else {
            "-"
        };
        let reason = s(item, "reason");
        let reason_col = if reason.is_empty() {
            "-".to_string()
        } else {
            md_cell(reason)
        };
        md.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} |\n",
            md_cell(run_id),
            status,
            commit_col,
            accept_col,
            auto_col,
            reason_col
        ));
    }
    md.push('\n');
    if merged_count > 0 && b(&report["mergeAccept"], "ok") {
        md.push_str(&format!(
            "## 下一步\n\n主控确认后将提交链原样合回源仓库（push 仍由主控执行）：\n\n```bash\ndelegate apply --keep-commits {merge_run}\n```\n"
        ));
    } else if merged_count > 0 {
        md.push_str(&format!(
            "## 下一步\n\n最终 `mergeAccept` 未通过，请检查 `merge-accept.log`；集成 worktree 保留在 `{}`。\n",
            s(report, "worktree")
        ));
    } else {
        md.push_str("## 下一步\n\n本批所有 run 均已跳过，未产生新提交；请逐项处理跳过原因后重试。\n");
    }
    md
}

fn save_merge_state_record(report: &Value) {
    let dir = merges_state_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let id = s(report, "mergeRun");
    if id.is_empty() {
        return;
    }
    let _ = write_json(dir.join(format!("{id}.json")), report);
}

/// Called from `main.rs` after `delegate apply --keep-commits <merge-run>` succeeds.
/// Updates the merge state record and marks every merged source run as `.applied`.
pub fn mark_merge_run_applied(merge_run: &Path, applied_head: &str) {
    let report_path = merge_run.join("merge-report.json");
    if !report_path.is_file() {
        return;
    }
    let mut report = json(&report_path);
    if !report.is_object() {
        return;
    }
    let at = iso();
    report["applied"] = json!(true);
    report["appliedAt"] = json!(&at);
    if !applied_head.is_empty() {
        report["appliedHead"] = json!(applied_head);
    }
    let _ = write_json(&report_path, &report);
    save_merge_state_record(&report);

    let runs_dir = merge_run.parent().unwrap_or(Path::new("/"));
    let merge_id = s(&report, "mergeRun").to_string();
    for item in report["items"].as_array().into_iter().flatten() {
        if s(item, "status") != "MERGED" {
            continue;
        }
        let src_id = s(item, "run");
        if src_id.is_empty() {
            continue;
        }
        let src_dir = runs_dir.join(src_id);
        if !src_dir.is_dir() {
            continue;
        }
        let applied_state = json!({
            "at": at,
            "head": applied_head,
            "viaMerge": merge_id,
        });
        let _ = write_json(src_dir.join(".applied"), &applied_state);
        let mut src_meta = json(src_dir.join("meta.json"));
        if src_meta.is_object() {
            src_meta["appliedAt"] = json!(&at);
            if !applied_head.is_empty() {
                src_meta["appliedHead"] = json!(applied_head);
            }
            src_meta["appliedViaMerge"] = json!(&merge_id);
            let _ = write_json(src_dir.join("meta.json"), &src_meta);
            crate::eta::record_calibration_applied(&src_dir, &src_meta, &at);
        }
    }
}

/// Load saved merge reports from `<state_dir>/merges/*.json`, newest first.
pub fn load_merge_reports(repo_filter: Option<&Path>) -> Vec<Value> {
    let dir = merges_state_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let filter_canon = repo_filter.map(canonical_repo);
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let mut doc = json(&p);
        if !doc.is_object() {
            continue;
        }
        if let Some(ref want) = filter_canon {
            let r = s(&doc, "repo");
            if r.is_empty() || canonical_repo(Path::new(r)) != *want {
                continue;
            }
        }
        // Reflect live .applied status if the merge run directory still exists.
        let run_dir = PathBuf::from(s(&doc, "dir"));
        if run_dir.join(".applied").is_file() {
            doc["applied"] = json!(true);
        }
        out.push(doc);
    }
    out.sort_by(|a, b| {
        s(b, "startedAt")
            .cmp(s(a, "startedAt"))
            .then_with(|| s(b, "mergeRun").cmp(s(a, "mergeRun")))
    });
    out
}

fn list_command(args: &[String]) -> Res<i32> {
    let mut all = false;
    let mut json_mode = output::json_enabled();
    for a in args {
        match a.as_str() {
            "--all" => all = true,
            "--json" => json_mode = true,
            other => {
                return Err(unrecognized_arg(
                    other,
                    &["--all", "--json"],
                    "delegate merge list [--all] [--json]",
                ));
            }
        }
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let filter = if all { None } else { git_top(&cwd) };
    let reports = load_merge_reports(filter.as_deref());
    if json_mode {
        for r in &reports {
            println!("{r}");
        }
        return Ok(0);
    }
    if reports.is_empty() {
        println!("暂无合入记录");
        return Ok(0);
    }
    for r in &reports {
        let id = s(r, "mergeRun");
        let merged = n(r, "mergedCount");
        let skipped = n(r, "skippedCount");
        let auto = n(r, "autoMergeCount");
        let applied = if b(r, "applied") { "已合回" } else { "待合回" };
        let acc = if b(&r["mergeAccept"], "ok") {
            "总验✓"
        } else if b(&r["mergeAccept"], "skipped") {
            "总验-"
        } else {
            "总验✗"
        };
        let repo_name = Path::new(s(r, "repo"))
            .file_name()
            .and_then(|x| x.to_str())
            .unwrap_or("?");
        println!(
            "{id}  [{repo_name}]  合入 {merged} / 跳过 {skipped} / 自主 {auto}  {acc}  {applied}"
        );
    }
    Ok(0)
}

/// Entry point for `delegate merge`:
/// - `delegate merge [--ready] [--name NAME] [--json] [<run>...]`
/// - `delegate merge list [--all] [--json]`
pub fn command(args: &[String]) -> Res<i32> {
    if matches!(args.first().map(String::as_str), Some("list" | "--list")) {
        return list_command(&args[1..]);
    }
    let (pos, flags, opts) = parse_simple("merge", args, &["--ready", "--json"], &["--name"])?;
    if has(&flags, "--json") {
        output::enable_json();
    }
    let ready_mode = has(&flags, "--ready");
    if ready_mode && !pos.is_empty() {
        return Err("merge: cannot combine --ready with explicit run arguments".into());
    }
    if !ready_mode && pos.is_empty() {
        return Err("usage: delegate merge [--ready | <run>...] [--name NAME] [--json] | delegate merge list [--all] [--json]".into());
    }

    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut source_repo = git_top(&cwd);

    // Resolve target runs (either explicit or --ready).
    let target_runs: Vec<PathBuf> = if ready_mode {
        let Some(ref top) = source_repo else {
            return Err("merge --ready must be run inside a git repository".into());
        };
        let cfg = config::load(Some(top))?;
        let mut protect_rules = strings(&cfg.value["defaults"]["protect"]);
        if !protect_rules.is_empty() {
            launch::normalize_protect(&mut protect_rules)?;
        }
        let controller_patterns = strings(&cfg.value["controllerReviewPaths"]);
        let found = discover_ready_runs(top, &cfg.value, &protect_rules, &controller_patterns);
        if found.is_empty() {
            if output::json_enabled() {
                println!(
                    "{}",
                    json!({"ok": true, "ready": 0, "mergedCount": 0, "skippedCount": 0, "items": []})
                );
            } else {
                println!("没有可合入的就绪 run（需已交付、验收通过，且审查 PASS 或已签字）");
            }
            return Ok(0);
        }
        found
    } else {
        let mut resolved_list = Vec::new();
        for name in &pos {
            let r = runs::resolve(name)?;
            let head = if runs::continues_conversation(&r) {
                runs::latest(r)
            } else {
                r
            };
            if !resolved_list.contains(&head) {
                resolved_list.push(head);
            }
        }
        if source_repo.is_none() {
            for r in &resolved_list {
                let m = json(r.join("meta.json"));
                let src = s(&m["worktree"], "source");
                if !src.is_empty() && Path::new(src).is_dir() {
                    source_repo = Some(PathBuf::from(src));
                    break;
                }
            }
        }
        resolved_list
    };

    let source = source_repo.ok_or("merge requires a git repository")?;
    let source_canon = canonical_repo(&source);
    let repo_str = source_canon.to_string_lossy().into_owned();

    // Ensure all explicit worktree runs belong to the same repository.
    for r in &target_runs {
        let m = json(r.join("meta.json"));
        let src = s(&m["worktree"], "source");
        if !src.is_empty() && canonical_repo(Path::new(src)) != source_canon {
            return Err(format!(
                "run {} belongs to a different repository ({src} != {repo_str})",
                r.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
    }

    let base_head = git_text(&source_canon, &["rev-parse", "--verify", "HEAD^{commit}"])
        .map_err(|e| format!("source repository has no HEAD commit: {e}"))?;

    let cfg = config::load(Some(&source_canon))?;
    let accept_cmd = s(&cfg.value, "accept").to_string();
    let merge_accept_cmd = {
        let ma = s(&cfg.value, "mergeAccept");
        if !ma.is_empty() {
            ma.to_string()
        } else {
            accept_cmd.clone()
        }
    };
    let auto_merge_rules = strings(&cfg.value["autoMerge"]);
    let mut protect_rules = strings(&cfg.value["defaults"]["protect"]);
    if !protect_rules.is_empty() {
        launch::normalize_protect(&mut protect_rules)?;
    }
    let controller_patterns = strings(&cfg.value["controllerReviewPaths"]);
    let accept_timeout = cfg.value["acceptTimeout"]
        .as_str()
        .and_then(|d| seconds(d).ok());
    let generate_timeout = worktree::generate_timeout_seconds().ok().flatten();
    let work_cfg = worktree::work_config(&cfg.value, &source_canon.join(".delegate.json"))?;
    let env_cfg = if cfg.value["env"].is_object() {
        cfg.value["env"].clone()
    } else {
        json!({})
    };

    // Allocate merge run directory and standalone integration worktree.
    let merge_name = opt(&opts, "--name")
        .filter(|n| !n.trim().is_empty())
        .unwrap_or("merge")
        .to_string();
    let root = runs_root();
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let base_id = format!("{}-{}", stamp(), slug(&merge_name));
    let mut merge_run_id = base_id.clone();
    let mut merge_run = root.join(&merge_run_id);
    let mut suffix = 2u32;
    while merge_run.exists() {
        merge_run_id = format!("{base_id}-{suffix}");
        merge_run = root.join(&merge_run_id);
        suffix += 1;
    }
    fs::create_dir_all(&merge_run).map_err(|e| e.to_string())?;

    let wt_path = cache_dir().join("worktrees").join(&merge_run_id);
    let managed = prepare_integration_worktree(
        &source_canon,
        &base_head,
        &wt_path,
        &work_cfg,
        &env_cfg,
        &merge_run,
    )?;

    write(merge_run.join("worktree-base-commit"), &base_head)?;
    let initial_snap = changes::snapshot(&wt_path, &merge_run, &managed)
        .ok_or("failed to snapshot integration worktree")?;
    let started_at = iso();
    let started_epoch = epoch() as i64;
    let started_ns = now_ns() as u64;

    let run_ids: Vec<String> = target_runs
        .iter()
        .map(|r| events::run_id_for(r, &json(r.join("meta.json"))))
        .collect();

    let mut meta = json!({
        "run": merge_run_id,
        "name": merge_name,
        "kind": "merge",
        "isMerge": true,
        "dir": merge_run,
        "workdir": wt_path,
        "mode": "write",
        "agent": "merger",
        "top": source_canon,
        "base": initial_snap,
        "chainBase": initial_snap["tree"],
        "snapshotExclude": managed,
        "worktree": {
            "source": source_canon,
            "path": wt_path,
            "base": base_head,
            "config": work_cfg,
            "pooled": false,
        },
        "env": env_cfg,
        "accept": merge_accept_cmd,
        "merge": {
            "sourceRuns": run_ids,
            "baseHead": base_head,
            "readyMode": ready_mode,
        },
        "caller": caller(),
        "startedAt": started_at,
        "startedEpoch": started_epoch,
        "startedNs": started_ns,
    });
    write_json(merge_run.join("meta.json"), &meta)?;
    refs::pin_slot(&merge_run, &meta, "worktree-base", &base_head);
    refs::pin_run(&merge_run, &meta);
    notes::register_repo();

    events::emit(
        &merge_run_id,
        &repo_str,
        "merge_started",
        json!({
            "name": merge_name,
            "mergeRun": merge_run_id,
            "runs": run_ids,
            "count": target_runs.len(),
            "baseHead": base_head,
            "readyMode": ready_mode,
        }),
    );

    lane::install_signals();

    let mut last_good_head = base_head.clone();
    let mut items: Vec<Value> = Vec::new();
    let mut merged_count: i64 = 0;
    let mut skipped_count: i64 = 0;
    let mut auto_merge_count: i64 = 0;

    for (idx, r) in target_runs.iter().enumerate() {
        let r_meta = json(r.join("meta.json"));
        let r_id = events::run_id_for(r, &r_meta);
        let r_name = s(&r_meta, "name").to_string();

        if lane::stopped() {
            let reason = "收到中断信号，停止后续合入".to_string();
            skipped_count += 1;
            let entry = json!({
                "run": r_id,
                "name": r_name,
                "status": "SKIPPED",
                "commit": "",
                "title": "",
                "acceptOk": Value::Null,
                "autoMerge": false,
                "reason": reason,
            });
            events::emit(
                &merge_run_id,
                &repo_str,
                "merge_item",
                json!({
                    "mergeRun": merge_run_id,
                    "sourceRun": r_id,
                    "name": r_name,
                    "status": "SKIPPED",
                    "reason": reason,
                }),
            );
            items.push(entry);
            continue;
        }

        // 1. Pre-merge eligibility checks (delivered, accept, protect, controllerReviewPaths + sign, review).
        let check = evaluate_run_for_merge(
            r,
            &cfg.value,
            &protect_rules,
            &controller_patterns,
            false,
        );
        if let Some(reason) = check.skip_reason {
            skipped_count += 1;
            let entry = json!({
                "run": r_id,
                "name": r_name,
                "status": "SKIPPED",
                "commit": "",
                "title": "",
                "acceptOk": Value::Null,
                "autoMerge": false,
                "reason": reason,
            });
            events::emit(
                &merge_run_id,
                &repo_str,
                "merge_item",
                json!({
                    "mergeRun": merge_run_id,
                    "sourceRun": r_id,
                    "name": r_name,
                    "status": "SKIPPED",
                    "reason": reason,
                }),
            );
            items.push(entry);
            continue;
        }

        // 2. 3-way merge net changes into integration worktree (reusing apply logic).
        let merge_res = match worktree::merge_run_into_dir(r, &wt_path, &source_canon) {
            Ok(res) => res,
            Err(e) => {
                rollback_integration_worktree(&wt_path, &last_good_head, &managed);
                skipped_count += 1;
                let reason = format!("合并出错：{e}");
                let entry = json!({
                    "run": r_id,
                    "name": r_name,
                    "status": "SKIPPED",
                    "commit": "",
                    "title": "",
                    "acceptOk": Value::Null,
                    "autoMerge": false,
                    "reason": reason,
                });
                events::emit(
                    &merge_run_id,
                    &repo_str,
                    "merge_item",
                    json!({
                        "mergeRun": merge_run_id,
                        "sourceRun": r_id,
                        "name": r_name,
                        "status": "SKIPPED",
                        "reason": reason,
                    }),
                );
                items.push(entry);
                continue;
            }
        };

        if !merge_res.conflicts.is_empty() || !merge_res.marked_paths.is_empty() {
            rollback_integration_worktree(&wt_path, &last_good_head, &managed);
            skipped_count += 1;
            let mut all_conf: BTreeSet<String> = BTreeSet::new();
            for p in &merge_res.conflicts {
                all_conf.insert(p.clone());
            }
            for p in &merge_res.marked_paths {
                all_conf.insert(p.clone());
            }
            let conf_list: Vec<String> = all_conf.into_iter().collect();
            let reason = format!("语义冲突 / 冲突标记：{}", conf_list.join("、"));
            let entry = json!({
                "run": r_id,
                "name": r_name,
                "status": "SKIPPED",
                "commit": "",
                "title": "",
                "acceptOk": Value::Null,
                "autoMerge": false,
                "conflicts": conf_list,
                "reason": reason,
            });
            events::emit(
                &merge_run_id,
                &repo_str,
                "merge_item",
                json!({
                    "mergeRun": merge_run_id,
                    "sourceRun": r_id,
                    "name": r_name,
                    "status": "SKIPPED",
                    "conflicts": conf_list,
                    "reason": reason,
                }),
            );
            items.push(entry);
            continue;
        }

        // 3. Regenerate `generated.paths` if merged files touched `generated.inputs`.
        let mut regenerated = false;
        if merge_res.touches_generated_inputs {
            let gen_log = merge_run.join(format!("generate-{}-{}.log", idx + 1, slug(&r_id)));
            let gen_label = format!("merge-generate {r_id}");
            match run_lane_command(
                &gen_label,
                &merge_res.generate_command,
                &wt_path,
                &merge_run,
                &env_cfg,
                generate_timeout,
                &gen_log,
            ) {
                Ok((0, false)) => {
                    regenerated = true;
                }
                Ok((code, timed)) => {
                    rollback_integration_worktree(&wt_path, &last_good_head, &managed);
                    skipped_count += 1;
                    let reason = if timed {
                        format!("重新生成超时（exit {code}）")
                    } else {
                        format!("重新生成失败（exit {code}）")
                    };
                    let entry = json!({
                        "run": r_id,
                        "name": r_name,
                        "status": "SKIPPED",
                        "commit": "",
                        "title": "",
                        "acceptOk": Value::Null,
                        "autoMerge": false,
                        "reason": reason,
                    });
                    events::emit(
                        &merge_run_id,
                        &repo_str,
                        "merge_item",
                        json!({
                            "mergeRun": merge_run_id,
                            "sourceRun": r_id,
                            "name": r_name,
                            "status": "SKIPPED",
                            "reason": reason,
                        }),
                    );
                    items.push(entry);
                    continue;
                }
                Err(e) => {
                    rollback_integration_worktree(&wt_path, &last_good_head, &managed);
                    skipped_count += 1;
                    let reason = format!("重新生成执行出错：{e}");
                    let entry = json!({
                        "run": r_id,
                        "name": r_name,
                        "status": "SKIPPED",
                        "commit": "",
                        "title": "",
                        "acceptOk": Value::Null,
                        "autoMerge": false,
                        "reason": reason,
                    });
                    events::emit(
                        &merge_run_id,
                        &repo_str,
                        "merge_item",
                        json!({
                            "mergeRun": merge_run_id,
                            "sourceRun": r_id,
                            "name": r_name,
                            "status": "SKIPPED",
                            "reason": reason,
                        }),
                    );
                    items.push(entry);
                    continue;
                }
            }
        }

        // 4. Stage merged + regenerated paths and verify post-merge integrity:
        //    - ensure non-empty diff
        //    - ensure no protected paths crept in (e.g. via regeneration)
        //    - ensure signed controllerReviewPaths diff hash matches byte-for-byte
        let committed_files = match stage_paths_excluding_managed(&wt_path, &managed) {
            Ok(files) => files,
            Err(e) => {
                rollback_integration_worktree(&wt_path, &last_good_head, &managed);
                skipped_count += 1;
                let reason = format!("暂存改动失败：{e}");
                let entry = json!({
                    "run": r_id,
                    "name": r_name,
                    "status": "SKIPPED",
                    "commit": "",
                    "title": "",
                    "acceptOk": Value::Null,
                    "autoMerge": false,
                    "reason": reason,
                });
                events::emit(
                    &merge_run_id,
                    &repo_str,
                    "merge_item",
                    json!({
                        "mergeRun": merge_run_id,
                        "sourceRun": r_id,
                        "name": r_name,
                        "status": "SKIPPED",
                        "reason": reason,
                    }),
                );
                items.push(entry);
                continue;
            }
        };

        if committed_files.is_empty() {
            rollback_integration_worktree(&wt_path, &last_good_head, &managed);
            skipped_count += 1;
            let reason = "无净改动（已包含在当前树中）".to_string();
            let entry = json!({
                "run": r_id,
                "name": r_name,
                "status": "SKIPPED",
                "commit": "",
                "title": "",
                "acceptOk": Value::Null,
                "autoMerge": false,
                "reason": reason,
            });
            events::emit(
                &merge_run_id,
                &repo_str,
                "merge_item",
                json!({
                    "mergeRun": merge_run_id,
                    "sourceRun": r_id,
                    "name": r_name,
                    "status": "SKIPPED",
                    "reason": reason,
                }),
            );
            items.push(entry);
            continue;
        }

        if !protect_rules.is_empty() {
            let protect_hits: Vec<String> = committed_files
                .iter()
                .map(|(p, _)| p)
                .filter(|p| worktree::matches_rule(p, &protect_rules))
                .cloned()
                .collect();
            if !protect_hits.is_empty() {
                rollback_integration_worktree(&wt_path, &last_good_head, &managed);
                skipped_count += 1;
                let reason = format!("命中受保护路径：{}", protect_hits.join("、"));
                let entry = json!({
                    "run": r_id,
                    "name": r_name,
                    "status": "SKIPPED",
                    "commit": "",
                    "title": "",
                    "acceptOk": Value::Null,
                    "autoMerge": false,
                    "reason": reason,
                });
                events::emit(
                    &merge_run_id,
                    &repo_str,
                    "merge_item",
                    json!({
                        "mergeRun": merge_run_id,
                        "sourceRun": r_id,
                        "name": r_name,
                        "status": "SKIPPED",
                        "reason": reason,
                    }),
                );
                items.push(entry);
                continue;
            }
        }

        if let Some(ref signed_hit) = check.signed_hit {
            let intact = verify_signed_paths_after_merge(&wt_path, &last_good_head, signed_hit)
                .unwrap_or(false);
            if !intact {
                rollback_integration_worktree(&wt_path, &last_good_head, &managed);
                skipped_count += 1;
                let reason = format!(
                    "合入后模型可见文字改动与签字哈希不一致：{}",
                    signed_hit.paths.join("、")
                );
                let entry = json!({
                    "run": r_id,
                    "name": r_name,
                    "status": "SKIPPED",
                    "commit": "",
                    "title": "",
                    "acceptOk": Value::Null,
                    "autoMerge": false,
                    "reason": reason,
                });
                events::emit(
                    &merge_run_id,
                    &repo_str,
                    "merge_item",
                    json!({
                        "mergeRun": merge_run_id,
                        "sourceRun": r_id,
                        "name": r_name,
                        "status": "SKIPPED",
                        "reason": reason,
                    }),
                );
                items.push(entry);
                continue;
            }
        }

        // 5. Run repository acceptance via lane on the merged integration tree.
        let accept_ok = if !accept_cmd.is_empty() {
            let acc_log = merge_run.join(format!("accept-{}-{}.log", idx + 1, slug(&r_id)));
            let acc_label = format!("merge-accept {r_id}");
            match run_lane_command(
                &acc_label,
                &accept_cmd,
                &wt_path,
                &merge_run,
                &env_cfg,
                accept_timeout,
                &acc_log,
            ) {
                Ok((0, false)) => Some(true),
                Ok((code, timed)) => {
                    rollback_integration_worktree(&wt_path, &last_good_head, &managed);
                    skipped_count += 1;
                    let reason = if timed {
                        format!("验收超时（exit {code}）")
                    } else {
                        format!("测试 / 验收未通过（exit {code}）")
                    };
                    let entry = json!({
                        "run": r_id,
                        "name": r_name,
                        "status": "SKIPPED",
                        "commit": "",
                        "title": "",
                        "acceptOk": false,
                        "autoMerge": false,
                        "reason": reason,
                    });
                    events::emit(
                        &merge_run_id,
                        &repo_str,
                        "merge_item",
                        json!({
                            "mergeRun": merge_run_id,
                            "sourceRun": r_id,
                            "name": r_name,
                            "status": "SKIPPED",
                            "acceptOk": false,
                            "reason": reason,
                        }),
                    );
                    items.push(entry);
                    continue;
                }
                Err(e) => {
                    rollback_integration_worktree(&wt_path, &last_good_head, &managed);
                    skipped_count += 1;
                    let reason = format!("验收执行出错：{e}");
                    let entry = json!({
                        "run": r_id,
                        "name": r_name,
                        "status": "SKIPPED",
                        "commit": "",
                        "title": "",
                        "acceptOk": false,
                        "autoMerge": false,
                        "reason": reason,
                    });
                    events::emit(
                        &merge_run_id,
                        &repo_str,
                        "merge_item",
                        json!({
                            "mergeRun": merge_run_id,
                            "sourceRun": r_id,
                            "name": r_name,
                            "status": "SKIPPED",
                            "acceptOk": false,
                            "reason": reason,
                        }),
                    );
                    items.push(entry);
                    continue;
                }
            }
        } else {
            None
        };

        // Re-stage in case accept touched nothing or keep clean index; commit verbatim title + trailer.
        let orig_title = pick_original_title(r, &r_meta, &r_id);
        let commit_sha = match commit_staged(&wt_path, &orig_title, &r_id) {
            Ok(sha) => sha,
            Err(e) => {
                rollback_integration_worktree(&wt_path, &last_good_head, &managed);
                skipped_count += 1;
                let reason = format!("提交失败：{e}");
                let entry = json!({
                    "run": r_id,
                    "name": r_name,
                    "status": "SKIPPED",
                    "commit": "",
                    "title": orig_title,
                    "acceptOk": accept_ok,
                    "autoMerge": false,
                    "reason": reason,
                });
                events::emit(
                    &merge_run_id,
                    &repo_str,
                    "merge_item",
                    json!({
                        "mergeRun": merge_run_id,
                        "sourceRun": r_id,
                        "name": r_name,
                        "status": "SKIPPED",
                        "reason": reason,
                    }),
                );
                items.push(entry);
                continue;
            }
        };

        // Ensure working tree is clean after commit (in case accept left untracked artifacts).
        rollback_integration_worktree(&wt_path, &commit_sha, &managed);
        last_good_head = commit_sha.clone();

        let is_auto = auto_merge_eligible(
            &auto_merge_rules,
            &committed_files,
            &check.review_verdict,
            check.signed_hit.is_some(),
            false,
        );
        merged_count += 1;
        if is_auto {
            auto_merge_count += 1;
        }
        let mut notes_parts = Vec::new();
        if regenerated {
            notes_parts.push("已重新生成".to_string());
        }
        if !merge_res.numbered_warnings.is_empty() {
            notes_parts.push(format!(
                "编号前缀冲突提示({})",
                merge_res.numbered_warnings.len()
            ));
        }
        let note_str = notes_parts.join("；");
        let file_paths: Vec<String> = committed_files.iter().map(|(p, _)| p.clone()).collect();

        let entry = json!({
            "run": r_id,
            "name": r_name,
            "status": "MERGED",
            "commit": commit_sha,
            "title": orig_title,
            "acceptOk": accept_ok,
            "autoMerge": is_auto,
            "regenerated": regenerated,
            "files": file_paths,
            "reason": note_str,
        });
        events::emit(
            &merge_run_id,
            &repo_str,
            "merge_item",
            json!({
                "mergeRun": merge_run_id,
                "sourceRun": r_id,
                "name": r_name,
                "status": "MERGED",
                "commit": commit_sha,
                "title": orig_title,
                "acceptOk": accept_ok,
                "autoMerge": is_auto,
                "regenerated": regenerated,
            }),
        );
        items.push(entry);
    }

    // 6. Final `mergeAccept` once after all runs have been merged.
    let merge_accept_obj = if merged_count == 0 {
        json!({
            "command": merge_accept_cmd,
            "ok": false,
            "skipped": true,
            "exitCode": 0,
        })
    } else if merge_accept_cmd.is_empty() {
        json!({
            "command": "",
            "ok": true,
            "skipped": false,
            "exitCode": 0,
        })
    } else {
        let final_log = merge_run.join("merge-accept.log");
        let label = format!("mergeAccept {merge_run_id}");
        match run_lane_command(
            &label,
            &merge_accept_cmd,
            &wt_path,
            &merge_run,
            &env_cfg,
            accept_timeout,
            &final_log,
        ) {
            Ok((code, timed)) => {
                // Clean any build/test artifacts left untracked by mergeAccept so
                // `apply --keep-commits` sees a clean worktree.
                rollback_integration_worktree(&wt_path, &last_good_head, &managed);
                json!({
                    "command": merge_accept_cmd,
                    "ok": code == 0 && !timed,
                    "skipped": false,
                    "exitCode": code,
                    "timedOut": timed,
                    "log": final_log.to_string_lossy(),
                })
            }
            Err(e) => {
                rollback_integration_worktree(&wt_path, &last_good_head, &managed);
                json!({
                    "command": merge_accept_cmd,
                    "ok": false,
                    "skipped": false,
                    "exitCode": 1,
                    "error": e,
                    "log": final_log.to_string_lossy(),
                })
            }
        }
    };

    let merge_accept_ok = b(&merge_accept_obj, "ok");
    let finished_at = iso();
    let final_state = if merged_count > 0 && merge_accept_ok {
        "delivered"
    } else {
        "rejected"
    };

    let report_path = merge_run.join("merge-report.md");
    let report = json!({
        "mergeRun": merge_run_id,
        "name": merge_name,
        "repo": repo_str,
        "dir": merge_run.to_string_lossy(),
        "worktree": wt_path.to_string_lossy(),
        "reportPath": report_path.to_string_lossy(),
        "baseHead": base_head,
        "head": last_good_head,
        "startedAt": started_at,
        "finishedAt": finished_at,
        "state": final_state,
        "mergedCount": merged_count,
        "skippedCount": skipped_count,
        "autoMergeCount": auto_merge_count,
        "allAutoMerge": merged_count > 0 && auto_merge_count == merged_count,
        "mergeAccept": merge_accept_obj,
        "applied": false,
        "items": items,
    });

    let report_md = render_merge_report_md(&report);
    write(&report_path, &report_md)?;
    write(merge_run.join("result.md"), &report_md)?;
    write_json(merge_run.join("merge-report.json"), &report)?;

    // Record worktree changes and summary so `status`, `diff`, and `apply --keep-commits` work.
    let rec = changes::record(&mut meta, &merge_run);
    let mut sum = json!({
        "run": merge_run_id,
        "name": merge_name,
        "state": final_state,
        "mode": "write",
        "isMerge": true,
        "finishedAt": finished_at,
        "finishedEpoch": epoch() as i64,
        "merge": {
            "mergedCount": merged_count,
            "skippedCount": skipped_count,
            "autoMergeCount": auto_merge_count,
            "reportPath": report_path.to_string_lossy(),
        },
        "accept": {
            "command": merge_accept_cmd,
            "ok": merge_accept_ok,
            "exitCode": n(&merge_accept_obj, "exitCode"),
        },
    });
    if let Some((files, _)) = rec {
        let added: i64 = files.iter().map(|f| n(f, "added")).sum();
        let deleted: i64 = files.iter().map(|f| n(f, "deleted")).sum();
        sum["changes"] = json!({"files": files.len(), "added": added, "deleted": deleted});
        sum["files"] = json!(files);
    }
    meta["merge"]["mergedCount"] = json!(merged_count);
    meta["merge"]["skippedCount"] = json!(skipped_count);
    meta["merge"]["autoMergeCount"] = json!(auto_merge_count);
    meta["merge"]["head"] = json!(last_good_head);
    let has_conflict_skip = items.iter().any(|it| {
        it["conflicts"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    });
    let exit_code = if final_state == "delivered" {
        0
    } else if merged_count == 0 && has_conflict_skip {
        worktree::EXIT_CONFLICT
    } else {
        1
    };
    write_json(merge_run.join("meta.json"), &meta)?;
    write_json(merge_run.join("summary.json"), &sum)?;
    write(merge_run.join("exit_code"), format!("{exit_code}\n"))?;
    refs::pin_run(&merge_run, &meta);
    save_merge_state_record(&report);

    events::emit(
        &merge_run_id,
        &repo_str,
        "merge_done",
        json!({
            "mergeRun": merge_run_id,
            "name": merge_name,
            "state": final_state,
            "mergedCount": merged_count,
            "skippedCount": skipped_count,
            "autoMergeCount": auto_merge_count,
            "mergeAcceptOk": merge_accept_ok,
            "reportPath": report_path.to_string_lossy(),
        }),
    );

    if output::json_enabled() {
        println!("{report}");
    } else {
        print!("{report_md}");
        println!("报告文件：{}", report_path.display());
        for hint in crate::work::hints_for_merge(&merge_run_id, &report) {
            println!("{hint}");
        }
    }

    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_merge_rules_support_exclusions_and_add_only() {
        let rules: Vec<String> = vec![
            "docs/**".into(),
            "!docs/decision/**".into(),
            "apps/web/src/styles/**".into(),
            "add:**/tests/**".into(),
            "add:**/*_test.rs".into(),
            "!**/*.golden.md".into(),
        ];

        assert!(auto_merge_matches_file(
            &rules,
            "docs/guide.md",
            ChangeKind::Modified
        ));
        assert!(auto_merge_matches_file(
            &rules,
            "docs/sub/api.md",
            ChangeKind::Added
        ));
        assert!(!auto_merge_matches_file(
            &rules,
            "docs/decision/001-adr.md",
            ChangeKind::Added
        ));
        assert!(!auto_merge_matches_file(
            &rules,
            "docs/decision/001-adr.md",
            ChangeKind::Modified
        ));
        assert!(auto_merge_matches_file(
            &rules,
            "apps/web/src/styles/theme.css",
            ChangeKind::Modified
        ));
        assert!(auto_merge_matches_file(
            &rules,
            "crates/foo/tests/new_case.rs",
            ChangeKind::Added
        ));
        assert!(!auto_merge_matches_file(
            &rules,
            "crates/foo/tests/existing.rs",
            ChangeKind::Modified
        ));
        assert!(!auto_merge_matches_file(
            &rules,
            "crates/foo/tests/existing.rs",
            ChangeKind::Deleted
        ));
        assert!(auto_merge_matches_file(
            &rules,
            "crates/foo/src/bar_test.rs",
            ChangeKind::Added
        ));
        assert!(!auto_merge_matches_file(
            &rules,
            "crates/foo/tests/prompt.golden.md",
            ChangeKind::Added
        ));
        assert!(!auto_merge_matches_file(
            &rules,
            "crates/foo/src/lib.rs",
            ChangeKind::Modified
        ));
    }

    #[test]
    fn auto_merge_eligible_requires_pass_and_all_files_whitelisted() {
        let rules = vec!["docs/**".into(), "!docs/decision/**".into()];
        let files = vec![("docs/a.md".into(), ChangeKind::Modified)];
        assert!(auto_merge_eligible(&rules, &files, "PASS", false, false));
        assert!(!auto_merge_eligible(&rules, &files, "", false, false));
        assert!(!auto_merge_eligible(&rules, &files, "REWORK", false, false));
        assert!(!auto_merge_eligible(&rules, &files, "PASS", true, false));
        assert!(!auto_merge_eligible(&rules, &files, "PASS", false, true));
        assert!(!auto_merge_eligible(&[], &files, "PASS", false, false));

        let mixed = vec![
            ("docs/a.md".into(), ChangeKind::Modified),
            ("src/main.rs".into(), ChangeKind::Modified),
        ];
        assert!(!auto_merge_eligible(&rules, &mixed, "PASS", false, false));
    }

    #[test]
    fn validate_auto_merge_rejects_empty_patterns() {
        assert!(validate_auto_merge(Some(&json!(["docs/**", "!docs/decision/**", "add:**/*_test.rs"]))).is_ok());
        assert!(validate_auto_merge(Some(&json!([]))).is_ok());
        assert!(validate_auto_merge(Some(&json!(["!"]))).is_err());
        assert!(validate_auto_merge(Some(&json!(["add:"]))).is_err());
        assert!(validate_auto_merge(Some(&json!(["!add:"]))).is_err());
        assert!(validate_auto_merge(Some(&json!("docs/**"))).is_err());
    }
}
