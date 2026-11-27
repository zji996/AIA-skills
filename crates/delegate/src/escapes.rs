//! `delegate escapes` — track post-merge escaped defects via `Fixes-Run:` trailers
//! and summarize escape counts/rates by reviewer model, author model, and task class.

use crate::common::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::{Path, PathBuf};

const NO_REVIEW_LABEL: &str = "(无审查)";
const UNSPECIFIED_CLASS_LABEL: &str = "(未设)";

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunMetaInfo {
    source: &'static str,
    author_model: String,
    review_model: String,
    class: String,
}

#[derive(Debug, Default, Clone)]
struct Bucket {
    merged: usize,
    escaped: usize,
}

#[derive(Debug, Clone)]
struct CommitRecord {
    sha: String,
    delegate_runs: Vec<String>,
    fixes_runs: Vec<String>,
}

/// Extract `<key>: <run-id>` trailer values from a commit message body (case-insensitive key,
/// deduplicated per commit, whitespace-free run IDs).
pub fn extract_run_trailers(body: &str, trailer_key: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some((key, val)) = trimmed.split_once(':') {
            if key.trim().eq_ignore_ascii_case(trailer_key) {
                let run_id = val.trim();
                if !run_id.is_empty()
                    && !run_id.chars().any(char::is_whitespace)
                    && !out.iter().any(|x| x == run_id)
                {
                    out.push(run_id.to_string());
                }
            }
        }
    }
    out
}

fn non_empty_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn review_status_skipped(doc: &Value) -> bool {
    matches!(
        non_empty_str(doc, "status"),
        Some("skipped" | "disabled" | "cancelled")
    )
}

fn extract_review_model(meta: &Value, summary: &Value, review: &Value) -> String {
    if let Some(sr) = summary.get("review").filter(|v| v.is_object()) {
        if review_status_skipped(sr) {
            return NO_REVIEW_LABEL.to_string();
        }
        if let Some(m) = non_empty_str(sr, "model").or_else(|| non_empty_str(sr, "modelName")) {
            return m.to_string();
        }
    }
    if review.is_object() {
        if review_status_skipped(review) {
            return NO_REVIEW_LABEL.to_string();
        }
        if let Some(m) =
            non_empty_str(review, "model").or_else(|| non_empty_str(review, "modelName"))
        {
            return m.to_string();
        }
    }
    if let Some(rc) = meta.get("reviewConfig").filter(|v| v.is_object()) {
        if rc.get("enabled").and_then(Value::as_bool) != Some(false) {
            if let Some(m) = non_empty_str(rc, "model") {
                return m.to_string();
            }
        }
    }
    NO_REVIEW_LABEL.to_string()
}

fn read_run_meta_from_dir(dir: &Path, source: &'static str) -> Option<RunMetaInfo> {
    let meta_path = dir.join("meta.json");
    if !meta_path.is_file() {
        return None;
    }
    let meta = json(&meta_path);
    if !meta.is_object() {
        return None;
    }
    let summary = json(dir.join("summary.json"));
    let review = json(dir.join("review.json"));
    let author_model = non_empty_str(&meta, "model")
        .or_else(|| non_empty_str(&meta, "modelName"))
        .unwrap_or("unknown")
        .to_string();
    let review_model = extract_review_model(&meta, &summary, &review);
    let class = non_empty_str(&meta, "class")
        .unwrap_or(UNSPECIFIED_CLASS_LABEL)
        .to_string();
    Some(RunMetaInfo {
        source,
        author_model,
        review_model,
        class,
    })
}

/// Resolve run metadata for `run_id`, probing `<repo>/.local/run/delegate`,
/// `<DELEGATE_RUNS>` (if set), and `<cache_dir>/archive/<run_id>/record`.
fn resolve_run_meta(repo: &Path, run_id: &str) -> Option<RunMetaInfo> {
    let repo_runs = repo.join(".local/run/delegate");
    let mut candidates: Vec<(PathBuf, &'static str)> = vec![
        (repo_runs.join(run_id), "active"),
        (repo_runs.join(".applied").join(run_id), "applied"),
    ];
    if let Some(env_runs) =
        env::var_os("DELEGATE_RUNS").or_else(|| env::var_os("PI_DELEGATE_RUNS"))
    {
        if !env_runs.is_empty() {
            let root = PathBuf::from(env_runs);
            candidates.push((root.join(run_id), "active"));
            candidates.push((root.join(".applied").join(run_id), "applied"));
        }
    }
    candidates.push((
        cache_dir().join("archive").join(run_id).join("record"),
        "archive",
    ));
    candidates.push((repo.join(".local/run/pi").join(run_id), "active"));

    for (dir, source) in candidates {
        if let Some(info) = read_run_meta_from_dir(&dir, source) {
            return Some(info);
        }
    }
    None
}

fn scan_git_log(repo: &Path, since_epoch: Option<i64>) -> Res<Vec<CommitRecord>> {
    let mut cmd = git_command();
    cmd.arg("-C").arg(repo).arg("log");
    if let Some(ep) = since_epoch {
        cmd.arg(format!("--since=@{ep}"));
    }
    cmd.arg("--format=%x1e%H%x1f%B%x1e");
    let out = cmd
        .output()
        .map_err(|e| format!("failed to run git log: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("does not have any commits yet") {
            return Ok(Vec::new());
        }
        return Err(format!("git log failed: {}", stderr.trim()));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut records = Vec::new();
    for chunk in stdout.split('\x1e') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let Some((sha, body)) = chunk.split_once('\x1f') else {
            continue;
        };
        let sha = sha.trim().to_string();
        if sha.is_empty() {
            continue;
        }
        let delegate_runs = extract_run_trailers(body, "delegate-run");
        let fixes_runs = extract_run_trailers(body, "fixes-run");
        if !delegate_runs.is_empty() || !fixes_runs.is_empty() {
            records.push(CommitRecord {
                sha,
                delegate_runs,
                fixes_runs,
            });
        }
    }
    Ok(records)
}

fn sorted_rows(map: &BTreeMap<String, Bucket>) -> Vec<(String, Bucket)> {
    let mut rows: Vec<(String, Bucket)> =
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    rows.sort_by(|a, b| {
        b.1.escaped
            .cmp(&a.1.escaped)
            .then_with(|| b.1.merged.cmp(&a.1.merged))
            .then_with(|| a.0.cmp(&b.0))
    });
    rows
}

fn rate_value(b: &Bucket) -> Option<f64> {
    if b.merged == 0 {
        None
    } else {
        Some(b.escaped as f64 / b.merged as f64)
    }
}

fn format_rate(b: &Bucket) -> String {
    match rate_value(b) {
        Some(r) => format!("{:.1}%", r * 100.0),
        None => "-".to_string(),
    }
}

fn rows_to_json(rows: &[(String, Bucket)], key_name: &str) -> Vec<Value> {
    rows.iter()
        .map(|(name, b)| {
            let mut obj = serde_json::Map::new();
            obj.insert(key_name.to_string(), json!(name));
            obj.insert("merged".to_string(), json!(b.merged));
            obj.insert("escaped".to_string(), json!(b.escaped));
            obj.insert("rate".to_string(), json!(rate_value(b)));
            Value::Object(obj)
        })
        .collect()
}

fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| if c.is_ascii() { 1 } else { 2 })
        .sum()
}

fn pad_right(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

fn print_table(title: &str, col_header: &str, rows: &[(String, Bucket)]) {
    println!();
    println!("{title}：");
    if rows.is_empty() {
        println!("  (无记录)");
        return;
    }
    let name_w = rows
        .iter()
        .map(|(k, _)| display_width(k))
        .max()
        .unwrap_or(12)
        .max(display_width(col_header))
        .max(12);
    println!("  {}  {:>6}  {:>6}  {:>7}", pad_right(col_header, name_w), "合入", "漏出", "漏出率");
    for (name, b) in rows {
        println!(
            "  {}  {:>6}  {:>6}  {:>7}",
            pad_right(name, name_w),
            b.merged,
            b.escaped,
            format_rate(b),
        );
    }
}

pub fn command(args: &[String]) -> Res<i32> {
    let (pos, flags, kv) =
        crate::parse_simple("escapes", args, &["--json"], &["--since", "--repo"])?;
    if let Some(extra) = pos.first() {
        return Err(unrecognized_arg(
            extra,
            &["--json", "--since", "--repo"],
            "escapes",
        ));
    }
    let json_mode = crate::has(&flags, "--json");
    let since_raw = crate::value(&kv, "--since").unwrap_or("30d");
    let now_ep = epoch() as i64;
    let since_epoch = if since_raw.eq_ignore_ascii_case("all") {
        None
    } else {
        let secs = seconds(since_raw)?.round() as i64;
        if secs <= 0 {
            return Err(format!("invalid positive duration: {since_raw}"));
        }
        Some(now_ep.saturating_sub(secs))
    };

    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let repo = if let Some(p) = crate::value(&kv, "--repo") {
        let candidate = PathBuf::from(p);
        let abs = if candidate.is_absolute() {
            candidate
        } else {
            cwd.join(candidate)
        };
        git_top_fast(&abs).unwrap_or(abs)
    } else {
        git_top_fast(&cwd)
            .ok_or_else(|| "not inside a git repository (pass --repo PATH)".to_string())?
    };

    let commits = scan_git_log(&repo, since_epoch)?;

    let mut merged_runs: BTreeSet<String> = BTreeSet::new();
    let mut escaped_commits: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut fix_commits_count: usize = 0;

    for rec in &commits {
        for rid in &rec.delegate_runs {
            merged_runs.insert(rid.clone());
        }
        if !rec.fixes_runs.is_empty() {
            fix_commits_count += 1;
            for rid in &rec.fixes_runs {
                escaped_commits
                    .entry(rid.clone())
                    .or_default()
                    .push(rec.sha.clone());
            }
        }
    }

    let mut by_reviewer: BTreeMap<String, Bucket> = BTreeMap::new();
    let mut by_author: BTreeMap<String, Bucket> = BTreeMap::new();
    let mut by_class: BTreeMap<String, Bucket> = BTreeMap::new();

    let mut meta_cache: BTreeMap<String, Option<RunMetaInfo>> = BTreeMap::new();
    let mut unknown_merged_ids: Vec<String> = Vec::new();
    let mut unknown_escaped_ids: Vec<String> = Vec::new();

    for rid in &merged_runs {
        let info = meta_cache
            .entry(rid.clone())
            .or_insert_with(|| resolve_run_meta(&repo, rid))
            .clone();
        if let Some(m) = info {
            by_reviewer.entry(m.review_model).or_default().merged += 1;
            by_author.entry(m.author_model).or_default().merged += 1;
            by_class.entry(m.class).or_default().merged += 1;
        } else {
            unknown_merged_ids.push(rid.clone());
        }
    }

    let mut escape_details: Vec<Value> = Vec::new();
    for (rid, shas) in &escaped_commits {
        let info = meta_cache
            .entry(rid.clone())
            .or_insert_with(|| resolve_run_meta(&repo, rid))
            .clone();
        let merged_in_window = merged_runs.contains(rid);
        if let Some(m) = info {
            by_reviewer
                .entry(m.review_model.clone())
                .or_default()
                .escaped += 1;
            by_author
                .entry(m.author_model.clone())
                .or_default()
                .escaped += 1;
            by_class.entry(m.class.clone()).or_default().escaped += 1;
            escape_details.push(json!({
                "run": rid,
                "found": true,
                "source": m.source,
                "authorModel": m.author_model,
                "reviewModel": m.review_model,
                "class": m.class,
                "mergedInWindow": merged_in_window,
                "fixCommits": shas,
            }));
        } else {
            unknown_escaped_ids.push(rid.clone());
            escape_details.push(json!({
                "run": rid,
                "found": false,
                "source": "unknown",
                "authorModel": Value::Null,
                "reviewModel": Value::Null,
                "class": Value::Null,
                "mergedInWindow": merged_in_window,
                "fixCommits": shas,
            }));
        }
    }

    let reviewer_rows = sorted_rows(&by_reviewer);
    let author_rows = sorted_rows(&by_author);
    let class_rows = sorted_rows(&by_class);

    if json_mode {
        let doc = json!({
            "repo": repo,
            "since": since_raw,
            "sinceEpoch": since_epoch,
            "generatedEpoch": now_ep,
            "mergedRuns": merged_runs.len(),
            "escapedRuns": escaped_commits.len(),
            "fixCommits": fix_commits_count,
            "unknownRuns": unknown_escaped_ids.len(),
            "unknownRunIds": unknown_escaped_ids,
            "unknownMergedRuns": unknown_merged_ids.len(),
            "unknownMergedRunIds": unknown_merged_ids,
            "byReviewer": rows_to_json(&reviewer_rows, "model"),
            "byAuthor": rows_to_json(&author_rows, "model"),
            "byClass": rows_to_json(&class_rows, "class"),
            "escapes": escape_details,
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        return Ok(0);
    }

    let repo_label = repo
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("repo");
    let window_label = if since_raw.eq_ignore_ascii_case("all") {
        "全部".to_string()
    } else {
        format!("近 {since_raw}")
    };
    println!("漏网缺陷统计（{window_label}，仓库：{repo_label}）");
    let extra_unknown_merged = if unknown_merged_ids.is_empty() {
        String::new()
    } else {
        format!("，未知 meta 合入：{}", unknown_merged_ids.len())
    };
    println!(
        "合入委派：{}  被 Fixes-Run 指到：{}（其中未知 run：{}{}）",
        merged_runs.len(),
        escaped_commits.len(),
        unknown_escaped_ids.len(),
        extra_unknown_merged,
    );

    print_table("按审查模型", "模型", &reviewer_rows);
    print_table("按写手模型", "模型", &author_rows);
    print_table("按类别", "类别", &class_rows);

    if !unknown_escaped_ids.is_empty() {
        println!();
        let preview: Vec<&str> = unknown_escaped_ids
            .iter()
            .take(10)
            .map(String::as_str)
            .collect();
        let suffix = if unknown_escaped_ids.len() > 10 {
            format!(" …（共 {} 个）", unknown_escaped_ids.len())
        } else {
            String::new()
        };
        println!(
            "未知 run（{}）：{}{}",
            unknown_escaped_ids.len(),
            preview.join(", "),
            suffix
        );
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_trailers_handles_multiple_case_and_dedup() {
        let body = "fix: edge case\n\nDetails here.\n\nFixes-Run: run-1\nfixes-run: run-2\nFixes-Run: run-1\nDelegate-Run: run-fix\nFixes-Run:   \nFixes-Run: invalid with spaces\n";
        assert_eq!(
            extract_run_trailers(body, "fixes-run"),
            vec!["run-1".to_string(), "run-2".to_string()]
        );
        assert_eq!(
            extract_run_trailers(body, "delegate-run"),
            vec!["run-fix".to_string()]
        );
    }

    #[test]
    fn extract_review_model_priority_and_skipped() {
        let meta = json!({"reviewConfig": {"enabled": true, "model": "cfg-model"}});
        let summary = json!({"review": {"status": "done", "model": "sum-model"}});
        let review = json!({"status": "done", "model": "rev-model"});
        assert_eq!(extract_review_model(&meta, &summary, &review), "sum-model");

        let empty = Value::Null;
        assert_eq!(extract_review_model(&meta, &empty, &review), "rev-model");
        assert_eq!(extract_review_model(&meta, &empty, &empty), "cfg-model");

        let skipped = json!({"review": {"status": "skipped"}});
        assert_eq!(
            extract_review_model(&meta, &skipped, &empty),
            NO_REVIEW_LABEL
        );

        let disabled = json!({"reviewConfig": {"enabled": false, "model": "cfg-model"}});
        assert_eq!(
            extract_review_model(&disabled, &empty, &empty),
            NO_REVIEW_LABEL
        );
    }
}
