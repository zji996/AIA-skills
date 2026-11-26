//! Divergent scout / Radar (`delegate radar`):
//! 1. Deterministic collection: runs `.delegate.json` `radar.collect` commands sequentially
//!    and computes a SHA-256 fingerprint over all outputs.
//! 2. Unchanged fingerprint: zero-token exit with a single `radar_unchanged` event.
//! 3. Changed fingerprint: launches a read-only scout (default `scout` tier) that cannot
//!    write files or delegate subtasks; inputs are the numbered collection output and the
//!    previous candidate board.
//! 4. Host validates evidence anchors (`file:line`, `commit <sha>`, command output line
//!    numbers — candidates without anchors are dropped), deduplicates by fingerprint, and
//!    writes `<state>/radar/<repo>/board.md` (+ `board.json`).
//! 5. Controller picks candidates via `delegate radar pick <id>` to generate a task brief
//!    draft without auto-dispatching.
use crate::agents;
use crate::changes;
use crate::common::*;
use crate::config;
use crate::events;
use crate::models;
use crate::output;
use crate::review;
use crate::{has, parse_simple, value as opt};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicI32, Arc, Mutex};

const MAX_COLLECT_IN_PROMPT_BYTES: usize = 160 * 1024;
const MAX_BOARD_IN_PROMPT_BYTES: usize = 40 * 1024;

#[derive(Debug, Clone)]
pub struct CollectSpec {
    pub name: String,
    pub command: String,
}

pub fn validate_radar_config(value: Option<&Value>) -> Res<()> {
    let Some(v) = value else { return Ok(()) };
    if v.is_null() {
        return Ok(());
    }
    let map = v
        .as_object()
        .ok_or("radar must be an object {collect, tier?, model?, thinking?, timeout?}")?;
    for (k, val) in map {
        match k.as_str() {
            "collect" => {
                let arr = val
                    .as_array()
                    .ok_or("radar.collect must be a list of commands")?;
                for (i, item) in arr.iter().enumerate() {
                    if let Some(cmd) = item.as_str() {
                        if cmd.trim().is_empty() || cmd.contains('\0') {
                            return Err(format!("radar.collect[{i}] must be a non-empty string"));
                        }
                    } else if let Some(obj) = item.as_object() {
                        if obj
                            .keys()
                            .any(|key| !["name", "command"].contains(&key.as_str()))
                        {
                            return Err(format!(
                                "radar.collect[{i}] has unknown field (allowed: name, command)"
                            ));
                        }
                        let cmd = obj
                            .get("command")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if cmd.trim().is_empty() || cmd.contains('\0') {
                            return Err(format!(
                                "radar.collect[{i}].command must be a non-empty string"
                            ));
                        }
                        if let Some(name) = obj.get("name") {
                            if !name
                                .as_str()
                                .is_some_and(|s| !s.trim().is_empty() && !s.contains('\0'))
                            {
                                return Err(format!(
                                    "radar.collect[{i}].name must be a non-empty string"
                                ));
                            }
                        }
                    } else {
                        return Err(format!(
                            "radar.collect[{i}] must be a command string or {{name?, command}}"
                        ));
                    }
                }
            }
            "tier" => {
                let t = val
                    .as_str()
                    .ok_or("radar.tier must be a read-only tier (scout or review)")?;
                if !["scout", "review"].contains(&t) {
                    return Err(format!(
                        "radar.tier must be a read-only tier (scout or review), got {t:?}"
                    ));
                }
            }
            "model" => {
                let m = val.as_str().ok_or("radar.model must be a string")?;
                models::validate_model(m)?;
            }
            "thinking" => {
                let t = val.as_str().ok_or("radar.thinking must be a string")?;
                models::validate_thinking(t)?;
            }
            "timeout" => {
                let d = val
                    .as_str()
                    .ok_or("radar.timeout must be a positive duration")?;
                seconds(d)?;
            }
            other => {
                return Err(format!(
                    "radar has unknown field {other:?} (allowed: collect, tier, model, thinking, timeout)"
                ));
            }
        }
    }
    Ok(())
}

fn parse_collect_specs(radar_cfg: &Value) -> Vec<CollectSpec> {
    radar_cfg["collect"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, item)| {
            if let Some(cmd) = item.as_str() {
                Some(CollectSpec {
                    name: format!("cmd-{}", i + 1),
                    command: cmd.to_string(),
                })
            } else if let Some(obj) = item.as_object() {
                let cmd = obj.get("command").and_then(Value::as_str)?.to_string();
                let name = obj
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("cmd-{}", i + 1));
                Some(CollectSpec { name, command: cmd })
            } else {
                None
            }
        })
        .collect()
}

fn canonical_repo(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn radar_root() -> PathBuf {
    state_dir().join("radar")
}

/// Resolve `<state>/radar/<repo>/`: uses sanitized repo basename when unambiguous,
/// or `<basename>-<8hex>` if another repository already owns that basename.
pub fn repo_radar_dir(repo: &Path) -> PathBuf {
    let canon = canonical_repo(repo);
    let canon_str = canon.to_string_lossy().into_owned();
    let base = canon
        .file_name()
        .and_then(|n| n.to_str())
        .map(slug)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "repo".into());
    let root = radar_root();
    let direct = root.join(&base);
    let state_file = direct.join("state.json");
    if state_file.is_file() {
        let existing = json(&state_file);
        let existing_path = s(&existing, "repoPath");
        if existing_path.is_empty() || existing_path == canon_str {
            return direct;
        }
        let hash = review::sha256_hex(canon_str.as_bytes());
        return root.join(format!("{base}-{}", &hash[..8]));
    }
    direct
}

fn run_collect(
    repo: &Path,
    specs: &[CollectSpec],
    env_cfg: &Value,
    timeout: f64,
    scratch: &Path,
) -> Res<(String, String, Vec<Value>)> {
    fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let mut rendered = String::new();
    let mut hash_input = String::new();
    let mut sections = Vec::new();

    for (idx, spec) in specs.iter().enumerate() {
        let num = idx + 1;
        let out_file = scratch.join(format!("collect-{num}.out"));
        let mut f = fs::File::create(&out_file).map_err(|e| e.to_string())?;
        let (code, timed) = run_shell(
            (&spec.command, &format!("radar-collect-{num}")),
            repo,
            scratch,
            env_cfg,
            Some(timeout),
            &mut f,
            None,
        )?;
        let raw_out = fs::read_to_string(&out_file).unwrap_or_default();
        let trimmed_out = raw_out.trim_end_matches('\n');
        let line_count = if trimmed_out.is_empty() {
            0
        } else {
            trimmed_out.lines().count()
        };

        hash_input.push_str(&format!(
            "###[{num}] {}\nCMD:{}\nEXIT:{code} TIMED:{timed}\n{}\n\0",
            spec.name, spec.command, raw_out
        ));

        rendered.push_str(&format!(
            "### [collect#{num}] {} — `{}` (exit {}{})\n",
            spec.name,
            spec.command,
            code,
            if timed { ", timeout" } else { "" }
        ));
        if trimmed_out.is_empty() {
            rendered.push_str("（无输出）\n\n");
        } else {
            for (li, line) in trimmed_out.lines().enumerate() {
                rendered.push_str(&format!("L{}: {line}\n", li + 1));
            }
            rendered.push('\n');
        }

        sections.push(json!({
            "index": num,
            "name": spec.name,
            "command": spec.command,
            "exitCode": code,
            "timedOut": timed,
            "lines": line_count,
        }));
    }

    let fingerprint = format!("sha256:{}", review::sha256_hex(hash_input.as_bytes()));
    Ok((fingerprint, rendered, sections))
}

/// Check whether a single evidence string contains a concrete anchor:
/// 1. `file:line` or `file#Lline` (e.g. `src/main.rs:42`, `crates/a/b.rs:10-20`, `README.md#L5`)
/// 2. `commit <7..40 hex>` / `commit:<hex>` / `sha:<hex>`
/// 3. command output line reference (`collect#1:L12`, `cmd#2:L5`, `output#1:12`, `[1]:L12`, `第12行`, `line 12`, `L12`)
pub fn is_valid_evidence_anchor(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    // 1. Check `file:line` or `file#Lline` across whitespace/punctuation-delimited tokens.
    for raw_tok in trimmed.split(|c: char| {
        c.is_whitespace() || matches!(c, '`' | '(' | ')' | '[' | ']' | ',' | ';' | '"' | '\'' | '（' | '）' | '，' | '；')
    }) {
        let tok = raw_tok.trim_matches(|c: char| matches!(c, '.' | ':'));
        if tok.is_empty() {
            continue;
        }
        // `path:123` or `path:123-145` or `collect#1:L12` or `cmd#2:12`
        if let Some((left, right)) = tok.rsplit_once(':') {
            let right_clean = right.strip_prefix('L').or_else(|| right.strip_prefix('l')).unwrap_or(right);
            let line_part = right_clean.split('-').next().unwrap_or("");
            if !line_part.is_empty()
                && line_part.chars().all(|c| c.is_ascii_digit())
                && line_part.parse::<u64>().is_ok_and(|n| n > 0)
            {
                let left_lower = left.to_ascii_lowercase();
                // File path with `/` or `.` (e.g. `src/foo.rs`, `Cargo.toml`) OR command output label (`collect#1`, `cmd#2`, `output#1`, `输出#1`).
                if left.contains('/')
                    || left.contains('.')
                    || left.contains('#')
                    || left_lower.starts_with("collect")
                    || left_lower.starts_with("cmd")
                    || left_lower.starts_with("output")
                    || left.starts_with("命令")
                    || left.starts_with("输出")
                {
                    return true;
                }
            }
        }
        // `path#L123`
        if let Some((left, right)) = tok.rsplit_once("#L") {
            let line_part = right.split('-').next().unwrap_or("");
            if (left.contains('/') || left.contains('.'))
                && !line_part.is_empty()
                && line_part.chars().all(|c| c.is_ascii_digit())
                && line_part.parse::<u64>().is_ok_and(|n| n > 0)
            {
                return true;
            }
        }
        // Standalone `L12` / `L12-L20` token (command output line reference)
        if let Some(rest) = tok.strip_prefix('L') {
            let first = rest.split('-').next().unwrap_or("");
            if !first.is_empty()
                && first.chars().all(|c| c.is_ascii_digit())
                && first.parse::<u64>().is_ok_and(|n| n > 0)
            {
                return true;
            }
        }
    }

    // 2. Check `commit <hex>` / `commit:<hex>` / `sha:<hex>` / `git <hex>`
    let lower = trimmed.to_ascii_lowercase();
    for prefix in ["commit ", "commit:", "commit#", "sha:", "sha "] {
        if let Some(pos) = lower.find(prefix) {
            let after = lower[pos + prefix.len()..].trim_start();
            let hex: String = after
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            if (7..=40).contains(&hex.len()) {
                return true;
            }
        }
    }

    // 3. Check `line <N>` or `第<N>行`
    if let Some(pos) = lower.find("line ") {
        let after = lower[pos + 5..].trim_start();
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && digits.parse::<u64>().is_ok_and(|n| n > 0) {
            return true;
        }
    }
    if let Some(pos) = trimmed.find('第') {
        let after = trimmed[pos + '第'.len_utf8()..].trim_start();
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        let mut rest = after[digits.len()..].trim_start();
        if let Some(after_dash) = rest
            .strip_prefix('-')
            .or_else(|| rest.strip_prefix('–'))
            .or_else(|| rest.strip_prefix('—'))
            .or_else(|| rest.strip_prefix('~'))
            .or_else(|| rest.strip_prefix('～'))
            .or_else(|| rest.strip_prefix('至'))
            .or_else(|| rest.strip_prefix('到'))
        {
            let after_dash = after_dash.trim_start();
            let end_digits: String = after_dash
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if !end_digits.is_empty() {
                rest = after_dash[end_digits.len()..].trim_start();
            }
        }
        if !digits.is_empty()
            && digits.parse::<u64>().is_ok_and(|n| n > 0)
            && rest.starts_with('行')
        {
            return true;
        }
    }

    false
}

pub fn has_valid_evidence_anchor(evidence: &[String]) -> bool {
    evidence.iter().any(|e| is_valid_evidence_anchor(e))
}

fn normalize_key(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

pub fn candidate_fingerprint(title: &str, evidence: &[String]) -> String {
    let mut norm_ev: Vec<String> = evidence
        .iter()
        .map(|e| normalize_key(e))
        .filter(|e| !e.is_empty())
        .collect();
    norm_ev.sort();
    norm_ev.dedup();
    let payload = format!("{}\n{}", normalize_key(title), norm_ev.join("\n"));
    format!("sha256:{}", review::sha256_hex(payload.as_bytes()))
}

#[derive(Debug, Clone)]
pub struct ParsedCandidate {
    pub title: String,
    pub evidence: Vec<String>,
    pub suggestion: String,
    pub cost: String,
    pub brief_ready: bool,
    pub brief_note: String,
}

fn radar_fence_marker(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let ch = trimmed.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let count = trimmed.chars().take_while(|&c| c == ch).count();
    (count >= 3).then_some((ch, count))
}

fn is_radar_none_reply(answer: &str) -> bool {
    let clean = answer
        .trim()
        .trim_matches(['`', '*', '_', '#', '>', '-', '。', '.', '!', '！', ' ']);
    if clean.is_empty() || clean == "[]" {
        return true;
    }
    let lower = clean.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "none"
            | "no"
            | "nil"
            | "n/a"
            | "no new candidates"
            | "no candidates"
            | "nothing new"
    ) || matches!(
        clean,
        "无"
            | "暂无"
            | "没有"
            | "无新增"
            | "无新候选"
            | "暂无新候选"
            | "无值得新增的候选"
            | "本次无新增候选"
            | "本次采集没有值得新增的候选"
    )
}

fn parse_json_candidates(val: &Value) -> Option<Vec<ParsedCandidate>> {
    let arr = val
        .as_array()
        .or_else(|| val.get("candidates").and_then(Value::as_array))?;
    let mut out = Vec::new();
    for item in arr {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let title = obj
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if title.is_empty() {
            continue;
        }
        let evidence: Vec<String> = match obj.get("evidence") {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect(),
            Some(Value::String(st)) => st
                .lines()
                .map(|x| x.trim().trim_start_matches('-').trim().to_string())
                .filter(|x| !x.is_empty())
                .collect(),
            _ => Vec::new(),
        };
        let suggestion = obj
            .get("suggestion")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let cost = obj
            .get("cost")
            .and_then(Value::as_str)
            .unwrap_or("未估")
            .trim()
            .to_string();
        let brief_ready = obj
            .get("briefReady")
            .or_else(|| obj.get("brief_ready"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let brief_note = obj
            .get("briefNote")
            .or_else(|| obj.get("brief_note"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        out.push(ParsedCandidate {
            title,
            evidence,
            suggestion,
            cost,
            brief_ready,
            brief_note,
        });
    }
    Some(out)
}

fn try_parse_scout_json(trimmed: &str) -> Option<Vec<ParsedCandidate>> {
    let mut json_slice: Option<&str> = None;
    if let Some(start) = trimmed.find("```json") {
        let after = &trimmed[start + 7..];
        if let Some(end) = after.find("```") {
            json_slice = Some(after[..end].trim());
        }
    } else if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        let after = after.split_once('\n').map(|(_, rest)| rest).unwrap_or(after);
        if let Some(end) = after.find("```") {
            let candidate = after[..end].trim();
            if candidate.starts_with('[') || candidate.starts_with('{') {
                json_slice = Some(candidate);
            }
        }
    }

    if let Some(slice) = json_slice {
        if let Ok(val) = serde_json::from_str::<Value>(slice) {
            if let Some(parsed) = parse_json_candidates(&val) {
                return Some(parsed);
            }
        }
    }

    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
            if let Some(parsed) = parse_json_candidates(&val) {
                return Some(parsed);
            }
        }
    } else if let (Some(l), Some(r)) = (trimmed.find('['), trimmed.rfind(']')) {
        if l < r {
            let candidate = &trimmed[l..=r];
            if candidate.contains("\"title\"") || candidate.trim() == "[]" {
                if let Ok(val) = serde_json::from_str::<Value>(candidate) {
                    if let Some(parsed) = parse_json_candidates(&val) {
                        return Some(parsed);
                    }
                }
            }
        }
    }
    None
}

fn strip_ordered_item_prefix(line: &str) -> Option<&str> {
    if line.starts_with(' ') || line.starts_with('\t') {
        return None;
    }
    let idx = line.find(['.', ')'])?;
    if idx == 0 || idx > 3 || !line[..idx].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let after = &line[idx + 1..];
    if after.starts_with(char::is_whitespace) {
        Some(after.trim())
    } else {
        None
    }
}

fn strip_field_label<'a>(line: &'a str, labels: &[&str]) -> Option<&'a str> {
    let cleaned = line
        .trim()
        .trim_start_matches(['-', '*', '•', '+', ' '])
        .trim();
    let no_bold = cleaned.trim_start_matches("**");
    for lbl in labels {
        if let Some(rest) = no_bold.strip_prefix(lbl) {
            let r = rest.trim_start_matches(['*', '_', '`', ' ']);
            if let Some(after) = r
                .strip_prefix('：')
                .or_else(|| r.strip_prefix(':'))
                .or_else(|| r.strip_prefix('—'))
                .or_else(|| r.strip_prefix('–'))
            {
                return Some(after.trim());
            }
        }
        let lower = no_bold.to_ascii_lowercase();
        if lower.starts_with(&lbl.to_ascii_lowercase()) {
            let rest = &no_bold[lbl.len()..];
            let r = rest.trim_start_matches(['*', '_', '`', ' ']);
            if let Some(after) = r
                .strip_prefix(':')
                .or_else(|| r.strip_prefix('：'))
                .or_else(|| r.strip_prefix('—'))
                .or_else(|| r.strip_prefix('–'))
            {
                return Some(after.trim());
            }
        }
    }
    None
}

fn split_prose_title_and_inline(
    raw_header: &str,
    is_heading: bool,
    has_body: bool,
) -> (String, Option<String>) {
    let cleaned = raw_header
        .trim()
        .trim_start_matches('#')
        .trim()
        .trim_matches(['*', '_', ' ']);
    // Keep section heading titles intact when a body follows (e.g. `### 回归风险：未处理空数组`
    // must NOT be truncated to `回归风险` and collided across items). Only split inline body on
    // list items or standalone single-line headers.
    if !is_heading || !has_body {
        for sep in ["**：", "**:", " — ", " – ", "："] {
            if let Some((lhs, rhs)) = raw_header.trim().split_once(sep) {
                let title = lhs
                    .trim()
                    .trim_start_matches('#')
                    .trim()
                    .trim_matches(['*', '_', '`', '[', ']', '【', '】', ' '])
                    .to_string();
                let rest = rhs.trim().replace("**", "");
                if !title.is_empty() && !rest.is_empty() {
                    return (title, Some(rest));
                }
            }
        }
    }
    let title = cleaned
        .replace("**", "")
        .trim_matches(['[', ']', '【', '】', ' '])
        .to_string();
    (title, None)
}

fn is_candidate_field_line(line: &str) -> bool {
    strip_field_label(
        line,
        &[
            "证据锚点",
            "证据",
            "锚点",
            "依据",
            "evidence",
            "anchor",
            "建议做法",
            "建议动作",
            "建议",
            "做法",
            "动作",
            "修复",
            "suggestion",
            "action",
            "fix",
            "粗估成本",
            "成本",
            "工作量",
            "耗时",
            "cost",
            "effort",
            "可直接写成任务说明",
            "可直派",
            "任务说明",
            "briefReady",
            "brief ready",
            "brief",
        ],
    )
    .is_some()
}

fn parse_prose_candidate_block(
    header: &str,
    body_lines: &[String],
    is_heading: bool,
) -> Option<ParsedCandidate> {
    let has_body = body_lines.iter().any(|l| !l.trim().is_empty());
    let (title, inline_body) = split_prose_title_and_inline(header, is_heading, has_body);
    if title.is_empty() || is_radar_none_reply(&title) {
        return None;
    }

    let mut all_lines: Vec<String> = Vec::new();
    if let Some(inline) = inline_body {
        all_lines.push(inline);
    }
    all_lines.extend(body_lines.iter().cloned());

    let mut evidence: Vec<String> = Vec::new();
    let mut suggestion = String::new();
    let mut cost = String::new();
    let mut brief_ready = false;
    let mut brief_note = String::new();
    let mut fallback_desc: Vec<String> = Vec::new();

    for raw in &all_lines {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "---" || trimmed == "***" {
            continue;
        }
        if let Some(ev_val) = strip_field_label(trimmed, &["证据锚点", "证据", "锚点", "依据", "evidence", "anchor"]) {
            if !ev_val.is_empty() && is_valid_evidence_anchor(ev_val) && !evidence.contains(&ev_val.to_string()) {
                evidence.push(ev_val.to_string());
            }
            continue;
        }
        if let Some(sug_val) = strip_field_label(
            trimmed,
            &["建议做法", "建议动作", "建议", "做法", "动作", "修复", "suggestion", "action", "fix"],
        ) {
            if suggestion.is_empty() && !sug_val.is_empty() {
                suggestion = sug_val.replace("**", "").trim().to_string();
            }
            if is_valid_evidence_anchor(sug_val) && !evidence.contains(&sug_val.to_string()) {
                evidence.push(sug_val.to_string());
            }
            continue;
        }
        if let Some(cost_val) = strip_field_label(trimmed, &["粗估成本", "成本", "工作量", "耗时", "cost", "effort"]) {
            if cost.is_empty() && !cost_val.is_empty() {
                cost = cost_val.replace("**", "").trim().to_string();
            }
            continue;
        }
        if let Some(ready_val) = strip_field_label(
            trimmed,
            &["可直接写成任务说明", "可直派", "任务说明", "briefReady", "brief ready", "brief"],
        ) {
            let clean = ready_val.replace("**", "").trim().to_string();
            let lower = clean.to_ascii_lowercase();
            let neg = lower.starts_with("no")
                || lower.starts_with("false")
                || clean.starts_with('否')
                || clean.starts_with("不能")
                || clean.starts_with("不可")
                || clean.starts_with("需细化")
                || clean.starts_with("暂不能");
            let pos = lower.starts_with("yes")
                || lower.starts_with("true")
                || clean.starts_with('是')
                || clean.starts_with("可以")
                || clean.starts_with("可直派")
                || clean.starts_with("能");
            if pos && !neg {
                brief_ready = true;
            }
            if let Some((_, note)) = clean
                .split_once('（')
                .or_else(|| clean.split_once('('))
                .or_else(|| clean.split_once('，'))
                .or_else(|| clean.split_once(','))
                .or_else(|| clean.split_once('：'))
            {
                brief_note = note.trim_end_matches(['）', ')', '。', ' ']).trim().to_string();
            }
            continue;
        }

        let item_clean = trimmed
            .trim_start_matches(['-', '*', '•', '+', ' '])
            .replace("**", "")
            .trim()
            .to_string();
        if !item_clean.is_empty() {
            if is_valid_evidence_anchor(&item_clean) && !evidence.contains(&item_clean) {
                evidence.push(item_clean.clone());
            }
            fallback_desc.push(item_clean);
        }
    }

    // Also check if the title itself carried a concrete evidence anchor.
    if evidence.is_empty() && is_valid_evidence_anchor(&title) {
        evidence.push(title.clone());
    }

    if suggestion.is_empty() && !fallback_desc.is_empty() {
        suggestion = clip(&fallback_desc.into_iter().take(2).collect::<Vec<_>>().join(" "), 240);
    }
    if cost.is_empty() {
        cost = "未估".to_string();
    }

    Some(ParsedCandidate {
        title,
        evidence,
        suggestion,
        cost,
        brief_ready,
        brief_note,
    })
}

fn parse_prose_candidates(answer: &str) -> Vec<ParsedCandidate> {
    // Strip fenced code blocks first.
    let mut fence: Option<(char, usize)> = None;
    let mut lines: Vec<&str> = Vec::new();
    for line in answer.lines() {
        if let Some((fc, flen)) = fence {
            if let Some((c, len)) = radar_fence_marker(line) {
                if c == fc && len >= flen && line.trim_start()[len..].trim().is_empty() {
                    fence = None;
                }
            }
            continue;
        } else if let Some(m) = radar_fence_marker(line) {
            fence = Some(m);
            continue;
        }
        lines.push(line);
    }

    // 1. Prefer `### ` sections; if none, try `## ` sections unless the `## ` heading is just a
    // wrapper around a top-level ordered or multi-item candidate list.
    let has_h3 = lines.iter().any(|l| l.trim_start().starts_with("### "));
    let heading_prefix = if has_h3 {
        Some("### ")
    } else if lines.iter().any(|l| l.trim_start().starts_with("## ")) {
        Some("## ")
    } else {
        None
    };

    let mut blocks: Vec<(String, Vec<String>, bool)> = Vec::new();
    if let Some(prefix) = heading_prefix {
        let mut cur_header: Option<String> = None;
        let mut cur_body: Vec<String> = Vec::new();
        for &line in &lines {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                if let Some(hdr) = cur_header.take() {
                    blocks.push((hdr, std::mem::take(&mut cur_body), true));
                }
                cur_header = Some(rest.trim().to_string());
            } else if trimmed.starts_with("# ") || (prefix == "### " && trimmed.starts_with("## ")) {
                if let Some(hdr) = cur_header.take() {
                    blocks.push((hdr, std::mem::take(&mut cur_body), true));
                }
            } else if cur_header.is_some() {
                cur_body.push(line.to_string());
            }
        }
        if let Some(hdr) = cur_header {
            blocks.push((hdr, cur_body, true));
        }

        // If there are no `### ` headings and the `## ` wrapper simply contains an ordered list
        // (e.g. `## 本次新增候选\n1. ...\n2. ...`) or a single `## ` wraps multiple non-field bullet
        // candidates, drop the heading wrapper so list splitting extracts every item individually.
        if prefix == "## " {
            let ordered_items = lines
                .iter()
                .filter(|l| strip_ordered_item_prefix(l).is_some())
                .count();
            let single_h2_wraps_bullets = blocks.len() == 1 && {
                let body = &blocks[0].1;
                let top_bullets: Vec<&str> = body
                    .iter()
                    .filter_map(|l| l.strip_prefix("- ").or_else(|| l.strip_prefix("* ")))
                    .collect();
                let non_field_bullets = top_bullets
                    .iter()
                    .filter(|b| !is_candidate_field_line(b))
                    .count();
                top_bullets.len() >= 2 && non_field_bullets >= 2
            };
            if ordered_items >= 2 || (blocks.len() == 1 && ordered_items >= 1) || single_h2_wraps_bullets {
                blocks.clear();
            }
        }
    }

    // 2. Fallback to top-level ordered list (`1. `, `2) `).
    if blocks.is_empty() {
        let mut cur_header: Option<String> = None;
        let mut cur_body: Vec<String> = Vec::new();
        for &line in &lines {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                if let Some(hdr) = cur_header.take() {
                    blocks.push((hdr, std::mem::take(&mut cur_body), false));
                }
                continue;
            }
            if let Some(rest) = strip_ordered_item_prefix(line) {
                if let Some(hdr) = cur_header.take() {
                    blocks.push((hdr, std::mem::take(&mut cur_body), false));
                }
                cur_header = Some(rest.to_string());
            } else if cur_header.is_some() {
                cur_body.push(line.to_string());
            }
        }
        if let Some(hdr) = cur_header {
            blocks.push((hdr, cur_body, false));
        }
    }

    // 3. Fallback to top-level unordered list (`- ` / `* ` without indentation).
    if blocks.is_empty() {
        let mut cur_header: Option<String> = None;
        let mut cur_body: Vec<String> = Vec::new();
        for &line in &lines {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                if let Some(hdr) = cur_header.take() {
                    blocks.push((hdr, std::mem::take(&mut cur_body), false));
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
                if let Some(hdr) = cur_header.take() {
                    blocks.push((hdr, std::mem::take(&mut cur_body), false));
                }
                cur_header = Some(rest.trim().to_string());
            } else if cur_header.is_some() {
                cur_body.push(line.to_string());
            }
        }
        if let Some(hdr) = cur_header {
            blocks.push((hdr, cur_body, false));
        }
    }

    blocks
        .into_iter()
        .filter_map(|(hdr, body, is_heading)| parse_prose_candidate_block(&hdr, &body, is_heading))
        .collect()
}

/// Extract candidates from the scout's answer: accepts legacy JSON arrays (raw or fenced)
/// as well as natural prose sections (`### <title>` or top-level ordered/unordered lists).
/// Never fails the whole run solely because the scout replied in prose instead of JSON.
pub fn parse_scout_candidates(answer: &str) -> Res<Vec<ParsedCandidate>> {
    let trimmed = answer.trim();
    if trimmed.is_empty() || is_radar_none_reply(trimmed) {
        return Ok(Vec::new());
    }
    if let Some(json_items) = try_parse_scout_json(trimmed) {
        return Ok(json_items);
    }
    Ok(parse_prose_candidates(trimmed))
}

fn resolve_scout_config(config_val: &Value) -> Res<Value> {
    let radar_val = &config_val["radar"];
    let tier = radar_val["tier"]
        .as_str()
        .filter(|t| !t.is_empty())
        .unwrap_or("scout");
    let explicit_model = radar_val["model"].as_str().filter(|m| !m.is_empty());
    let explicit_thinking = radar_val["thinking"].as_str().filter(|t| !t.is_empty());
    let listed = agents::pi_models();
    let (model, model_name, thinking) = if let Some(name) = explicit_model {
        let resolved = models::resolve_model(name, config_val, &listed)?;
        let think = models::thinking(explicit_thinking, &json!(name), config_val)?;
        (resolved, name.to_string(), think)
    } else {
        let pick = models::pick(
            config_val,
            tier,
            explicit_thinking,
            &listed,
            agents::local_clock(),
            &|_| 0,
        )?;
        (pick.model, pick.name, pick.thinking)
    };
    let colleague = crate::agy::colleague(&model);
    let (agent_bin, agent_version) = agents::agent_identity(colleague)?;
    Ok(json!({
        "tier": tier,
        "model": model,
        "modelName": model_name,
        "thinking": thinking,
        "agent": colleague,
        "agentBin": agent_bin,
        "agentVersion": agent_version,
    }))
}

fn build_scout_prompt(
    repo: &Path,
    collect_text: &str,
    collect_file: &Path,
    prev_board_md: &str,
) -> String {
    let collect_section = if collect_text.len() <= MAX_COLLECT_IN_PROMPT_BYTES {
        collect_text.to_string()
    } else {
        let excerpt: String = collect_text.chars().take(60_000).collect();
        format!(
            "（采集输出共 {} 字节，已截断；完整采集文件：`{}`）\n\n{excerpt}\n...",
            collect_text.len(),
            collect_file.display()
        )
    };
    let board_section = if prev_board_md.trim().is_empty() {
        "（暂无历史候选板）".to_string()
    } else if prev_board_md.len() <= MAX_BOARD_IN_PROMPT_BYTES {
        prev_board_md.to_string()
    } else {
        let excerpt: String = prev_board_md.chars().take(15_000).collect();
        format!("{excerpt}\n...（已截断）")
    };

    format!(
        "你是发散侦察员（Radar Scout）。请基于本次确定性采集输出与已有候选板，提出值得主控关注的新候选事项（缺口、回归风险、异常、可清理债、待收敛项）。\n\
         严格遵守以下边界：\n\
         1. **只读**：严禁修改、创建或删除任何文件；严禁调用 `delegate` 派发子任务。\n\
         2. **不重复已有候选**：已在上一份候选板中列出的事项不要重复提出。\n\
         3. **自然小节输出**：每条候选以 `### <简短标题>`（或有序列表 `1. <简短标题>`）开头，自然写清发现的问题、带行号/commit 或采集输出行号（如 `文件:行号`、`commit <短哈希>`、`collect#1:L12`）的证据、建议动作、粗估成本及能否直接写成任务说明；无值得新增的候选直接写「无」。\n\n\
         ## 仓库\n\n`{}`\n\n\
         ## 本次确定性采集输出\n\n{}\n\n\
         ## 上一份候选板\n\n{}\n",
        repo.display(),
        collect_section.trim(),
        board_section.trim(),
    )
}

fn run_scout_agent(
    repo: &Path,
    cfg_val: &Value,
    scout_cfg: &Value,
    work_dir: &Path,
    prompt_file: &Path,
) -> Res<String> {
    let is_agy = crate::agy::is_agy(s(scout_cfg, "model"));
    let ro_workspace = work_dir.join("ro-workspace");
    let excludes: Vec<String> = Vec::new();
    let head_tree = git_text(repo, &["rev-parse", "HEAD^{tree}"]).unwrap_or_default();

    let (agent_workdir, before_pi_snap, before_agy_manifest) =
        if is_agy && !head_tree.is_empty() {
            review::prepare_ro_workspace(repo, &head_tree, &ro_workspace, work_dir)?;
            let manifest = review::dir_manifest_for_repo(&ro_workspace, repo)?;
            (ro_workspace.clone(), None, Some(manifest))
        } else {
            let snap = changes::snapshot(repo, work_dir, &excludes);
            (repo.to_path_buf(), snap, None)
        };

    let run_id = format!("radar-{}", stamp());
    let mut scout_meta = json!({
        "run": run_id,
        "name": "radar scout",
        "dir": work_dir,
        "workdir": agent_workdir,
        "mode": "read-only",
        "shell": false,
        "isRadarScout": true,
        "agent": scout_cfg["agent"],
        "agentBin": scout_cfg["agentBin"],
        "agentVersion": scout_cfg["agentVersion"],
        "tier": scout_cfg["tier"],
        "model": scout_cfg["model"],
        "modelName": scout_cfg["modelName"],
        "thinking": scout_cfg["thinking"],
        "provider": Value::Null,
        "sessionDir": work_dir.join("session"),
        "fork": Value::Null,
        "agentPromptFile": prompt_file,
        "images": [],
        "env": cfg_val["env"],
        "agy": cfg_val["agy"],
        "stall": cfg_val["stall"],
        "timeoutSeconds": 900.0,
        "top": repo,
        "agentDeny": [],
    });
    if s(&scout_meta, "agent").is_empty() {
        scout_meta["agent"] = json!(agents::AGENT);
    }
    crate::deny::prepare(&scout_meta, work_dir)?;

    let holder = Arc::new(AtomicI32::new(0));
    let grace = Arc::new(Mutex::new(None));
    let res = agents::run_agent(&scout_meta, work_dir, 1, holder, grace);

    // Clean up any leftover scout subprocesses.
    let pgid = read(work_dir.join("agent.pid")).trim().parse().unwrap_or(0);
    crate::cleanup::record(work_dir, pgid);
    if pgid > 0 && group_members(pgid) {
        end_group(pgid, 1.0);
    }

    // Fail closed if the scout violated its read-only boundary.
    let raw_ro_violations: Vec<String> = if let Some(before_m) = before_agy_manifest {
        let after_m = review::dir_manifest_for_repo(&ro_workspace, repo).unwrap_or_default();
        review::diff_manifests(&before_m, &after_m)
    } else if let Some(before_s) = before_pi_snap {
        if let Some(after_s) = changes::snapshot(repo, work_dir, &excludes) {
            if s(&before_s, "tree") != s(&after_s, "tree") {
                changes::tree_changes(repo, &before_s, &after_s, false)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| s(&c, "path").to_string())
                    .collect()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    let ro_violations = review::filter_ro_violations(repo, raw_ro_violations);

    if ro_workspace.exists() {
        let _ = fs::remove_dir_all(&ro_workspace);
    }

    if !ro_violations.is_empty() {
        return Err(format!(
            "radar scout violated read-only boundary and modified files: {}",
            ro_violations.join(", ")
        ));
    }

    match res {
        Ok((outcome, text)) if outcome == "ok" => Ok(text),
        Ok((outcome, text)) => Err(format!(
            "radar scout ended with outcome {outcome}: {}",
            clip(&text, 200)
        )),
        Err(e) => Err(format!("radar scout failed: {e}")),
    }
}

fn render_board_md(board: &Value) -> String {
    let repo = s(board, "repo");
    let repo_name = s(board, "repoName");
    let updated_at = s(board, "updatedAt");
    let collect_fp = s(board, "lastFingerprint");
    let candidates = board["candidates"].as_array();
    let count = candidates.map_or(0, Vec::len);

    let mut md = String::new();
    md.push_str(&format!("# Radar 候选板：{repo_name}\n\n"));
    md.push_str(&format!("- 仓库：`{repo}`\n"));
    md.push_str(&format!("- 更新时间：{updated_at}\n"));
    md.push_str(&format!("- 采集指纹：`{collect_fp}`\n"));
    md.push_str(&format!("- 当前候选：{count} 条\n\n"));
    let last_note = s(board, "lastNote");
    if count == 0 {
        md.push_str("暂无候选事项。\n");
        if !last_note.is_empty() {
            md.push_str(&format!("\n> 注：{last_note}\n"));
        }
        return md;
    }

    for item in candidates.into_iter().flatten() {
        let id = s(item, "id");
        let title = s(item, "title");
        let suggestion = s(item, "suggestion");
        let cost = s(item, "cost");
        let brief_ready = b(item, "briefReady");
        let brief_note = s(item, "briefNote");
        let fp = s(item, "fingerprint");
        let first_seen = s(item, "firstSeenAt");
        let picked_at = s(item, "pickedAt");

        md.push_str(&format!("## [{id}] {title}\n\n"));
        md.push_str(&format!(
            "- **证据**：{}\n",
            strings(&item["evidence"])
                .into_iter()
                .map(|e| format!("`{e}`"))
                .collect::<Vec<_>>()
                .join("；")
        ));
        md.push_str(&format!("- **建议做法**：{suggestion}\n"));
        md.push_str(&format!("- **粗估成本**：{cost}\n"));
        let ready_text = if brief_ready { "是" } else { "否" };
        if brief_note.is_empty() {
            md.push_str(&format!("- **可直接写成任务说明**：{ready_text}\n"));
        } else {
            md.push_str(&format!(
                "- **可直接写成任务说明**：{ready_text}（{brief_note}）\n"
            ));
        }
        md.push_str(&format!(
            "- **指纹 / 首次发现**：`{}` · {}\n",
            &fp[..fp.len().min(19)],
            first_seen
        ));
        if !picked_at.is_empty() {
            md.push_str(&format!("- **已生成草稿**：{picked_at}\n"));
        }
        md.push_str(&format!("- **挑选生成草稿**：`delegate radar pick {id}`\n\n"));
    }
    if !last_note.is_empty() {
        md.push_str(&format!("> 注：{last_note}\n"));
    }
    md
}

fn load_board_doc(radar_dir: &Path, repo_canon: &Path) -> Value {
    let p = radar_dir.join("board.json");
    let doc = json(&p);
    if doc.is_object() {
        return doc;
    }
    let repo_name = repo_canon
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
        .to_string();
    json!({
        "repo": repo_canon.to_string_lossy(),
        "repoName": repo_name,
        "updatedAt": "",
        "lastFingerprint": "",
        "candidates": [],
    })
}

fn save_board_doc(radar_dir: &Path, board: &Value) -> Res<PathBuf> {
    fs::create_dir_all(radar_dir).map_err(|e| e.to_string())?;
    write_json(radar_dir.join("board.json"), board)?;
    let board_md_path = radar_dir.join("board.md");
    write(&board_md_path, render_board_md(board))?;
    Ok(board_md_path)
}

pub fn load_all_candidates(repo_filter: Option<&Path>) -> Vec<Value> {
    let root = radar_root();
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let filter_canon = repo_filter.map(canonical_repo);
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let board = json(dir.join("board.json"));
        if !board.is_object() {
            continue;
        }
        let repo_str = s(&board, "repo");
        if let Some(ref want) = filter_canon {
            if repo_str.is_empty() || canonical_repo(Path::new(repo_str)) != *want {
                continue;
            }
        }
        let repo_name = s(&board, "repoName").to_string();
        let board_md = dir.join("board.md").to_string_lossy().into_owned();
        for item in board["candidates"].as_array().into_iter().flatten() {
            let mut c = item.clone();
            c["repo"] = json!(repo_str);
            c["repoName"] = json!(repo_name);
            c["boardPath"] = json!(board_md);
            out.push(c);
        }
    }
    out.sort_by(|a, b| {
        s(b, "lastSeenAt")
            .cmp(s(a, "lastSeenAt"))
            .then_with(|| s(a, "id").cmp(s(b, "id")))
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
                    "delegate radar list [--all] [--json]",
                ));
            }
        }
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let filter = if all { None } else { git_top(&cwd) };
    let candidates = load_all_candidates(filter.as_deref());
    if json_mode {
        for c in &candidates {
            println!("{c}");
        }
        return Ok(0);
    }
    if candidates.is_empty() {
        println!("候选板为空（先配置 .delegate.json 的 radar.collect 并运行 delegate radar）");
        return Ok(0);
    }
    for c in &candidates {
        let id = s(c, "id");
        let repo_name = s(c, "repoName");
        let title = s(c, "title");
        let cost = s(c, "cost");
        let ready = if b(c, "briefReady") { "可直派" } else { "需细化" };
        let ev = strings(&c["evidence"])
            .into_iter()
            .take(2)
            .collect::<Vec<_>>()
            .join(", ");
        println!("{id}  [{repo_name}]  [{ready} | {cost}]  {title}  ({ev})");
    }
    Ok(0)
}

fn render_pick_draft(candidate: &Value, repo: &str) -> String {
    let id = s(candidate, "id");
    let title = s(candidate, "title");
    let suggestion = s(candidate, "suggestion");
    let cost = s(candidate, "cost");
    let brief_ready = b(candidate, "briefReady");
    let brief_note = s(candidate, "briefNote");
    let evidence = strings(&candidate["evidence"]);

    let mut md = String::new();
    md.push_str(&format!("# {title}\n\n"));
    md.push_str(&format!(
        "> 来源：Radar 候选 `{id}`（仓库 `{repo}`，粗估成本：{cost}，可直接写成任务说明：{}）\n\n",
        if brief_ready {
            "是".to_string()
        } else if brief_note.is_empty() {
            "否（请主控补充边界与验收）".to_string()
        } else {
            format!("否：{brief_note}")
        }
    ));
    md.push_str("## 目标与建议做法\n\n");
    if suggestion.is_empty() {
        md.push_str("（请主控补充具体目标与改法）\n\n");
    } else {
        md.push_str(&format!("{suggestion}\n\n"));
    }
    md.push_str("## 证据锚点\n\n");
    for ev in &evidence {
        md.push_str(&format!("- `{ev}`\n"));
    }
    md.push_str("\n## 约束与范围（主控核对后填写）\n\n");
    md.push_str("- 可改范围：\n");
    md.push_str("- 禁止触碰：受保护路径、ADR、模型可见文字（除非主控明确放开）\n\n");
    md.push_str("## 完成标准\n\n");
    md.push_str("- [ ] 相关测试通过\n");
    md.push_str("- [ ] 仓库验收命令通过\n\n");
    md.push_str("## 派发参考（本命令不会自动派活）\n\n");
    md.push_str("```bash\n");
    md.push_str(&format!(
        "delegate start --worktree --name {} --prompt-file <本草稿路径>\n",
        slug(title)
    ));
    md.push_str("```\n");
    md
}

fn pick_command(args: &[String]) -> Res<i32> {
    let (pos, flags, opts) = parse_simple(
        "delegate radar pick <id> [--out PATH] [--json]",
        args,
        &["--json"],
        &["--out", "-o"],
    )?;
    if has(&flags, "--json") {
        output::enable_json();
    }
    if pos.len() != 1 {
        return Err("usage: delegate radar pick <id> [--out PATH] [--json]".into());
    }
    let query = pos[0].trim();
    if query.is_empty() {
        return Err("radar pick requires a non-empty candidate id".into());
    }

    let root = radar_root();
    let cwd = std::env::current_dir().unwrap_or_default();
    let cur_repo = git_top(&cwd).map(|p| canonical_repo(&p));

    // Search current repo first, then all repos under `<state>/radar/`.
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(ref repo) = cur_repo {
        let d = repo_radar_dir(repo);
        if d.join("board.json").is_file() {
            dirs.push(d);
        }
    }
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            let d = entry.path();
            if d.is_dir() && d.join("board.json").is_file() && !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }

    let mut matches: Vec<(PathBuf, usize, Value)> = Vec::new();
    for d in &dirs {
        let board = json(d.join("board.json"));
        for (idx, item) in board["candidates"].as_array().into_iter().flatten().enumerate() {
            let id = s(item, "id");
            if id.eq_ignore_ascii_case(query) {
                matches.clear();
                matches.push((d.clone(), idx, item.clone()));
                break;
            }
            if id.to_ascii_lowercase().starts_with(&query.to_ascii_lowercase()) {
                matches.push((d.clone(), idx, item.clone()));
            }
        }
        if matches.len() == 1 && s(&matches[0].2, "id").eq_ignore_ascii_case(query) {
            break;
        }
    }

    if matches.is_empty() {
        return Err(format!("radar candidate not found: {query}"));
    }
    if matches.len() > 1 {
        let ids: Vec<String> = matches
            .iter()
            .map(|(_, _, item)| s(item, "id").to_string())
            .collect();
        return Err(format!(
            "ambiguous radar candidate id {query:?}, matches: {}",
            ids.join(", ")
        ));
    }

    let (radar_dir, cand_idx, mut cand) = matches.remove(0);
    let mut board = json(radar_dir.join("board.json"));
    let repo_str = s(&board, "repo").to_string();
    let picked_at = iso();
    cand["pickedAt"] = json!(&picked_at);
    if let Some(arr) = board["candidates"].as_array_mut() {
        if let Some(slot) = arr.get_mut(cand_idx) {
            slot["pickedAt"] = json!(&picked_at);
        }
    }
    let _ = save_board_doc(&radar_dir, &board);

    let draft = render_pick_draft(&cand, &repo_str);
    let out_path = opt(&opts, "--out").or_else(|| opt(&opts, "-o"));
    let saved_file = if let Some(p) = out_path {
        let dst = PathBuf::from(p);
        if let Some(parent) = dst.parent().filter(|x| !x.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        write(&dst, &draft)?;
        Some(dst.to_string_lossy().into_owned())
    } else {
        None
    };

    if output::json_enabled() {
        println!(
            "{}",
            json!({
                "id": s(&cand, "id"),
                "repo": repo_str,
                "title": s(&cand, "title"),
                "pickedAt": picked_at,
                "outFile": saved_file,
                "draft": draft,
            })
        );
    } else if let Some(path) = saved_file {
        println!("已生成任务说明草稿（未自动派活）：{path}");
    } else {
        print!("{draft}");
    }
    Ok(0)
}

fn schedule_command(args: &[String]) -> Res<i32> {
    let (_, _, opts) = parse_simple("delegate radar schedule [--repo PATH]", args, &[], &["--repo"])?;
    let cwd = std::env::current_dir().unwrap_or_default();
    let repo = opt(&opts, "--repo")
        .map(PathBuf::from)
        .or_else(|| git_top(&cwd))
        .unwrap_or(cwd);
    let repo_canon = canonical_repo(&repo);
    let repo_name = repo_canon
        .file_name()
        .and_then(|n| n.to_str())
        .map(slug)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "repo".into());
    let bin = script();

    println!("# Delegate Radar 定时安装说明（默认 00:30–04:30 每两小时一次；不会自动安装）");
    println!();
    println!("## 方式一：systemd user timer（推荐）");
    println!();
    println!("1. 写入 `~/.config/systemd/user/delegate-radar-{repo_name}.service`：");
    println!("```ini");
    println!("[Unit]");
    println!("Description=Delegate Radar scout for {repo_name}");
    println!();
    println!("[Service]");
    println!("Type=oneshot");
    println!("WorkingDirectory={}", repo_canon.display());
    println!("ExecStart={} radar", bin.display());
    println!("```");
    println!();
    println!("2. 写入 `~/.config/systemd/user/delegate-radar-{repo_name}.timer`：");
    println!("```ini");
    println!("[Unit]");
    println!("Description=Run Delegate Radar for {repo_name} (00:30-04:30 every 2h)");
    println!();
    println!("[Timer]");
    println!("OnCalendar=*-*-* 00,02,04:30:00");
    println!("Persistent=true");
    println!();
    println!("[Install]");
    println!("WantedBy=timers.target");
    println!("```");
    println!();
    println!("3. 手动启用：");
    println!("```bash");
    println!("systemctl --user daemon-reload");
    println!("systemctl --user enable --now delegate-radar-{repo_name}.timer");
    println!("```");
    println!();
    println!("## 方式二：crontab");
    println!();
    println!("运行 `crontab -e` 添加一行：");
    println!("```cron");
    println!(
        "30 0,2,4 * * * cd {} && {} radar >> ~/.local/state/delegate/radar/{repo_name}/cron.log 2>&1",
        repo_canon.display(),
        bin.display()
    );
    println!("```");
    Ok(0)
}

fn run_command(args: &[String]) -> Res<i32> {
    let usage = "delegate radar [run] [--force] [--dry-run] [--json]";
    let (pos, flags, _) = parse_simple(usage, args, &["--force", "--dry-run", "--json"], &[])?;
    if !pos.is_empty() {
        return Err(unrecognized_arg(
            &pos.join(" "),
            &["--force", "--dry-run", "--json"],
            usage,
        ));
    }
    if has(&flags, "--json") {
        output::enable_json();
    }
    let force = has(&flags, "--force");
    let dry_run = has(&flags, "--dry-run");

    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let repo = git_top(&cwd).unwrap_or(cwd);
    let repo_canon = canonical_repo(&repo);
    let repo_str = repo_canon.to_string_lossy().into_owned();
    let repo_name = repo_canon
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
        .to_string();

    let cfg = config::load(Some(&repo_canon))?;
    let specs = parse_collect_specs(&cfg.value["radar"]);
    if specs.is_empty() {
        return Err(
            "radar.collect is not configured in .delegate.json; add a non-empty radar.collect command list"
                .into(),
        );
    }

    let collect_timeout = cfg.value["radar"]["timeout"]
        .as_str()
        .and_then(|d| seconds(d).ok())
        .unwrap_or(300.0);
    let env_cfg = if cfg.value["env"].is_object() {
        cfg.value["env"].clone()
    } else {
        json!({})
    };

    let radar_dir = repo_radar_dir(&repo_canon);
    fs::create_dir_all(&radar_dir).map_err(|e| e.to_string())?;
    let run_scratch = radar_dir.join(format!("run-{}-{}", stamp(), std::process::id()));

    let (fingerprint, collect_text, sections) =
        run_collect(&repo_canon, &specs, &env_cfg, collect_timeout, &run_scratch)?;
    let collect_file = radar_dir.join("last-collect.txt");
    write(&collect_file, &collect_text)?;

    let state_path = radar_dir.join("state.json");
    let prev_state = json(&state_path);
    let prev_fp = s(&prev_state, "lastFingerprint").to_string();
    let unchanged = !prev_fp.is_empty() && prev_fp == fingerprint;

    if dry_run {
        let _ = fs::remove_dir_all(&run_scratch);
        if output::json_enabled() {
            println!(
                "{}",
                json!({
                    "dryRun": true,
                    "repo": repo_str,
                    "fingerprint": fingerprint,
                    "previousFingerprint": prev_fp,
                    "changed": !unchanged,
                    "sections": sections,
                    "collectFile": collect_file.to_string_lossy(),
                })
            );
        } else {
            println!(
                "Radar 采集（dry-run）：{} · 指纹 {}（{}）",
                repo_name,
                &fingerprint[..fingerprint.len().min(19)],
                if unchanged { "未变化" } else { "有变化" }
            );
            print!("{collect_text}");
        }
        return Ok(0);
    }

    // Fingerprint unchanged -> zero-token exit with a single event.
    if unchanged && !force {
        let _ = fs::remove_dir_all(&run_scratch);
        let now = iso();
        let mut st_doc = if prev_state.is_object() {
            prev_state
        } else {
            json!({})
        };
        st_doc["repoPath"] = json!(&repo_str);
        st_doc["repoName"] = json!(&repo_name);
        st_doc["lastFingerprint"] = json!(&fingerprint);
        st_doc["lastCheckedAt"] = json!(&now);
        write_json(&state_path, &st_doc)?;

        events::emit(
            &format!("radar-{repo_name}"),
            &repo_str,
            "radar_unchanged",
            json!({
                "repoName": repo_name,
                "fingerprint": fingerprint,
                "tokens": 0,
            }),
        );

        if output::json_enabled() {
            println!(
                "{}",
                json!({
                    "repo": repo_str,
                    "changed": false,
                    "fingerprint": fingerprint,
                    "tokens": 0,
                    "boardPath": radar_dir.join("board.md").to_string_lossy(),
                })
            );
        } else {
            println!(
                "Radar 指纹未变化（{}），零 token 退出。",
                &fingerprint[..fingerprint.len().min(19)]
            );
        }
        return Ok(0);
    }

    // Fingerprint changed (or --force) -> run read-only scout.
    let scout_cfg = resolve_scout_config(&cfg.value)?;
    let prev_board_md = read(radar_dir.join("board.md"));
    let prompt = build_scout_prompt(&repo_canon, &collect_text, &collect_file, &prev_board_md);
    let prompt_file = run_scratch.join("prompt.md");
    write(&prompt_file, &prompt)?;

    let raw_answer = match run_scout_agent(&repo_canon, &cfg.value, &scout_cfg, &run_scratch, &prompt_file) {
        Ok(text) => text,
        Err(e) => {
            let _ = write(radar_dir.join("last-error.txt"), &e);
            let _ = fs::remove_dir_all(&run_scratch);
            return Err(e);
        }
    };
    let last_output_path = radar_dir.join("last-scout-output.md");
    let _ = write(&last_output_path, &raw_answer);
    let _ = fs::remove_dir_all(&run_scratch);

    let parsed = parse_scout_candidates(&raw_answer)?;
    let parsed_empty = parsed.is_empty();
    let now = iso();
    let mut board = load_board_doc(&radar_dir, &repo_canon);
    board["repo"] = json!(&repo_str);
    board["repoName"] = json!(&repo_name);
    board["updatedAt"] = json!(&now);
    board["lastFingerprint"] = json!(&fingerprint);

    let existing = board["candidates"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut merged_candidates = existing;
    let mut known_fps: BTreeSet<String> = merged_candidates
        .iter()
        .map(|c| s(c, "fingerprint").to_string())
        .filter(|x| !x.is_empty())
        .collect();
    let mut known_titles: BTreeSet<String> = merged_candidates
        .iter()
        .map(|c| normalize_key(s(c, "title")))
        .filter(|x| !x.is_empty())
        .collect();
    let mut known_ids: BTreeSet<String> = merged_candidates
        .iter()
        .map(|c| s(c, "id").to_string())
        .collect();

    let mut added_count: usize = 0;
    let mut deduped_count: usize = 0;
    let mut dropped_no_anchor: usize = 0;

    for cand in parsed {
        if !has_valid_evidence_anchor(&cand.evidence) {
            dropped_no_anchor += 1;
            continue;
        }
        let fp = candidate_fingerprint(&cand.title, &cand.evidence);
        let norm_title = normalize_key(&cand.title);
        if known_fps.contains(&fp) || known_titles.contains(&norm_title) {
            // Update lastSeenAt on the existing entry instead of duplicating.
            for item in &mut merged_candidates {
                if s(item, "fingerprint") == fp || normalize_key(s(item, "title")) == norm_title {
                    item["lastSeenAt"] = json!(&now);
                    break;
                }
            }
            deduped_count += 1;
            continue;
        }

        let hex_part = fp.strip_prefix("sha256:").unwrap_or(&fp);
        let mut id = format!("r-{}", &hex_part[..hex_part.len().min(8)]);
        let mut suffix = 2u32;
        while known_ids.contains(&id) {
            id = format!("r-{}-{suffix}", &hex_part[..hex_part.len().min(8)]);
            suffix += 1;
        }
        known_ids.insert(id.clone());
        known_fps.insert(fp.clone());
        known_titles.insert(norm_title);
        added_count += 1;

        merged_candidates.push(json!({
            "id": id,
            "title": cand.title,
            "evidence": cand.evidence,
            "suggestion": cand.suggestion,
            "cost": cand.cost,
            "briefReady": cand.brief_ready,
            "briefNote": cand.brief_note,
            "fingerprint": fp,
            "collectFingerprint": fingerprint,
            "firstSeenAt": now,
            "lastSeenAt": now,
            "pickedAt": "",
        }));
    }

    board["candidates"] = json!(merged_candidates);
    if dropped_no_anchor > 0 {
        board["lastNote"] = json!(format!(
            "本轮有 {dropped_no_anchor} 条候选未提取到具体证据锚点，已跳过入板；完整侦察原文见 `{}`",
            last_output_path.display()
        ));
    } else if parsed_empty && !raw_answer.trim().is_empty() && !is_radar_none_reply(&raw_answer) {
        board["lastNote"] = json!(format!(
            "本轮侦察未提取到带证据锚点的候选条目；完整侦察原文见 `{}`",
            last_output_path.display()
        ));
    } else if let Some(obj) = board.as_object_mut() {
        obj.remove("lastNote");
    }
    let total = board["candidates"].as_array().map_or(0, Vec::len);
    let board_path = save_board_doc(&radar_dir, &board)?;

    let st_doc = json!({
        "repoPath": repo_str,
        "repoName": repo_name,
        "lastFingerprint": fingerprint,
        "lastCheckedAt": now,
        "lastScoutAt": now,
        "model": scout_cfg["model"],
        "tier": scout_cfg["tier"],
        "added": added_count,
        "deduped": deduped_count,
        "droppedNoAnchor": dropped_no_anchor,
        "total": total,
        "boardPath": board_path.to_string_lossy(),
    });
    write_json(&state_path, &st_doc)?;

    events::emit(
        &format!("radar-{repo_name}"),
        &repo_str,
        "radar_done",
        json!({
            "repoName": repo_name,
            "fingerprint": fingerprint,
            "model": scout_cfg["model"],
            "added": added_count,
            "deduped": deduped_count,
            "droppedNoAnchor": dropped_no_anchor,
            "total": total,
            "boardPath": board_path.to_string_lossy(),
        }),
    );

    if output::json_enabled() {
        println!(
            "{}",
            json!({
                "repo": repo_str,
                "changed": true,
                "fingerprint": fingerprint,
                "model": scout_cfg["model"],
                "tier": scout_cfg["tier"],
                "added": added_count,
                "deduped": deduped_count,
                "droppedNoAnchor": dropped_no_anchor,
                "total": total,
                "boardPath": board_path.to_string_lossy(),
            })
        );
    } else {
        println!(
            "Radar 侦察完成（{}）：新增 {added_count} 条 · 去重 {deduped_count} 条 · 无锚点丢弃 {dropped_no_anchor} 条 · 候选板共 {total} 条",
            repo_name
        );
        println!("候选板：{}", board_path.display());
    }
    Ok(0)
}

/// Entry point for `delegate radar`:
/// - `delegate radar [run] [--force] [--dry-run] [--json]`
/// - `delegate radar list|board [--all] [--json]`
/// - `delegate radar pick <id> [--out PATH] [--json]`
/// - `delegate radar schedule [--repo PATH]`
pub fn command(args: &[String]) -> Res<i32> {
    match args.first().map(String::as_str) {
        Some("run") => run_command(&args[1..]),
        Some("list" | "board") => list_command(&args[1..]),
        Some("pick") => pick_command(&args[1..]),
        Some("schedule") => schedule_command(&args[1..]),
        _ => run_command(args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_anchor_validation_accepts_concrete_anchors_and_rejects_vague_text() {
        assert!(is_valid_evidence_anchor("crates/foo/src/bar.rs:42"));
        assert!(is_valid_evidence_anchor("src/app.ts:10-25"));
        assert!(is_valid_evidence_anchor("README.md#L15"));
        assert!(is_valid_evidence_anchor("commit 61951c8"));
        assert!(is_valid_evidence_anchor("commit: abcdef123456"));
        assert!(is_valid_evidence_anchor("collect#2:L18"));
        assert!(is_valid_evidence_anchor("cmd#1:5"));
        assert!(is_valid_evidence_anchor("命令输出第12行"));
        assert!(is_valid_evidence_anchor("see line 9 of output"));
        assert!(is_valid_evidence_anchor("L42"));

        // Vague references without concrete anchor must be rejected.
        assert!(!is_valid_evidence_anchor(""));
        assert!(!is_valid_evidence_anchor("代码里可能有问题"));
        assert!(!is_valid_evidence_anchor("src/foo.rs"));
        assert!(!is_valid_evidence_anchor("commit abc"));
        assert!(!is_valid_evidence_anchor("最近改了很多文件"));
        assert!(!is_valid_evidence_anchor("src/foo.rs:0"));
    }

    #[test]
    fn parse_scout_candidates_handles_fenced_and_raw_json() {
        let fenced = "分析如下：\n```json\n[\n  {\"title\": \"补测试\", \"evidence\": [\"src/a.rs:10\"], \"suggestion\": \"加单测\", \"cost\": \"低\", \"briefReady\": true}\n]\n```\n";
        let items = parse_scout_candidates(fenced).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "补测试");
        assert!(items[0].brief_ready);
        assert!(has_valid_evidence_anchor(&items[0].evidence));

        let empty = parse_scout_candidates("[]").unwrap();
        assert!(empty.is_empty());
        assert!(parse_scout_candidates("无").unwrap().is_empty());
    }

    #[test]
    fn parse_scout_candidates_handles_prose_sections_ordered_lists_and_missing_anchors() {
        let prose = "\
### 补齐重试退避测试
- 证据：`src/retry.rs:42-58` 缺少超时退避断言（参见 collect#2:L10）
- 建议：在 `tests/retry.rs` 增加退避上限单测。
- 成本：低（15 分钟）
- 可直接写成任务说明：是（边界明确）

### 没有锚点的模糊想法
- 感觉这里可以再抽象一层，让代码更好看。
- 成本：中
";
        let items = parse_scout_candidates(prose).unwrap();
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].title, "补齐重试退避测试");
        assert!(has_valid_evidence_anchor(&items[0].evidence), "{:?}", items[0]);
        assert!(items[0].brief_ready);
        assert_eq!(items[0].brief_note, "边界明确");
        assert!(items[0].suggestion.contains("退避上限单测"));
        // Anchorless block does not fail parsing; caller skips it with a factual note.
        assert_eq!(items[1].title, "没有锚点的模糊想法");
        assert!(!has_valid_evidence_anchor(&items[1].evidence));

        let ordered = "\
1. **清理废弃配置**：`crates/delegate/src/config.rs#L120` 仍保留旧字段，建议删除。
   - 成本：低
2. 核对提交 commit 61951c8 的兼容逻辑，第 12-30 行存在重复分支。
";
        let ord_items = parse_scout_candidates(ordered).unwrap();
        assert_eq!(ord_items.len(), 2, "{ord_items:?}");
        assert_eq!(ord_items[0].title, "清理废弃配置");
        assert!(has_valid_evidence_anchor(&ord_items[0].evidence));
        assert!(has_valid_evidence_anchor(&ord_items[1].evidence));
        assert!(is_valid_evidence_anchor("第 12-30 行存在重复分支"));

        // Board markdown renders `lastNote` pointing to `last-scout-output.md`.
        let md = render_board_md(&json!({
            "repo": "/tmp/demo",
            "repoName": "demo",
            "updatedAt": "2026-11-27T00:00:00Z",
            "lastFingerprint": "sha256:1234567890abcdef",
            "candidates": [],
            "lastNote": "本轮有 1 条候选未提取到具体证据锚点，已跳过入板；完整侦察原文见 `/tmp/last-scout-output.md`",
        }));
        assert!(md.contains("last-scout-output.md"), "{md}");

        // `## ` wrapper heading around an ordered list must split each list item, not collapse to 1.
        let h2_wrapped_ordered = "\
## 本次新增候选

1. **清理废弃配置**：`crates/delegate/src/config.rs:120` 仍保留旧字段，建议删除。
2. **补齐超时测试**：`src/retry.rs:42` 缺少超时覆盖，建议补齐单测。
";
        let wrapped = parse_scout_candidates(h2_wrapped_ordered).unwrap();
        assert_eq!(wrapped.len(), 2, "{wrapped:?}");
        assert_eq!(wrapped[0].title, "清理废弃配置");
        assert_eq!(wrapped[1].title, "补齐超时测试");
        assert!(has_valid_evidence_anchor(&wrapped[0].evidence));
        assert!(has_valid_evidence_anchor(&wrapped[1].evidence));

        // Same-prefix `### 分类：具体标题` sections must keep full titles so they do not collide on dedup.
        let same_prefix = "\
### 回归风险：未处理空数组
- 证据：`src/parser.rs:15` 未判空
- 建议：补空数组守卫

### 回归风险：缺少超时控制
- 证据：`src/client.rs:88` 未设超时
- 建议：加默认超时
";
        let prefixed = parse_scout_candidates(same_prefix).unwrap();
        assert_eq!(prefixed.len(), 2, "{prefixed:?}");
        assert_eq!(prefixed[0].title, "回归风险：未处理空数组");
        assert_eq!(prefixed[1].title, "回归风险：缺少超时控制");
        assert_ne!(normalize_key(&prefixed[0].title), normalize_key(&prefixed[1].title));
    }

    #[test]
    fn validate_radar_config_checks_fields() {
        assert!(validate_radar_config(Some(&json!({
            "collect": ["git log -n 5 --oneline", {"name": "gaps", "command": "echo ok"}],
            "tier": "scout",
            "timeout": "2m"
        })))
        .is_ok());
        assert!(validate_radar_config(Some(&json!({
            "collect": ["echo ok"],
            "tier": "write"
        })))
        .is_err());
        assert!(validate_radar_config(Some(&json!({
            "collect": [""]
        })))
        .is_err());
    }
}
