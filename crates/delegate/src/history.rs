//! Append-only per-repository history index, progressive-disclosure `history`/`show`,
//! backfill of existing runs/archives, and pre-dispatch deduplication hints.
use crate::common::*;
use crate::runs;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub const MAX_HISTORY_LINE_BYTES: usize = 2000;
pub const PROMPT_FIRST_LINE_CHARS: usize = 120;
pub const PROMPT_EXCERPT_CHARS: usize = 300;
pub const CONCLUSION_MAX_CHARS: usize = 600;

pub const DEDUP_WINDOW_DAYS: u64 = 14;
pub const DEDUP_SIMILARITY_THRESHOLD: f64 = 0.30;
pub const DEDUP_NAME_THRESHOLD: f64 = 0.50;
pub const DEDUP_MIN_SHARED: usize = 2;
pub const DEDUP_MAX_HINTS: usize = 3;

#[derive(Clone, Debug, Default)]
pub struct HistoryEntry {
    pub run: String,
    pub name: String,
    pub mode: String,
    pub started_at: String,
    pub finished_at: String,
    pub agent: String,
    pub tier: String,
    pub model: String,
    pub model_name: String,
    pub state: String,
    pub question: bool,
    pub policy_refused: bool,
    pub parent: String,
    pub children: Vec<String>,
    pub prompt_first_line: String,
    pub prompt_excerpt: String,
    pub conclusion: String,
    pub files_by_dir: BTreeMap<String, usize>,
    pub files: Vec<String>,
    pub changes_files: usize,
    pub added: i64,
    pub deleted: i64,
    pub accept_line: String,
    pub applied_at: String,
    pub applied_head: String,
    pub applied_by: String,
    pub tokens: Value,
    pub tokens_delta: Value,
    pub elapsed_seconds: i64,
    pub rework_count: u64,
    pub archive: String,
    pub cleaned: bool,
    pub source_root: String,
}

impl HistoryEntry {
    /// Canonical per-run incremental token usage (`tokensDelta` when present, else `tokens`).
    pub fn effective_tokens(&self) -> &Value {
        if self.tokens_delta.as_object().is_some_and(|o| !o.is_empty()) {
            &self.tokens_delta
        } else {
            &self.tokens
        }
    }

    fn sort_ts(&self) -> i64 {
        iso_epoch(&self.started_at)
            .or_else(|| iso_epoch(&self.finished_at))
            .unwrap_or(0)
    }

    fn recent_ts(&self) -> i64 {
        iso_epoch(&self.finished_at)
            .or_else(|| iso_epoch(&self.started_at))
            .unwrap_or(0)
    }

    fn to_json(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("run".into(), json!(self.run));
        obj.insert("name".into(), json!(self.name));
        if !self.mode.is_empty() {
            obj.insert("mode".into(), json!(self.mode));
        }
        if !self.started_at.is_empty() {
            obj.insert("startedAt".into(), json!(self.started_at));
        }
        if !self.finished_at.is_empty() {
            obj.insert("finishedAt".into(), json!(self.finished_at));
        }
        if !self.agent.is_empty() {
            obj.insert("agent".into(), json!(self.agent));
        }
        if !self.tier.is_empty() {
            obj.insert("tier".into(), json!(self.tier));
        }
        if !self.model.is_empty() {
            obj.insert("model".into(), json!(self.model));
        }
        if !self.model_name.is_empty() {
            obj.insert("modelName".into(), json!(self.model_name));
        }
        obj.insert("state".into(), json!(self.state));
        if self.question {
            obj.insert("question".into(), json!(true));
        }
        if self.policy_refused {
            obj.insert("policyRefused".into(), json!(true));
        }
        if !self.parent.is_empty() {
            obj.insert("parent".into(), json!(self.parent));
        }
        if !self.children.is_empty() {
            obj.insert("children".into(), json!(self.children));
        }
        if !self.prompt_first_line.is_empty() {
            obj.insert("promptFirstLine".into(), json!(self.prompt_first_line));
        }
        if !self.prompt_excerpt.is_empty() {
            obj.insert("promptExcerpt".into(), json!(self.prompt_excerpt));
        }
        if !self.conclusion.is_empty() {
            obj.insert("conclusion".into(), json!(self.conclusion));
        }
        if !self.files_by_dir.is_empty() {
            obj.insert("filesByDir".into(), json!(self.files_by_dir));
        }
        if !self.files.is_empty() {
            obj.insert("files".into(), json!(self.files));
        }
        if self.changes_files > 0 || self.added > 0 || self.deleted > 0 {
            obj.insert(
                "changes".into(),
                json!({"files": self.changes_files, "added": self.added, "deleted": self.deleted}),
            );
        }
        if !self.accept_line.is_empty() {
            obj.insert("acceptLine".into(), json!(self.accept_line));
        }
        if !self.applied_at.is_empty() {
            obj.insert("appliedAt".into(), json!(self.applied_at));
        }
        if !self.applied_head.is_empty() {
            obj.insert("appliedHead".into(), json!(self.applied_head));
        }
        if !self.applied_by.is_empty() {
            obj.insert("appliedBy".into(), json!(self.applied_by));
        }
        if self.tokens.is_object() && !self.tokens.as_object().unwrap().is_empty() {
            obj.insert("tokens".into(), self.tokens.clone());
        }
        if self.tokens_delta.is_object() && !self.tokens_delta.as_object().unwrap().is_empty() {
            obj.insert("tokensDelta".into(), self.tokens_delta.clone());
        }
        if self.elapsed_seconds > 0 {
            obj.insert("elapsedSeconds".into(), json!(self.elapsed_seconds));
        }
        if self.rework_count > 0 {
            obj.insert("reworkCount".into(), json!(self.rework_count));
        }
        if !self.archive.is_empty() {
            obj.insert("archive".into(), json!(self.archive));
        }
        if self.cleaned {
            obj.insert("cleaned".into(), json!(true));
        }
        if !self.source_root.is_empty() {
            obj.insert("sourceRoot".into(), json!(self.source_root));
        }
        Value::Object(obj)
    }
}

fn explicit_runs_env() -> Option<PathBuf> {
    env::var_os("DELEGATE_RUNS")
        .or_else(|| env::var_os("PI_DELEGATE_RUNS"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn canonical_string(path: &Path) -> String {
    fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

pub fn current_history_key() -> String {
    if let Some(root) = explicit_runs_env() {
        return format!("runs:{}", canonical_string(&root));
    }
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::launch::repo_key(&cwd)
}

pub fn history_key_for_run(meta: &Value) -> String {
    if let Some(root) = explicit_runs_env() {
        return format!("runs:{}", canonical_string(&root));
    }
    let key = crate::launch::repo_key_for_meta(meta);
    if key.is_empty() {
        current_history_key()
    } else {
        key
    }
}

pub fn history_dir_for_key(key: &str) -> PathBuf {
    let raw = key.strip_prefix("runs:").unwrap_or(key);
    let path = Path::new(raw);
    let candidate = if path.file_name().is_some_and(|n| n == ".git") {
        path.parent().and_then(Path::file_name)
    } else {
        path.file_name()
    };
    let mut slug: String = candidate
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(24)
        .collect();
    slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        slug = "repo".into();
    }
    let digest = sha1_smol::Sha1::from(key.as_bytes()).digest().to_string();
    state_dir()
        .join("history")
        .join(format!("{slug}-{}", &digest[..12]))
}

pub fn history_path_for_key(key: &str) -> PathBuf {
    history_dir_for_key(key).join("history.jsonl")
}

fn append_jsonl(key: &str, value: &Value) -> Res<()> {
    let dir = history_dir_for_key(key);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let _guard = crate::locks::acquire(crate::locks::LockKind::History, &dir.join("history.lock"), true).map_err(|e| e.to_string())?;
    let line = serde_json::to_string(value).map_err(|e| e.to_string())?;
    append(dir.join("history.jsonl"), &format!("{line}\n"))
}

pub fn clip_chars(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let out: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{}…", out.trim_end())
    } else {
        out
    }
}

pub fn strip_delegate_prompt_suffixes(raw: &str, standing: &str) -> String {
    let mut end = raw.len();
    let markers = [
        "\n\nCompletion criterion:",
        "\n\n完成标准：",
        "\n\n只读任务：",
        "\n\n只在当前工作目录内改文件",
        "\n\nProtected paths",
        "\n\n受保护路径",
        "\n\n本次开放 / Open for this task only:",
        "\n\n生成物保护例外 / Generated protection exception:",
        "\n\n未提交的编号文件已占用这些编号",
        "\n\n答复不超过 ",
        "\n\n可读输入：",
        "\n\n验收命令冲突说明：",
        "\n\nWorktree boundary:",
        "\n\nOnly modify files inside",
    ];
    for marker in markers {
        if let Some(pos) = raw[..end].find(marker) {
            end = end.min(pos);
        }
    }
    if !standing.trim().is_empty() {
        let needle = format!("\n\n{}", standing.trim());
        if let Some(pos) = raw[..end].find(&needle) {
            end = end.min(pos);
        }
    }
    raw[..end].trim().to_string()
}

pub fn extract_user_prompt(run: &Path, meta: &Value) -> String {
    let raw = read(run.join("prompt.md"));
    if raw.is_empty() {
        return String::new();
    }
    if let Some(len) = meta["userPromptLen"].as_u64() {
        let len = len as usize;
        if len <= raw.len() && raw.is_char_boundary(len) {
            return raw[..len].trim().to_string();
        }
    }
    strip_delegate_prompt_suffixes(&raw, s(meta, "standing"))
}

pub fn prompt_first_line(prompt: &str) -> String {
    let first = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    clip_chars(first, PROMPT_FIRST_LINE_CHARS)
}

pub fn prompt_excerpt(prompt: &str) -> String {
    clip_chars(prompt.trim(), PROMPT_EXCERPT_CHARS)
}

fn is_heading_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ')
}

/// Deterministically extract the conclusion segment from a colleague's answer:
/// the first markdown heading section if the answer starts with a heading, else the first ~600 chars.
pub fn extract_conclusion(result: &str) -> String {
    let normalized = result.replace("\r\n", "\n");
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    if is_heading_line(lines[0]) {
        let mut out = Vec::new();
        let mut in_fence = false;
        let mut has_body = false;
        for (i, &line) in lines.iter().enumerate() {
            let t = line.trim_start();
            if t.starts_with("```") {
                in_fence = !in_fence;
            }
            if i > 0 && !in_fence && is_heading_line(line) {
                if has_body {
                    break;
                }
                // Title heading immediately followed by a sub-heading (e.g. `# Report\n\n## Conclusion`).
                out.push(line);
                continue;
            }
            if i > 0 && !line.trim().is_empty() && !is_heading_line(line) {
                has_body = true;
            }
            out.push(line);
        }
        return clip_chars(out.join("\n").trim(), CONCLUSION_MAX_CHARS);
    }
    clip_chars(trimmed, CONCLUSION_MAX_CHARS)
}

pub fn files_and_dirs(run: &Path, meta: &Value, sum: &Value) -> (Vec<String>, BTreeMap<String, usize>) {
    let top = if !s(&meta["worktree"], "source").is_empty() {
        s(&meta["worktree"], "source")
    } else if !s(meta, "top").is_empty() {
        s(meta, "top")
    } else {
        s(meta, "workdir")
    };
    let mut paths = strings(&sum["files"]);
    if paths.is_empty() {
        let rec = json(run.join("changes.json"));
        if let Some(arr) = rec["files"].as_array() {
            for item in arr {
                if let Some(p) = item.as_str() {
                    paths.push(p.to_string());
                } else if let Some(p) = item["path"].as_str() {
                    paths.push(p.to_string());
                }
            }
        }
    }
    let mut rels = Vec::new();
    let mut dirs = BTreeMap::<String, usize>::new();
    for path in paths {
        let rel = repository_relative(&path, top);
        let p = Path::new(&rel);
        let parent = p.parent().unwrap_or(Path::new("."));
        let dir = parent
            .components()
            .take(2)
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        *dirs
            .entry(if dir.is_empty() { ".".into() } else { dir })
            .or_default() += 1;
        rels.push(rel);
    }
    rels.sort();
    rels.dedup();
    (rels, dirs)
}

pub fn accept_summary_line(meta: &Value, sum: &Value) -> String {
    let cmd = s(meta, "accept");
    if sum["accept"].is_object() {
        let status = if b(&sum["accept"], "notExecuted") {
            "验收未执行（排队/基础设施）"
        } else if b(sum, "infrastructureFailure") {
            "验收基础设施失败"
        } else if b(&sum["accept"], "ok") {
            "验收通过"
        } else {
            "验收失败"
        };
        if cmd.is_empty() {
            status.to_string()
        } else {
            format!("{status}（{}）", clip_chars(cmd, 80))
        }
    } else if !cmd.is_empty() {
        format!("验收未运行（{}）", clip_chars(cmd, 80))
    } else {
        "未设验收".to_string()
    }
}

pub fn source_root_for_meta(meta: &Value) -> String {
    for candidate in [
        s(&meta["worktree"], "source"),
        s(meta, "sourceBuildRoot"),
        s(meta, "top"),
        s(meta, "workdir"),
    ] {
        if !candidate.is_empty() {
            if let Some(top) = git_top(Path::new(candidate)) {
                return top.to_string_lossy().into_owned();
            }
            return candidate.to_string();
        }
    }
    String::new()
}

fn compact_finish_record(mut rec: Value) -> Value {
    let check_len = |v: &Value| serde_json::to_string(v).map(|s| s.len()).unwrap_or(usize::MAX);
    if check_len(&rec) <= MAX_HISTORY_LINE_BYTES {
        return rec;
    }
    for cap in [12usize, 6, 3, 0] {
        if check_len(&rec) <= MAX_HISTORY_LINE_BYTES {
            return rec;
        }
        if let Some(arr) = rec.get("files").and_then(Value::as_array) {
            if arr.len() > cap {
                if cap == 0 {
                    rec.as_object_mut().unwrap().remove("files");
                } else {
                    rec["files"] = json!(arr[..cap]);
                }
            }
        }
    }
    for (field, targets) in [
        ("conclusion", &[400usize, 260, 160, 80][..]),
        ("promptExcerpt", &[200usize, 120, 60][..]),
        ("promptFirstLine", &[80usize, 40][..]),
        ("acceptLine", &[80usize, 40][..]),
    ] {
        for &limit in targets {
            if check_len(&rec) <= MAX_HISTORY_LINE_BYTES {
                return rec;
            }
            if let Some(val) = rec.get(field).and_then(Value::as_str).map(str::to_string) {
                if val.chars().count() > limit {
                    rec[field] = json!(clip_chars(&val, limit));
                }
            }
        }
    }
    // Hard byte-safety guard for extreme metadata cases.
    for field in ["conclusion", "promptExcerpt", "filesByDir"] {
        if check_len(&rec) <= MAX_HISTORY_LINE_BYTES {
            break;
        }
        if field == "filesByDir" {
            if let Some(obj) = rec.get("filesByDir").and_then(Value::as_object) {
                let mut pairs: Vec<_> = obj.iter().collect();
                pairs.sort_by_key(|b| std::cmp::Reverse(b.1.as_u64()));
                let kept: serde_json::Map<String, Value> = pairs
                    .into_iter()
                    .take(5)
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                rec["filesByDir"] = Value::Object(kept);
            }
        } else if let Some(val) = rec.get(field).and_then(Value::as_str).map(str::to_string) {
            let trimmed = clip_chars(&val, 40);
            rec[field] = json!(trimmed);
        }
    }
    rec
}

fn build_finish_record(run: &Path) -> Option<(String, Value)> {
    let meta = json(run.join("meta.json"));
    if !meta.is_object() {
        return None;
    }
    let sum = json(run.join("summary.json"));
    let run_id = if !s(&meta, "run").is_empty() {
        s(&meta, "run").to_string()
    } else {
        run.file_name()?.to_string_lossy().into_owned()
    };
    let state = if !s(&sum, "state").is_empty() {
        s(&sum, "state").to_string()
    } else {
        runs::state(run)
    };
    let prompt = extract_user_prompt(run, &meta);
    let first_line = prompt_first_line(&prompt);
    let excerpt = prompt_excerpt(&prompt);
    let result_text = read(run.join("result.md"));
    let conclusion = extract_conclusion(&result_text);
    let (mut files, files_by_dir) = files_and_dirs(run, &meta, &sum);
    if files.len() > 24 {
        files.truncate(24);
    }
    let changes = &sum["changes"];
    let changes_files = changes["files"]
        .as_u64()
        .map(|n| n as usize)
        .unwrap_or(files.len());
    let added = n(changes, "added");
    let deleted = n(changes, "deleted");
    let applied = json(run.join(".applied"));
    let applied_at = if !s(&meta, "appliedAt").is_empty() {
        s(&meta, "appliedAt").to_string()
    } else {
        s(&applied, "at").to_string()
    };
    let applied_head = if !s(&meta, "appliedHead").is_empty() {
        s(&meta, "appliedHead").to_string()
    } else {
        s(&applied, "head").to_string()
    };
    let applied_by = if !applied_at.is_empty() {
        let by = s(&applied, "appliedBy");
        if by.is_empty() {
            "delegate".to_string()
        } else {
            by.to_string()
        }
    } else {
        String::new()
    };
    let archive_manifest = json(run.join("archive.json"));
    let archive_path = if !s(&archive_manifest, "path").is_empty() {
        s(&archive_manifest, "path").to_string()
    } else if crate::archive::is_record(run) {
        run.parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let cleaned = crate::archive::is_record(run);
    let started_at = s(&meta, "startedAt").to_string();
    let finished_at = if !s(&sum, "finishedAt").is_empty() {
        s(&sum, "finishedAt").to_string()
    } else {
        iso()
    };
    let model = if !s(&sum, "model").is_empty() {
        s(&sum, "model").to_string()
    } else {
        s(&meta, "model").to_string()
    };
    let rework_count = crate::launch::rework_count(run);
    let mut rec = serde_json::Map::new();
    rec.insert("event".into(), json!("finish"));
    rec.insert("at".into(), json!(finished_at));
    rec.insert("run".into(), json!(run_id));
    rec.insert("name".into(), json!(s(&meta, "name")));
    if !s(&meta, "mode").is_empty() {
        rec.insert("mode".into(), json!(s(&meta, "mode")));
    }
    if !started_at.is_empty() {
        rec.insert("startedAt".into(), json!(started_at));
    }
    rec.insert("finishedAt".into(), json!(finished_at));
    if !s(&meta, "agent").is_empty() {
        rec.insert("agent".into(), json!(s(&meta, "agent")));
    }
    if !s(&meta, "tier").is_empty() {
        rec.insert("tier".into(), json!(s(&meta, "tier")));
    }
    if !model.is_empty() {
        rec.insert("model".into(), json!(model));
    }
    if !s(&meta, "modelName").is_empty() {
        rec.insert("modelName".into(), json!(s(&meta, "modelName")));
    }
    rec.insert("state".into(), json!(state));
    if b(&sum, "question") {
        rec.insert("question".into(), json!(true));
    }
    if b(&sum, "policyRefused") {
        rec.insert("policyRefused".into(), json!(true));
    }
    if !s(&meta, "parent").is_empty() {
        rec.insert("parent".into(), json!(s(&meta, "parent")));
    }
    if !first_line.is_empty() {
        rec.insert("promptFirstLine".into(), json!(first_line));
    }
    if !excerpt.is_empty() {
        rec.insert("promptExcerpt".into(), json!(excerpt));
    }
    if !conclusion.is_empty() {
        rec.insert("conclusion".into(), json!(conclusion));
    }
    if !files_by_dir.is_empty() {
        rec.insert("filesByDir".into(), json!(files_by_dir));
    }
    if !files.is_empty() {
        rec.insert("files".into(), json!(files));
    }
    if changes_files > 0 || added > 0 || deleted > 0 {
        rec.insert(
            "changes".into(),
            json!({"files": changes_files, "added": added, "deleted": deleted}),
        );
    }
    rec.insert("acceptLine".into(), json!(accept_summary_line(&meta, &sum)));
    if !applied_at.is_empty() {
        rec.insert("appliedAt".into(), json!(applied_at));
    }
    if !applied_head.is_empty() {
        rec.insert("appliedHead".into(), json!(applied_head));
    }
    if !applied_by.is_empty() {
        rec.insert("appliedBy".into(), json!(applied_by));
    }
    if sum["tokens"].is_object() && !sum["tokens"].as_object().unwrap().is_empty() {
        rec.insert("tokens".into(), sum["tokens"].clone());
    }
    if sum["tokensDelta"].is_object() && !sum["tokensDelta"].as_object().unwrap().is_empty() {
        rec.insert("tokensDelta".into(), sum["tokensDelta"].clone());
    }
    if n(&sum, "elapsedSeconds") > 0 {
        rec.insert("elapsedSeconds".into(), json!(n(&sum, "elapsedSeconds")));
    }
    if rework_count > 0 {
        rec.insert("reworkCount".into(), json!(rework_count));
    }
    if !archive_path.is_empty() {
        rec.insert("archive".into(), json!(archive_path));
    }
    if cleaned {
        rec.insert("cleaned".into(), json!(true));
    }
    let src_root = source_root_for_meta(&meta);
    if !src_root.is_empty() {
        rec.insert("sourceRoot".into(), json!(src_root));
    }
    let key = history_key_for_run(&meta);
    Some((key, compact_finish_record(Value::Object(rec))))
}

/// Append a finish record when a run reaches a terminal state. Best-effort: never fails the run.
pub fn record_finish(run: &Path) {
    let Some((key, rec)) = build_finish_record(run) else {
        return;
    };
    if let Err(err) = append_jsonl(&key, &rec) {
        eprintln!("delegate: history finish write failed: {err}");
    }
}

pub fn record_apply(
    run: &Path,
    meta: &Value,
    applied_at: &str,
    applied_head: &str,
    applied_by: &str,
    archive: Option<&Path>,
) {
    let run_id = if !s(meta, "run").is_empty() {
        s(meta, "run").to_string()
    } else {
        run.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    let key = history_key_for_run(meta);
    let mut ev = json!({
        "event": "apply",
        "at": iso(),
        "run": run_id,
        "appliedAt": applied_at,
        "appliedBy": applied_by,
    });
    if !applied_head.is_empty() {
        ev["appliedHead"] = json!(applied_head);
    }
    if let Some(arch) = archive {
        ev["archive"] = json!(arch.to_string_lossy());
    }
    if let Err(err) = append_jsonl(&key, &ev) {
        eprintln!("delegate: history apply write failed: {err}");
    }
}

pub fn record_clean(run: &Path, meta: &Value, archive: Option<&Path>) {
    let run_id = if !s(meta, "run").is_empty() {
        s(meta, "run").to_string()
    } else {
        run.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    let key = history_key_for_run(meta);
    let mut ev = json!({
        "event": "clean",
        "at": iso(),
        "run": run_id,
        "cleaned": true,
    });
    if let Some(arch) = archive {
        ev["archive"] = json!(arch.to_string_lossy());
    }
    if let Err(err) = append_jsonl(&key, &ev) {
        eprintln!("delegate: history clean write failed: {err}");
    }
}

pub fn record_reply(parent_meta: &Value, child_run: &Path, rework_count: u64) {
    let parent_id = s(parent_meta, "run");
    if parent_id.is_empty() {
        return;
    }
    let child_id = child_run
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let key = history_key_for_run(parent_meta);
    let mut ev = json!({
        "event": "reply",
        "at": iso(),
        "run": parent_id,
        "child": child_id,
    });
    if rework_count > 0 {
        ev["reworkCount"] = json!(rework_count);
    }
    if let Err(err) = append_jsonl(&key, &ev) {
        eprintln!("delegate: history reply write failed: {err}");
    }
}

/// Read and fold all lines in a repository's `history.jsonl` into entries sorted oldest-first.
pub fn load_entries(key: &str) -> Vec<HistoryEntry> {
    let path = history_path_for_key(key);
    load_entries_from_text(&read(path))
}

/// Strictly read-only `load_entries` that also folds un-backfilled legacy runs/archives in memory
/// when `.backfilled-v1` has not yet been written (zero locks, zero disk writes).
pub fn load_entries_readonly(key: &str) -> Vec<HistoryEntry> {
    let dir = history_dir_for_key(key);
    let path = dir.join("history.jsonl");
    let mut text = read(&path);
    if !dir.join(".backfilled-v1").is_file() && key == current_history_key() {
        let existing: HashSet<String> = load_entries_from_text(&text)
            .into_iter()
            .map(|e| e.run)
            .collect();
        let mut seen = existing;
        for run in runs::all_runs().into_iter().chain(runs::archived_runs()) {
            if !run.join("meta.json").is_file() || runs::active(&runs::state(&run)) {
                continue;
            }
            let meta = json(run.join("meta.json"));
            let id = if !s(&meta, "run").is_empty() {
                s(&meta, "run").to_string()
            } else {
                run.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            };
            if id.is_empty() || !seen.insert(id) {
                continue;
            }
            if let Some((_, rec)) = build_finish_record(&run) {
                if let Ok(line) = serde_json::to_string(&rec) {
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(&line);
                    text.push('\n');
                }
            }
        }
    }
    load_entries_from_text(&text)
}

fn load_entries_from_text(content: &str) -> Vec<HistoryEntry> {
    let mut map: HashMap<String, HistoryEntry> = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let run_id = s(&v, "run").to_string();
        if run_id.is_empty() {
            continue;
        }
        let event = s(&v, "event");
        match event {
            "" | "finish" => {
                let entry = map.entry(run_id.clone()).or_default();
                entry.run = run_id;
                if !s(&v, "name").is_empty() {
                    entry.name = s(&v, "name").to_string();
                }
                if !s(&v, "mode").is_empty() {
                    entry.mode = s(&v, "mode").to_string();
                }
                if !s(&v, "startedAt").is_empty() {
                    entry.started_at = s(&v, "startedAt").to_string();
                }
                if !s(&v, "finishedAt").is_empty() {
                    entry.finished_at = s(&v, "finishedAt").to_string();
                }
                if !s(&v, "agent").is_empty() {
                    entry.agent = s(&v, "agent").to_string();
                }
                if !s(&v, "tier").is_empty() {
                    entry.tier = s(&v, "tier").to_string();
                }
                if !s(&v, "model").is_empty() {
                    entry.model = s(&v, "model").to_string();
                }
                if !s(&v, "modelName").is_empty() {
                    entry.model_name = s(&v, "modelName").to_string();
                }
                if !s(&v, "state").is_empty() {
                    entry.state = s(&v, "state").to_string();
                }
                entry.question = b(&v, "question");
                entry.policy_refused = b(&v, "policyRefused");
                if !s(&v, "parent").is_empty() {
                    entry.parent = s(&v, "parent").to_string();
                }
                if !s(&v, "promptFirstLine").is_empty() {
                    entry.prompt_first_line = s(&v, "promptFirstLine").to_string();
                }
                if !s(&v, "promptExcerpt").is_empty() {
                    entry.prompt_excerpt = s(&v, "promptExcerpt").to_string();
                }
                if !s(&v, "conclusion").is_empty() {
                    entry.conclusion = s(&v, "conclusion").to_string();
                }
                if let Some(obj) = v["filesByDir"].as_object() {
                    entry.files_by_dir.clear();
                    for (k, c) in obj {
                        if let Some(cnt) = c.as_u64() {
                            entry.files_by_dir.insert(k.clone(), cnt as usize);
                        }
                    }
                }
                let f = strings(&v["files"]);
                if !f.is_empty() {
                    entry.files = f;
                }
                if v["changes"].is_object() {
                    entry.changes_files = v["changes"]["files"].as_u64().unwrap_or(0) as usize;
                    entry.added = n(&v["changes"], "added");
                    entry.deleted = n(&v["changes"], "deleted");
                }
                if !s(&v, "acceptLine").is_empty() {
                    entry.accept_line = s(&v, "acceptLine").to_string();
                }
                if !s(&v, "appliedAt").is_empty() {
                    entry.applied_at = s(&v, "appliedAt").to_string();
                }
                if !s(&v, "appliedHead").is_empty() {
                    entry.applied_head = s(&v, "appliedHead").to_string();
                }
                if !s(&v, "appliedBy").is_empty() {
                    entry.applied_by = s(&v, "appliedBy").to_string();
                }
                if v["tokens"].is_object() {
                    entry.tokens = v["tokens"].clone();
                }
                if v["tokensDelta"].is_object() {
                    entry.tokens_delta = v["tokensDelta"].clone();
                }
                if n(&v, "elapsedSeconds") > 0 {
                    entry.elapsed_seconds = n(&v, "elapsedSeconds");
                }
                if let Some(rc) = v["reworkCount"].as_u64() {
                    entry.rework_count = entry.rework_count.max(rc);
                }
                if !s(&v, "archive").is_empty() {
                    entry.archive = s(&v, "archive").to_string();
                }
                if b(&v, "cleaned") {
                    entry.cleaned = true;
                }
                if !s(&v, "sourceRoot").is_empty() {
                    entry.source_root = s(&v, "sourceRoot").to_string();
                }
            }
            "apply" => {
                let entry = map.entry(run_id.clone()).or_default();
                entry.run = run_id;
                if !s(&v, "appliedAt").is_empty() {
                    entry.applied_at = s(&v, "appliedAt").to_string();
                }
                if !s(&v, "appliedHead").is_empty() {
                    entry.applied_head = s(&v, "appliedHead").to_string();
                }
                if !s(&v, "appliedBy").is_empty() {
                    entry.applied_by = s(&v, "appliedBy").to_string();
                }
                if !s(&v, "archive").is_empty() {
                    entry.archive = s(&v, "archive").to_string();
                }
            }
            "clean" => {
                let entry = map.entry(run_id.clone()).or_default();
                entry.run = run_id;
                entry.cleaned = true;
                if !s(&v, "archive").is_empty() {
                    entry.archive = s(&v, "archive").to_string();
                }
            }
            "reply" => {
                let entry = map.entry(run_id.clone()).or_default();
                entry.run = run_id;
                let child = s(&v, "child").to_string();
                if !child.is_empty() && !entry.children.contains(&child) {
                    entry.children.push(child);
                }
                if let Some(rc) = v["reworkCount"].as_u64() {
                    entry.rework_count = entry.rework_count.max(rc);
                }
            }
            _ => {}
        }
    }
    // Also synthesize parent->children links from `parent` fields across entries.
    let links: Vec<(String, String)> = map
        .values()
        .filter(|e| !e.parent.is_empty())
        .map(|e| (e.parent.clone(), e.run.clone()))
        .collect();
    for (parent, child) in links {
        if let Some(p) = map.get_mut(&parent) {
            if !p.children.contains(&child) {
                p.children.push(child);
            }
        }
    }
    let mut entries: Vec<HistoryEntry> = map
        .into_values()
        .filter(|e| !e.state.is_empty() || !e.name.is_empty())
        .collect();
    entries.sort_by(|a, b| a.sort_ts().cmp(&b.sort_ts()).then_with(|| a.run.cmp(&b.run)));
    entries
}

/// One-time backfill for existing uncleaned runs and archives on first run of this version.
/// Never moves or migrates run/archive directories.
pub fn ensure_backfilled() {
    let key = current_history_key();
    let dir = history_dir_for_key(&key);
    let marker = dir.join(".backfilled-v1");
    let history_file = dir.join("history.jsonl");
    if marker.is_file() && history_file.is_file() {
        return;
    }
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(_guard) = crate::locks::acquire(crate::locks::LockKind::History, &dir.join("history.lock"), true) else {
        return;
    };
    if marker.is_file() && history_file.is_file() {
        return;
    }
    let existing: HashSet<String> = load_entries(&key).into_iter().map(|e| e.run).collect();
    let mut candidates = Vec::new();
    for run in runs::all_runs().into_iter().chain(runs::archived_runs()) {
        if !run.join("meta.json").is_file() {
            continue;
        }
        let st = runs::state(&run);
        if runs::active(&st) {
            continue;
        }
        let meta = json(run.join("meta.json"));
        let id = if !s(&meta, "run").is_empty() {
            s(&meta, "run").to_string()
        } else {
            run.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        if id.is_empty() || existing.contains(&id) {
            continue;
        }
        let ns = meta["startedNs"].as_u64().unwrap_or(0);
        candidates.push((ns, id, run));
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut seen = existing;
    let mut buf = String::new();
    for (_, id, run) in candidates {
        if !seen.insert(id) {
            continue;
        }
        if let Some((_, rec)) = build_finish_record(&run) {
            if let Ok(line) = serde_json::to_string(&rec) {
                buf.push_str(&line);
                buf.push('\n');
            }
        }
    }
    if !buf.is_empty() {
        let _ = append(&history_file, &buf);
    } else if !history_file.exists() {
        let _ = write(&history_file, "");
    }
    let _ = write(&marker, "1\n");
}

// --- Tokenization, Chinese unigram+bigram matching, and deduplication ---

fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF | 0x20000..=0x2A6DF
    )
}

fn is_cjk_stopword(c: char) -> bool {
    matches!(
        c,
        '的' | '了'
            | '是'
            | '和'
            | '与'
            | '或'
            | '把'
            | '被'
            | '在'
            | '为'
            | '之'
            | '而'
            | '及'
            | '等'
            | '请'
            | '我'
            | '你'
            | '它'
            | '这'
            | '那'
            | '个'
            | '一'
            | '也'
            | '都'
            | '就'
            | '着'
            | '过'
            | '里'
            | '中'
            | '到'
            | '对'
            | '让'
            | '给'
            | '用'
            | '吗'
            | '呢'
            | '吧'
            | '啊'
    )
}

fn is_ascii_stopword(word: &str) -> bool {
    matches!(
        word,
        "the"
            | "and"
            | "for"
            | "with"
            | "from"
            | "this"
            | "that"
            | "into"
            | "onto"
            | "are"
            | "was"
            | "were"
            | "has"
            | "have"
            | "had"
            | "not"
            | "but"
            | "can"
            | "will"
            | "all"
            | "any"
            | "our"
            | "your"
            | "its"
            | "in"
            | "on"
            | "of"
            | "to"
            | "is"
            | "be"
            | "by"
            | "at"
            | "as"
            | "it"
            | "or"
            | "an"
    )
}

/// Tokenize text into CJK unigrams (excluding stopwords), CJK bigrams, and lowercased ASCII words.
pub fn tokenize(text: &str) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if is_cjk(c) {
            let start = i;
            while i < chars.len() && is_cjk(chars[i]) {
                i += 1;
            }
            let run = &chars[start..i];
            for &ch in run {
                if !is_cjk_stopword(ch) {
                    tokens.insert(ch.to_string());
                }
            }
            for pair in run.windows(2) {
                let mut bg = String::with_capacity(8);
                bg.push(pair[0]);
                bg.push(pair[1]);
                tokens.insert(bg);
            }
        } else if c.is_ascii_alphanumeric() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_alphanumeric() {
                i += 1;
            }
            let word: String = chars[start..i]
                .iter()
                .map(|ch| ch.to_ascii_lowercase())
                .collect();
            if word.len() >= 2 && !is_ascii_stopword(&word) {
                tokens.insert(word);
            }
        } else {
            i += 1;
        }
    }
    tokens
}

pub fn dice_similarity(a: &BTreeSet<String>, b: &BTreeSet<String>) -> (f64, usize) {
    if a.is_empty() || b.is_empty() {
        return (0.0, 0);
    }
    let shared = a.intersection(b).count();
    let score = (2.0 * shared as f64) / (a.len() + b.len()) as f64;
    (score, shared)
}

pub fn similarity_score(
    name_a: &str,
    prompt_a: &str,
    name_b: &str,
    prompt_b: &str,
) -> Option<f64> {
    let full_a = tokenize(&format!("{name_a}\n{prompt_a}"));
    let full_b = tokenize(&format!("{name_b}\n{prompt_b}"));
    let (score, shared) = dice_similarity(&full_a, &full_b);
    if shared >= DEDUP_MIN_SHARED && score >= DEDUP_SIMILARITY_THRESHOLD {
        return Some(score);
    }
    let na = tokenize(name_a);
    let nb = tokenize(name_b);
    let (name_score, name_shared) = dice_similarity(&na, &nb);
    if name_shared >= 3 && name_score >= DEDUP_NAME_THRESHOLD {
        return Some(name_score.max(score));
    }
    None
}

/// Match a single search keyword against haystack (name + prompt + conclusion).
/// Supports exact substring, ASCII words, and Chinese unigram + bigram matching.
pub fn keyword_matches(haystack: &str, keyword: &str) -> bool {
    let kw = keyword.trim();
    if kw.is_empty() {
        return true;
    }
    let hay_lower = haystack.to_lowercase();
    let kw_lower = kw.to_lowercase();
    if hay_lower.contains(&kw_lower) {
        return true;
    }
    // Split into ASCII word requirements and CJK unigram+bigram requirements.
    let chars: Vec<char> = kw.chars().collect();
    let mut ascii_words = Vec::new();
    let mut cjk_unigrams = BTreeSet::new();
    let mut cjk_bigrams = BTreeSet::new();
    let mut i = 0;
    while i < chars.len() {
        if is_cjk(chars[i]) {
            let start = i;
            while i < chars.len() && is_cjk(chars[i]) {
                i += 1;
            }
            let run = &chars[start..i];
            for &ch in run {
                cjk_unigrams.insert(ch);
            }
            for pair in run.windows(2) {
                let mut bg = String::with_capacity(8);
                bg.push(pair[0]);
                bg.push(pair[1]);
                cjk_bigrams.insert(bg);
            }
        } else if chars[i].is_ascii_alphanumeric() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_alphanumeric() {
                i += 1;
            }
            let w: String = chars[start..i]
                .iter()
                .map(|c| c.to_ascii_lowercase())
                .collect();
            if !w.is_empty() {
                ascii_words.push(w);
            }
        } else {
            i += 1;
        }
    }
    if ascii_words.is_empty() && cjk_unigrams.is_empty() {
        return false;
    }
    for word in &ascii_words {
        if !hay_lower.contains(word.as_str()) {
            return false;
        }
    }
    if cjk_unigrams.is_empty() {
        return true;
    }
    if cjk_unigrams.len() == 1 {
        let ch = *cjk_unigrams.iter().next().unwrap();
        return haystack.contains(ch);
    }
    if cjk_bigrams.len() == 1 {
        let bg = cjk_bigrams.iter().next().unwrap();
        return haystack.contains(bg.as_str());
    }
    let hay_chars: BTreeSet<char> = haystack.chars().filter(|&c| is_cjk(c)).collect();
    let uni_hits = cjk_unigrams
        .iter()
        .filter(|c| hay_chars.contains(c))
        .count();
    let bg_hits = cjk_bigrams
        .iter()
        .filter(|bg| haystack.contains(bg.as_str()))
        .count();
    (uni_hits == cjk_unigrams.len() && bg_hits >= 1)
        || (!cjk_bigrams.is_empty() && (bg_hits as f64 / cjk_bigrams.len() as f64) >= 0.5)
}

pub fn state_label(state: &str, question: bool, applied: bool) -> String {
    state_label_full(state, question, false, applied)
}

pub fn state_label_full(
    state: &str,
    question: bool,
    policy_refused: bool,
    applied: bool,
) -> String {
    let base = if question {
        "提问"
    } else if policy_refused {
        "结论被模型服务拒绝"
    } else {
        match state {
            "running" => "运行中",
            "waiting" => "排队中",
            "starting" => "启动中",
            "delivered" => "已交付",
            "answered" => "已答复",
            "blocked_mainline" => "主干阻塞",
            "rejected" => "未通过",
            "timeout" => "超时",
            "stalled" => "卡住",
            "stopped" => "已停止",
            "needs-redispatch" => "需要重派",
            "malformed" => "答复畸形",
            "failed" => "失败",
            "crashed" => "进程异常",
            "killed" => "已终止",
            _ => state,
        }
    };
    if applied && matches!(state, "delivered" | "answered") && !question && !policy_refused {
        format!("{base}/已合入")
    } else {
        base.to_string()
    }
}

pub fn relative_age(ts: i64) -> String {
    let diff = (epoch() as i64 - ts).max(0);
    if diff < 60 {
        "刚刚".to_string()
    } else if diff < 3600 {
        format!("{} 分钟前", diff / 60)
    } else if diff < 86400 {
        format!("{} 小时前", diff / 3600)
    } else {
        format!("{} 天前", diff / 86400)
    }
}

/// Compare a new start/run task against active runs and the last 14 days of history.
/// Prints up to 3 lines on stderr; advisory only, never blocks.
pub fn warn_similar_runs(created_run: &Path, name: &str, raw_prompt: &str) {
    let created_id = created_run
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let now = epoch() as i64;
    let cutoff = now - (DEDUP_WINDOW_DAYS as i64 * 86400);
    let mut seen = HashSet::new();
    seen.insert(created_id);

    // (is_active, score, ts, run_id, name, state_label)
    let mut hits: Vec<(bool, f64, i64, String, String, String)> = Vec::new();

    for run in runs::all_runs() {
        let st = runs::state(&run);
        if !runs::active(&st) {
            continue;
        }
        let meta = json(run.join("meta.json"));
        let id = if !s(&meta, "run").is_empty() {
            s(&meta, "run").to_string()
        } else {
            run.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        let other_name = s(&meta, "name").to_string();
        let other_prompt = extract_user_prompt(&run, &meta);
        if let Some(score) = similarity_score(name, raw_prompt, &other_name, &other_prompt) {
            let ts = meta["startedEpoch"].as_i64().unwrap_or(now);
            hits.push((true, score, ts, id, other_name, state_label(&st, false, false)));
        }
    }

    let key = current_history_key();
    for entry in load_entries(&key) {
        if !seen.insert(entry.run.clone()) {
            continue;
        }
        let ts = entry.recent_ts();
        if ts > 0 && ts < cutoff {
            continue;
        }
        let prompt_cmp = if !entry.prompt_excerpt.is_empty() {
            entry.prompt_excerpt.as_str()
        } else {
            entry.prompt_first_line.as_str()
        };
        if let Some(score) = similarity_score(name, raw_prompt, &entry.name, prompt_cmp) {
            let label = state_label_full(
                &entry.state,
                entry.question,
                entry.policy_refused,
                !entry.applied_at.is_empty(),
            );
            hits.push((false, score, ts, entry.run, entry.name, label));
        }
    }

    hits.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.total_cmp(&a.1))
            .then_with(|| b.2.cmp(&a.2))
    });
    for (_, _, ts, id, other_name, label) in hits.into_iter().take(DEDUP_MAX_HINTS) {
        let age = if ts > 0 { relative_age(ts) } else { "未知时间".into() };
        let clean_name = other_name.replace(['\n', '\r'], " ");
        eprintln!("相似：{clean_name} · {label} · {age} · delegate show {id}");
    }
}

pub fn compact_model_label(agent: &str, model: &str, model_name: &str, tier: &str) -> String {
    let agent = if agent.is_empty() {
        crate::agents::AGENT
    } else {
        agent
    };
    let name = if !model_name.is_empty() && !model_name.contains('/') {
        model_name
    } else if !model.is_empty() {
        model.rsplit_once('/').map_or(model, |(_, n)| n)
    } else {
        tier
    };
    if name.is_empty() {
        agent.to_string()
    } else {
        format!("{agent}/{name}")
    }
}

fn compact_changes_label(entry: &HistoryEntry) -> String {
    if entry.changes_files == 0 {
        if entry.mode == "read-only" {
            return "只读".to_string();
        }
        return "无改动".to_string();
    }
    let mut dirs: Vec<(&String, &usize)> = entry.files_by_dir.iter().collect();
    dirs.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let dir_str = dirs
        .iter()
        .take(2)
        .map(|(d, c)| format!("{d} {c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut text = format!("改 {} 文件", entry.changes_files);
    if entry.added > 0 || entry.deleted > 0 {
        text.push_str(&format!(" +{}/-{}", entry.added, entry.deleted));
    }
    if !dir_str.is_empty() {
        text.push_str(&format!("（{dir_str}）"));
    }
    text
}

fn matches_state_filter(entry: &HistoryEntry, filter: &str) -> bool {
    let label = state_label_full(
        &entry.state,
        entry.question,
        entry.policy_refused,
        !entry.applied_at.is_empty(),
    );
    filter.split(',').map(str::trim).filter(|s| !s.is_empty()).any(|want| {
        let want_lower = want.to_lowercase();
        match want_lower.as_str() {
            "applied" | "已合入" => !entry.applied_at.is_empty(),
            "question" | "提问" => entry.question,
            _ => {
                entry.state.eq_ignore_ascii_case(want)
                    || label.contains(want)
            }
        }
    })
}

pub fn history_command(args: &[String]) -> Res<i32> {
    let mut keywords = Vec::new();
    let mut since: Option<f64> = None;
    let mut state_filter: Option<String> = None;
    let mut limit: Option<usize> = None;
    let mut json_mode = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json_mode = true,
            "--since" => {
                i += 1;
                let v = args.get(i).ok_or("--since requires a duration (e.g. 14d)")?;
                since = Some(seconds(v)?);
            }
            "--state" => {
                i += 1;
                let v = args.get(i).ok_or("--state requires a state value")?;
                state_filter = Some(v.clone());
            }
            "--limit" => {
                i += 1;
                let v = args.get(i).ok_or("--limit requires a positive integer")?;
                limit = Some(
                    v.parse::<usize>()
                        .map_err(|_| format!("invalid --limit: {v}"))?,
                );
            }
            arg if arg.starts_with("--since=") => {
                since = Some(seconds(&arg["--since=".len()..])?);
            }
            arg if arg.starts_with("--state=") => {
                state_filter = Some(arg["--state=".len()..].to_string());
            }
            arg if arg.starts_with("--limit=") => {
                let v = &arg["--limit=".len()..];
                limit = Some(
                    v.parse::<usize>()
                        .map_err(|_| format!("invalid --limit: {v}"))?,
                );
            }
            arg if arg.starts_with('-') => {
                return Err(format!("unrecognized arguments: {arg}"));
            }
            arg => keywords.push(arg.to_string()),
        }
        i += 1;
    }

    let key = current_history_key();
    let now = epoch() as i64;
    let mut entries: Vec<HistoryEntry> = load_entries_readonly(&key)
        .into_iter()
        .filter(|e| {
            if let Some(window) = since {
                let ts = e.recent_ts();
                if ts > 0 && (now - ts) as f64 > window {
                    return false;
                }
            }
            if let Some(ref st) = state_filter {
                if !matches_state_filter(e, st) {
                    return false;
                }
            }
            if !keywords.is_empty() {
                let haystack = format!(
                    "{}\n{}\n{}\n{}\n{}",
                    e.run, e.name, e.prompt_first_line, e.prompt_excerpt, e.conclusion
                );
                if !keywords.iter().all(|kw| keyword_matches(&haystack, kw)) {
                    return false;
                }
            }
            true
        })
        .collect();

    if let Some(n) = limit {
        if entries.len() > n {
            entries = entries.split_off(entries.len() - n);
        }
    }

    if json_mode {
        for entry in &entries {
            println!("{}", entry.to_json());
        }
        return Ok(0);
    }

    if entries.is_empty() {
        println!("没有匹配的历史记录");
        return Ok(0);
    }

    for entry in &entries {
        let ts = entry.recent_ts();
        let time_str = if ts > 0 {
            format_epoch(ts, true, "%Y-%m-%d %H:%M")
        } else {
            "未知时间".into()
        };
        let clean_name = entry.name.replace(['\n', '\r'], " ");
        let st_label = state_label_full(
            &entry.state,
            entry.question,
            entry.policy_refused,
            !entry.applied_at.is_empty(),
        );
        let model_label =
            compact_model_label(&entry.agent, &entry.model, &entry.model_name, &entry.tier);
        let changes_label = compact_changes_label(entry);
        let tok_str = format_tokens(entry.effective_tokens());
        if tok_str.is_empty() {
            println!(
                "{time_str} · {clean_name} · {st_label} · {model_label} · {changes_label} · delegate show {}",
                entry.run
            );
        } else {
            let tok_suffix = if entry.tokens_delta.as_object().is_some_and(|o| !o.is_empty()) {
                "（增量）"
            } else {
                ""
            };
            println!(
                "{time_str} · {clean_name} · {st_label} · {model_label} · {changes_label} · tokens {tok_str}{tok_suffix} · delegate show {}",
                entry.run
            );
        }
    }
    Ok(0)
}

fn subsequent_commits(
    source_root: &str,
    applied_at: &str,
    applied_head: &str,
    files: &[String],
) -> Vec<String> {
    if source_root.is_empty() || files.is_empty() || (applied_at.is_empty() && applied_head.is_empty())
    {
        return Vec::new();
    }
    let root = Path::new(source_root);
    if !root.is_dir() {
        return Vec::new();
    }
    let mut args: Vec<String> = vec![
        "log".into(),
        "--format=%h %cs %s".into(),
        "-n".into(),
        "10".into(),
    ];
    let head_valid = !applied_head.is_empty()
        && git_text(
            root,
            &["rev-parse", "--verify", &format!("{applied_head}^{{commit}}")],
        )
        .is_ok();
    if head_valid {
        args.push(format!("{applied_head}..HEAD"));
    } else if !applied_at.is_empty() {
        args.push(format!("--since={applied_at}"));
    } else {
        return Vec::new();
    }
    args.push("--".into());
    for f in files.iter().take(50) {
        args.push(f.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match git_text(root, &arg_refs) {
        Ok(out) => out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn build_chain(target_id: &str, entries: &[HistoryEntry]) -> Vec<String> {
    let mut parent_of: HashMap<String, String> = HashMap::new();
    let mut children_of: HashMap<String, Vec<String>> = HashMap::new();
    for e in entries {
        if !e.parent.is_empty() {
            parent_of.insert(e.run.clone(), e.parent.clone());
            children_of
                .entry(e.parent.clone())
                .or_default()
                .push(e.run.clone());
        }
        for c in &e.children {
            parent_of.entry(c.clone()).or_insert_with(|| e.run.clone());
            let list = children_of.entry(e.run.clone()).or_default();
            if !list.contains(c) {
                list.push(c.clone());
            }
        }
    }
    for run in runs::all_runs() {
        let meta = json(run.join("meta.json"));
        let id = s(&meta, "run").to_string();
        let p = s(&meta, "parent").to_string();
        if !id.is_empty() && !p.is_empty() {
            parent_of.insert(id.clone(), p.clone());
            let list = children_of.entry(p).or_default();
            if !list.contains(&id) {
                list.push(id);
            }
        }
    }
    let mut root = target_id.to_string();
    let mut visited = HashSet::new();
    visited.insert(root.clone());
    while let Some(p) = parent_of.get(&root) {
        if p.is_empty() || !visited.insert(p.clone()) {
            break;
        }
        root = p.clone();
    }
    let mut chain = Vec::new();
    let mut queue = std::collections::VecDeque::new();
    let mut seen = HashSet::new();
    queue.push_back(root);
    while let Some(cur) = queue.pop_front() {
        if !seen.insert(cur.clone()) {
            continue;
        }
        chain.push(cur.clone());
        if let Some(kids) = children_of.get(&cur) {
            for k in kids {
                queue.push_back(k.clone());
            }
        }
    }
    chain
}

fn find_history_entry<'a>(entries: &'a [HistoryEntry], reference: &str) -> Option<&'a HistoryEntry> {
    if reference == "last" {
        return entries.last();
    }
    if let Some(exact) = entries.iter().find(|e| e.run == reference) {
        return Some(exact);
    }
    let named: Vec<_> = entries.iter().filter(|e| e.name == reference).collect();
    if !named.is_empty() {
        return named.last().copied();
    }
    let sub: Vec<_> = entries.iter().filter(|e| e.run.contains(reference)).collect();
    if sub.len() == 1 {
        return Some(sub[0]);
    }
    None
}

/// Resolve a cleaned/archived run's `archive/<run>/record` directory via the repository history index.
pub fn resolve_archive_record(reference: &str) -> Option<PathBuf> {
    let entries = load_entries(&current_history_key());
    let h = find_history_entry(&entries, reference)?;
    if h.archive.is_empty() {
        return None;
    }
    let rec = Path::new(&h.archive).join("record");
    if rec.join("meta.json").is_file() {
        let _ = crate::archive::touch(Path::new(&h.archive));
        Some(rec)
    } else {
        None
    }
}

fn format_tokens(tokens: &Value) -> String {
    if !tokens.is_object() {
        return String::new();
    }
    let input = n(tokens, "input");
    let output = n(tokens, "output");
    let cache = n(tokens, "cacheRead");
    if input == 0 && output == 0 && cache == 0 {
        return String::new();
    }
    if cache > 0 {
        format!("in {input} / out {output} / cache {cache}")
    } else {
        format!("in {input} / out {output}")
    }
}

fn format_elapsed(seconds: i64) -> String {
    if seconds <= 0 {
        return String::new();
    }
    if seconds < 60 {
        format!("{seconds} 秒")
    } else if seconds < 3600 {
        format!("{} 分 {} 秒", seconds / 60, seconds % 60)
    } else {
        format!("{} 小时 {} 分", seconds / 3600, (seconds % 3600) / 60)
    }
}

pub fn show_command(args: &[String]) -> Res<i32> {
    let mut pos = Vec::new();
    let mut json_mode = false;
    for arg in args {
        if arg == "--json" {
            json_mode = true;
        } else if arg.starts_with('-') {
            return Err(format!("unrecognized arguments: {arg}"));
        } else {
            pos.push(arg.clone());
        }
    }
    if pos.len() > 1 {
        return Err("show takes at most one run".into());
    }
    let reference = pos.first().map(String::as_str).unwrap_or("last");

    let key = current_history_key();
    let entries = load_entries(&key);

    // Resolve on-disk run dir (live or archived record) or fall back to history index.
    let mut disk_dir = runs::resolve(reference).ok();
    let hist = if let Some(ref dir) = disk_dir {
        let meta = json(dir.join("meta.json"));
        let id = if !s(&meta, "run").is_empty() {
            s(&meta, "run").to_string()
        } else {
            dir.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        entries.iter().find(|e| e.run == id).cloned()
    } else {
        find_history_entry(&entries, reference).cloned()
    };

    // If disk_dir wasn't found via resolve, check if history's archive/record still exists.
    if disk_dir.is_none() {
        if let Some(ref h) = hist {
            if !h.archive.is_empty() {
                let rec = Path::new(&h.archive).join("record");
                if rec.join("meta.json").is_file() {
                    let _ = crate::archive::touch(Path::new(&h.archive));
                    disk_dir = Some(rec);
                }
            }
        }
    }

    if disk_dir.is_none() && hist.is_none() {
        return Err(format!("run '{reference}' not found in runs, archives, or history"));
    }

    // Touch archive LRU if reading an archive record.
    let mut archive_path = hist
        .as_ref()
        .map(|h| h.archive.clone())
        .unwrap_or_default();
    let mut archive_alive = !archive_path.is_empty() && Path::new(&archive_path).is_dir();
    let mut live_dir: Option<PathBuf> = None;

    if let Some(ref dir) = disk_dir {
        if crate::archive::is_record(dir) {
            if let Some(arch) = dir.parent() {
                archive_path = arch.to_string_lossy().into_owned();
                archive_alive = arch.is_dir();
                let _ = crate::archive::touch(arch);
            }
        } else {
            live_dir = Some(dir.clone());
            if let Some(manifest) = crate::archive::status(dir) {
                archive_path = s(&manifest, "path").to_string();
                archive_alive = !archive_path.is_empty() && Path::new(&archive_path).is_dir();
            }
        }
    }

    let original_expired = disk_dir.is_none();

    // Gather unified fields from disk_dir (authoritative when present) + history fallback.
    let (
        run_id,
        name,
        mode,
        started_at,
        finished_at,
        agent,
        tier,
        model,
        model_name,
        state,
        question,
        policy_refused,
        prompt_head,
        conclusion,
        files_by_dir,
        files,
        changes_files,
        added,
        deleted,
        accept_line,
        applied_at,
        applied_head,
        applied_by,
        tokens,
        tokens_delta,
        elapsed_seconds,
        rework_count,
        source_root,
        has_result,
        has_patch,
    ) = if let Some(ref dir) = disk_dir {
        let meta = json(dir.join("meta.json"));
        let sum = json(dir.join("summary.json"));
        let st = runs::state(dir);
        let id = if !s(&meta, "run").is_empty() {
            s(&meta, "run").to_string()
        } else {
            dir.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        let full_prompt = extract_user_prompt(dir, &meta);
        let prompt_lines: String = full_prompt
            .lines()
            .take(5)
            .collect::<Vec<_>>()
            .join("\n");
        let prompt_head = clip_chars(prompt_lines.trim(), PROMPT_EXCERPT_CHARS);
        let result_text = read(dir.join("result.md"));
        let conclusion = extract_conclusion(&result_text);
        let (f, fdir) = files_and_dirs(dir, &meta, &sum);
        let ch = &sum["changes"];
        let cfiles = ch["files"].as_u64().map(|x| x as usize).unwrap_or(f.len());
        let added = n(ch, "added");
        let deleted = n(ch, "deleted");
        let applied = json(dir.join(".applied"));
        let applied_at = if !s(&meta, "appliedAt").is_empty() {
            s(&meta, "appliedAt").to_string()
        } else if !s(&applied, "at").is_empty() {
            s(&applied, "at").to_string()
        } else {
            hist.as_ref().map(|h| h.applied_at.clone()).unwrap_or_default()
        };
        let applied_head = if !s(&meta, "appliedHead").is_empty() {
            s(&meta, "appliedHead").to_string()
        } else if !s(&applied, "head").is_empty() {
            s(&applied, "head").to_string()
        } else {
            hist.as_ref().map(|h| h.applied_head.clone()).unwrap_or_default()
        };
        let applied_by = if !applied_at.is_empty() {
            let by = s(&applied, "appliedBy");
            if !by.is_empty() {
                by.to_string()
            } else {
                hist.as_ref()
                    .map(|h| h.applied_by.clone())
                    .filter(|x| !x.is_empty())
                    .unwrap_or_else(|| "delegate".into())
            }
        } else {
            String::new()
        };
        let has_result = dir.join("result.md").is_file();
        let has_patch = dir.join("applied.patch").is_file() || dir.join("changes.patch").is_file();
        let rework = crate::launch::rework_count(dir)
            .max(hist.as_ref().map(|h| h.rework_count).unwrap_or(0));
        let model = if !s(&sum, "model").is_empty() {
            s(&sum, "model").to_string()
        } else {
            s(&meta, "model").to_string()
        };
        (
            id,
            s(&meta, "name").to_string(),
            s(&meta, "mode").to_string(),
            s(&meta, "startedAt").to_string(),
            s(&sum, "finishedAt").to_string(),
            s(&meta, "agent").to_string(),
            s(&meta, "tier").to_string(),
            model,
            s(&meta, "modelName").to_string(),
            st,
            b(&sum, "question"),
            b(&sum, "policyRefused"),
            prompt_head,
            conclusion,
            fdir,
            f,
            cfiles,
            added,
            deleted,
            accept_summary_line(&meta, &sum),
            applied_at,
            applied_head,
            applied_by,
            sum["tokens"].clone(),
            sum["tokensDelta"].clone(),
            n(&sum, "elapsedSeconds"),
            rework,
            source_root_for_meta(&meta),
            has_result,
            has_patch,
        )
    } else {
        let h = hist.as_ref().unwrap();
        let src = if !h.source_root.is_empty() {
            h.source_root.clone()
        } else {
            git_top(&env::current_dir().unwrap_or_default())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        (
            h.run.clone(),
            h.name.clone(),
            h.mode.clone(),
            h.started_at.clone(),
            h.finished_at.clone(),
            h.agent.clone(),
            h.tier.clone(),
            h.model.clone(),
            h.model_name.clone(),
            h.state.clone(),
            h.question,
            h.policy_refused,
            if !h.prompt_excerpt.is_empty() {
                h.prompt_excerpt.clone()
            } else {
                h.prompt_first_line.clone()
            },
            h.conclusion.clone(),
            h.files_by_dir.clone(),
            h.files.clone(),
            h.changes_files,
            h.added,
            h.deleted,
            h.accept_line.clone(),
            h.applied_at.clone(),
            h.applied_head.clone(),
            h.applied_by.clone(),
            h.tokens.clone(),
            h.tokens_delta.clone(),
            h.elapsed_seconds,
            h.rework_count,
            src,
            false,
            false,
        )
    };

    let commits = if !applied_at.is_empty() {
        subsequent_commits(&source_root, &applied_at, &applied_head, &files)
    } else {
        Vec::new()
    };
    let chain = build_chain(&run_id, &entries);

    if json_mode {
        let mut next = serde_json::Map::new();
        if has_result {
            next.insert("result".into(), json!(format!("delegate result {run_id}")));
        }
        if has_patch {
            next.insert("patch".into(), json!(format!("delegate patch {run_id}")));
        }
        if archive_alive {
            next.insert("archive".into(), json!(archive_path));
        }
        let mut out = json!({
            "run": run_id,
            "name": name,
            "mode": mode,
            "state": state,
            "question": question,
            "policyRefused": policy_refused,
            "agent": agent,
            "tier": tier,
            "model": model,
            "modelName": model_name,
            "startedAt": started_at,
            "finishedAt": finished_at,
            "elapsedSeconds": elapsed_seconds,
            "tokens": tokens,
            "reworkCount": rework_count,
            "prompt": prompt_head,
            "conclusion": conclusion,
            "filesByDir": files_by_dir,
            "files": files,
            "changes": {"files": changes_files, "added": added, "deleted": deleted},
            "acceptLine": accept_line,
            "appliedAt": if applied_at.is_empty() { Value::Null } else { json!(applied_at) },
            "appliedHead": if applied_head.is_empty() { Value::Null } else { json!(applied_head) },
            "appliedBy": if applied_by.is_empty() { Value::Null } else { json!(applied_by) },
            "subsequentCommits": commits,
            "chain": chain,
            "dir": live_dir.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "archive": if archive_path.is_empty() { Value::Null } else { json!(archive_path) },
            "originalExpired": original_expired,
            "next": Value::Object(next),
        });
        if tokens_delta.as_object().is_some_and(|o| !o.is_empty()) {
            out["tokensDelta"] = tokens_delta;
        }
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return Ok(0);
    }

    let st_label = state_label_full(&state, question, policy_refused, !applied_at.is_empty());
    let model_label = compact_model_label(&agent, &model, &model_name, &tier);
    let mut header = vec![
        run_id.clone(),
        name.replace(['\n', '\r'], " "),
        st_label,
        model_label,
    ];
    let elapsed_str = format_elapsed(elapsed_seconds);
    if !elapsed_str.is_empty() {
        header.push(format!("耗时 {elapsed_str}"));
    }
    let has_delta = tokens_delta.as_object().is_some_and(|o| !o.is_empty());
    let display_tokens = if has_delta { &tokens_delta } else { &tokens };
    let tok_str = format_tokens(display_tokens);
    if !tok_str.is_empty() {
        if has_delta {
            header.push(format!("tokens {tok_str}（本轮增量）"));
        } else {
            header.push(format!("tokens {tok_str}"));
        }
    }
    if rework_count > 0 {
        header.push(format!("返工 {rework_count}"));
    }
    println!("{}", header.join(" · "));

    if !started_at.is_empty() || !finished_at.is_empty() {
        let s_str = iso_epoch(&started_at)
            .map(|t| format_epoch(t, true, "%Y-%m-%d %H:%M:%S"))
            .unwrap_or_else(|| started_at.clone());
        let f_str = iso_epoch(&finished_at)
            .map(|t| format_epoch(t, true, "%Y-%m-%d %H:%M:%S"))
            .unwrap_or_else(|| finished_at.clone());
        println!("时间：{s_str} → {f_str}");
    }

    if !prompt_head.is_empty() {
        println!("提示词：\n  {}", prompt_head.replace('\n', "\n  "));
    }

    if !conclusion.is_empty() {
        println!("结论：\n  {}", conclusion.replace('\n', "\n  "));
    }

    if changes_files > 0 {
        let mut dirs: Vec<(&String, &usize)> = files_by_dir.iter().collect();
        dirs.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let dir_str = dirs
            .iter()
            .take(4)
            .map(|(d, c)| format!("{d} {c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let file_preview = files
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let more = if files.len() > 8 {
            format!(" 等共 {} 个", files.len())
        } else {
            String::new()
        };
        println!(
            "改动：{changes_files} 个文件 +{added}/-{deleted}{}{}",
            if dir_str.is_empty() {
                String::new()
            } else {
                format!("（{dir_str}）")
            },
            if file_preview.is_empty() {
                String::new()
            } else {
                format!("：{file_preview}{more}")
            }
        );
    }

    if !accept_line.is_empty() {
        println!("验收：{accept_line}");
    }

    if !applied_at.is_empty() {
        let at_str = iso_epoch(&applied_at)
            .map(|t| format_epoch(t, true, "%Y-%m-%d %H:%M:%S"))
            .unwrap_or_else(|| applied_at.clone());
        let head_str = if applied_head.len() >= 7 {
            format!("（HEAD {}）", &applied_head[..7])
        } else if !applied_head.is_empty() {
            format!("（HEAD {applied_head}）")
        } else {
            String::new()
        };
        println!("合入：{at_str}{head_str}");
        if !commits.is_empty() {
            println!("合入后同文件提交：");
            for c in &commits {
                println!("  {c}");
            }
        } else if !files.is_empty() {
            println!("合入后同文件提交：无");
        }
    }

    if chain.len() > 1 {
        let formatted: Vec<String> = chain
            .iter()
            .map(|id| {
                if id == &run_id {
                    format!("[{id}]")
                } else {
                    id.clone()
                }
            })
            .collect();
        println!("续做链：{}", formatted.join(" → "));
    }

    if original_expired {
        if !archive_path.is_empty() {
            println!("归档：原文已过期，仅剩索引（原路径：{archive_path}）");
        } else {
            println!("归档：原文已过期，仅剩索引");
        }
    } else {
        if let Some(ref live) = live_dir {
            println!("目录：{}", live.display());
        }
        if archive_alive {
            println!("归档：{archive_path}");
        }
        let mut next_parts = Vec::new();
        if has_result {
            next_parts.push(format!("delegate result {run_id}"));
        }
        if has_patch {
            next_parts.push(format!("delegate patch {run_id}"));
        }
        if archive_alive {
            next_parts.push(format!("原文归档 {archive_path}"));
        }
        if !next_parts.is_empty() {
            println!("下一层：{}", next_parts.join(" · "));
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_and_english_similarity_separates_paraphrases_from_unrelated_tasks() {
        // Positive: same Chinese task rephrased.
        let s1 = similarity_score(
            "修复头像上传",
            "修复用户头像上传接口返回 500 的问题，检查文件大小校验和格式转换",
            "头像接口500",
            "排查上传用户头像时后端报 500 错误，看看大小限制与图片转码逻辑",
        );
        assert!(
            s1.is_some_and(|v| v >= DEDUP_SIMILARITY_THRESHOLD),
            "paraphrased Chinese task should match: {s1:?}"
        );

        // Positive: another Chinese paraphrase.
        let s2 = similarity_score(
            "头像裁剪",
            "给前端个人资料页加上头像上传和裁剪功能",
            "用户头像裁剪",
            "实现个人主页的头像裁剪与上传",
        );
        assert!(
            s2.is_some_and(|v| v >= DEDUP_SIMILARITY_THRESHOLD),
            "second Chinese paraphrase should match: {s2:?}"
        );

        // Positive: English paraphrase.
        let s3 = similarity_score(
            "web-login-timeout",
            "Fix login timeout bug in apps/web auth middleware",
            "auth-timeout",
            "Investigate auth middleware login timeout issue in apps/web",
        );
        assert!(
            s3.is_some_and(|v| v >= DEDUP_SIMILARITY_THRESHOLD),
            "paraphrased English task should match: {s3:?}"
        );

        // Negative: unrelated Chinese tasks.
        let n1 = similarity_score(
            "修复头像上传",
            "修复用户头像上传接口返回 500 的问题，检查文件大小校验和格式转换",
            "订单CSV导出",
            "给订单导出增加 CSV 下载接口，支持按日期范围筛选和分页查询",
        );
        assert!(n1.is_none(), "unrelated Chinese tasks must not match: {n1:?}");

        // Negative: another unrelated Chinese task.
        let n2 = similarity_score(
            "修复头像上传",
            "修复用户头像上传接口返回 500 的问题",
            "优化构建缓存",
            "优化构建脚本的缓存命中率并减少 CI 耗时",
        );
        assert!(n2.is_none(), "unrelated tasks must not match: {n2:?}");

        // Negative: unrelated English tasks.
        let n3 = similarity_score(
            "web-login-timeout",
            "Fix login timeout bug in apps/web auth middleware",
            "orders-csv-export",
            "Add CSV export endpoint for orders billing report",
        );
        assert!(n3.is_none(), "unrelated English tasks must not match: {n3:?}");

        // Negative: single generic word placeholder should not spam.
        let n4 = similarity_score("task", "task", "task", "task");
        assert!(n4.is_none(), "single generic word must not spam: {n4:?}");
    }

    #[test]
    fn keyword_matching_handles_chinese_unigrams_bigrams_and_non_contiguous() {
        let hay = "修复用户头像的上传接口返回 500 错误，已补大小校验";
        assert!(keyword_matches(hay, "头像"));
        assert!(keyword_matches(hay, "头像上传"));
        assert!(keyword_matches(hay, "500"));
        assert!(!keyword_matches("头部图片像册整理", "头像"));
        assert!(!keyword_matches("优化构建脚本的缓存命中率", "头像上传"));
        assert!(keyword_matches("调整头像组件的 CSS 样式", "css头像"));
    }

    #[test]
    fn extract_conclusion_takes_first_heading_section_or_600_chars() {
        let with_heading = "## 结论\n已修复头像上传 500。\n\n## 详细改动\n改了 20 个文件。";
        let c = extract_conclusion(with_heading);
        assert!(c.contains("已修复头像上传 500"), "{c}");
        assert!(!c.contains("详细改动"), "{c}");

        let with_title = "# 审查报告\n\n## 结论\n发现 2 个空指针。\n\n## 证据\nfoo.rs:10";
        let c2 = extract_conclusion(with_title);
        assert!(c2.contains("发现 2 个空指针"), "{c2}");
        assert!(!c2.contains("foo.rs:10"), "{c2}");

        let plain = "这是一段很长的纯文本结论。".repeat(80);
        let c3 = extract_conclusion(&plain);
        assert!(c3.chars().count() <= CONCLUSION_MAX_CHARS + 1);
        assert!(c3.ends_with('…'));
    }

    #[test]
    fn compact_finish_record_stays_under_2kb_even_for_pure_cjk() {
        let big = json!({
            "event": "finish",
            "at": "2026-11-22T01:00:00Z",
            "run": "20261122-010000-测试超长记录名称",
            "name": "测试超长记录名称",
            "state": "delivered",
            "promptFirstLine": "中".repeat(120),
            "promptExcerpt": "文".repeat(300),
            "conclusion": "结".repeat(600),
            "files": (0..24).map(|i| format!("crates/module_{i}/src/very_long_filename_{i}.rs")).collect::<Vec<_>>(),
            "filesByDir": {"crates/a": 10, "crates/b": 14},
            "acceptLine": "验收通过（cargo test --workspace）",
            "archive": "/home/user/.cache/delegate/archive/20261122-010000-测试超长记录名称"
        });
        let compacted = compact_finish_record(big);
        let serialized = serde_json::to_string(&compacted).unwrap();
        assert!(
            serialized.len() < 2048,
            "expected < 2048 bytes, got {}",
            serialized.len()
        );
    }

    #[test]
    fn strip_delegate_prompt_suffixes_removes_standing_and_boundary() {
        let raw = "修头像上传\n\n只在当前工作目录内改文件；其他路径只读。\n\n仓库规矩：不要改 Cargo.lock\n";
        let clean = strip_delegate_prompt_suffixes(raw, "仓库规矩：不要改 Cargo.lock");
        assert_eq!(clean, "修头像上传");
    }
}
