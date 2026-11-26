//! Two-step scout-first orchestration helpers for write tasks:
//! verbatim scout prompt & writer appendix templates, deterministic L0/L1 splitting,
//! L0 anchor validation + git blob SHA hashing, section-3 mismatch extraction,
//! lightweight brief detection, and `preEditSteps` counting.
use crate::brief_check::CheckReport;
use crate::common::s;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

pub const SCOUT_PROMPT_TEMPLATE: &str = "\
# 只读探路：为下面这项改动画一张地图

接下来另一位同事会在新会话里实现这项改动。它看不到你读过的内容，只拿到你的回答。你的任务是读代码，把它要动的位置找准；不写实现方案，不贴代码（一行函数签名可以）。

## 要实现的改动
{{brief}}

回答只有两节，标题逐字如下：

## L0
不超过 1000 字，只写四类事实：
1. 落点：每条一行，格式 `路径` :: `符号` (L起-L止) — 为什么要动它；不超过 12 条。
2. 跨文件关系：谁调用谁、数据从哪来；写手要跑的相关测试命令。
3. 与说明不符之处：说明里假设的东西在代码里实际是什么样；没有就写「无」。
4. 未核实项：你没查清的点，以及该去哪里查。

## L1
不超过 6000 字：完整的相关调用表与数据结构位置（路径 :: 符号 (行段)），可参照的同类实现，测试夹具的现成写法，以及「看过但无关」的文件和一句理由。";

pub const WRITER_APPENDIX_HEADER: &str = "\n\n## 已核实的落点（探路地图 L0）\n以下是只读探路对现状的核实结果（与上文冲突时以上文为准）：\n\n";

pub const LIGHTWEIGHT_MAX_CHARS: usize = 2000;

pub fn build_scout_prompt(raw_brief: &str) -> String {
    SCOUT_PROMPT_TEMPLATE.replace("{{brief}}", raw_brief)
}

pub fn build_writer_brief(
    raw_brief: &str,
    l0: &str,
    map_path: &Path,
    controller_reply: Option<&str>,
) -> String {
    let mut out = format!(
        "{}{}{}\n\n完整地图（L1：调用表、同类实现、测试夹具写法、看过但无关的文件）在 `{}`，需要时按节查。",
        raw_brief.trim_end(),
        WRITER_APPENDIX_HEADER,
        l0.trim(),
        map_path.display()
    );
    if let Some(reply) = controller_reply.map(str::trim).filter(|r| !r.is_empty()) {
        out.push_str(&format!("\n\n## 主控对探路发现的答复\n\n{reply}"));
    }
    out
}

/// Lightweight write brief exemption:
/// 1-3 repo-relative paths named in the brief, all existing, zero symbol hints, and <= 2000 chars.
pub fn is_lightweight_brief(prompt: &str, report: &CheckReport) -> bool {
    let n = report.paths.len();
    (1..=3).contains(&n)
        && report.missing_paths.is_empty()
        && !report.has_symbol_hints
        && prompt.chars().count() <= LIGHTWEIGHT_MAX_CHARS
}

/// Count tool invocations (`read`/`bash`/`tool`) before the first `edit`/`write` event.
pub fn compute_pre_edit_steps(evs: &[Value]) -> u64 {
    let mut steps = 0u64;
    for ev in evs {
        match s(ev, "e") {
            "edit" | "write" => break,
            "read" | "bash" | "tool" => steps += 1,
            _ => {}
        }
    }
    steps
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMap {
    pub l0: String,
    pub l1: Option<String>,
    pub mismatch: Option<String>,
}

fn fence_marker(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = trimmed.chars().take_while(|&c| c == first).count();
    (count >= 3).then_some((first, count))
}

fn is_level2_heading(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("##")?;
    if rest.starts_with('#') {
        return None;
    }
    let title = rest.trim();
    (!title.is_empty()).then_some(title)
}

fn matches_section_tag(title: &str, tag: &str) -> bool {
    let Some(after) = title.strip_prefix(tag) else {
        return false;
    };
    match after.chars().next() {
        None => true,
        Some(c) => c.is_whitespace() || matches!(c, ':' | '：' | '（' | '(' | '-' | '—' | '·'),
    }
}

/// Deterministically split scout output into `## L0` and optional `## L1`.
/// Returns `None` if `## L0` is absent or empty.
pub fn parse_sections(text: &str) -> Option<ParsedMap> {
    let mut fence: Option<(char, usize)> = None;
    let mut current: Option<&str> = None;
    let mut l0_lines: Vec<&str> = Vec::new();
    let mut l1_lines: Vec<&str> = Vec::new();
    let mut saw_l0 = false;
    let mut saw_l1 = false;

    for line in text.lines() {
        if let Some((fc, flen)) = fence {
            if let Some((c, len)) = fence_marker(line) {
                if c == fc && len >= flen && line.trim_start()[len..].trim().is_empty() {
                    fence = None;
                }
            }
        } else if let Some(m) = fence_marker(line) {
            fence = Some(m);
        } else if let Some(title) = is_level2_heading(line) {
            if matches_section_tag(title, "L0") {
                saw_l0 = true;
                current = Some("L0");
                continue;
            } else if matches_section_tag(title, "L1") {
                saw_l1 = true;
                current = Some("L1");
                continue;
            } else if current.is_some() {
                current = None;
            }
        }

        match current {
            Some("L0") => l0_lines.push(line),
            Some("L1") => l1_lines.push(line),
            _ => {}
        }
    }

    if !saw_l0 {
        return None;
    }
    let l0 = l0_lines.join("\n").trim().to_string();
    if l0.is_empty() {
        return None;
    }
    let l1 = if saw_l1 {
        let body = l1_lines.join("\n").trim().to_string();
        (!body.is_empty()).then_some(body)
    } else {
        None
    };
    let mismatch = extract_mismatch_section(&l0);
    Some(ParsedMap { l0, l1, mismatch })
}

fn strip_md_heading_prefix(line: &str) -> &str {
    let mut s = line.trim();
    s = s.trim_start_matches('#').trim();
    if let Some(inner) = s.strip_prefix("**").and_then(|x| x.strip_suffix("**")) {
        s = inner.trim();
    }
    s
}

fn is_l0_numbered_header(line: &str, num: u8, keywords: &[&str]) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let is_heading = trimmed.starts_with("###") || trimmed.starts_with("####");
    let cleaned = strip_md_heading_prefix(trimmed);
    let prefix_dot = format!("{num}.");
    let prefix_cn = format!("{num}、");
    let rest = cleaned
        .strip_prefix(&prefix_dot)
        .or_else(|| cleaned.strip_prefix(&prefix_cn))
        .map(str::trim);
    let candidate = match rest {
        Some(r) => r,
        None if is_heading => cleaned,
        None => return None,
    };
    for kw in keywords {
        if let Some(after_kw) = candidate.strip_prefix(kw) {
            let after_kw = after_kw
                .trim_start()
                .trim_start_matches("**")
                .trim_start()
                .trim_start_matches([':', '：', '-', '—'])
                .trim();
            return Some(after_kw.to_string());
        }
    }
    if is_heading && keywords.iter().any(|kw| candidate.contains(kw)) {
        return Some(String::new());
    }
    None
}

fn is_mismatch_none(body: &str) -> bool {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return true;
    }
    // Check single-line or bulleted "无"
    let meaningful: Vec<&str> = trimmed
        .lines()
        .map(|l| {
            l.trim()
                .trim_start_matches(['-', '*', '•', '·'])
                .trim()
                .trim_matches(['「', '」', '`', '"', '\'', '。', '.', '，', ','])
                .trim()
        })
        .filter(|l| !l.is_empty())
        .collect();
    if meaningful.is_empty() {
        return true;
    }
    if meaningful.len() == 1 {
        let only = meaningful[0];
        let lower = only.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "无" | "暂无" | "没有" | "无不符" | "未发现" | "未发现不符" | "none" | "n/a" | "nil"
        ) {
            return true;
        }
        if let Some(rest) = only.strip_prefix('无') {
            if rest
                .chars()
                .next()
                .is_some_and(|c| matches!(c, '。' | '，' | ',' | '（' | '(' | '；' | ';' | ' '))
            {
                return true;
            }
        }
    }
    false
}

/// Extract section 3 ("与说明不符之处") raw text from L0; returns `None` if empty or "无".
pub fn extract_mismatch_section(l0: &str) -> Option<String> {
    let mut in_sec3 = false;
    let mut collected: Vec<String> = Vec::new();
    let mut fence: Option<(char, usize)> = None;

    for line in l0.lines() {
        if let Some((fc, flen)) = fence {
            if let Some((c, len)) = fence_marker(line) {
                if c == fc && len >= flen && line.trim_start()[len..].trim().is_empty() {
                    fence = None;
                }
            }
            if in_sec3 {
                collected.push(line.to_string());
            }
            continue;
        } else if let Some(m) = fence_marker(line) {
            fence = Some(m);
            if in_sec3 {
                collected.push(line.to_string());
            }
            continue;
        }

        if !in_sec3 {
            if let Some(inline) = is_l0_numbered_header(line, 3, &["与说明不符之处", "与说明不符"]) {
                in_sec3 = true;
                if !inline.is_empty() {
                    collected.push(inline);
                }
            }
        } else {
            if is_l0_numbered_header(line, 4, &["未核实项", "未核实"]).is_some()
                || line.trim_start().starts_with("### ")
                || line.trim_start().starts_with("#### ")
                || line.trim() == "---"
            {
                break;
            }
            collected.push(line.to_string());
        }
    }

    if !in_sec3 {
        return None;
    }
    let body = collected.join("\n").trim().to_string();
    if is_mismatch_none(&body) {
        None
    } else {
        Some(body)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAnchor {
    pub path: String,
    pub symbol: String,
    pub start_line: usize,
    pub end_line: usize,
}

fn parse_line_range_paren(s: &str) -> Option<(usize, usize)> {
    // Find `(Lstart-Lend)` / `(Lstart–Lend)` / `(Lstart-end)` / `(Lstart)` (ASCII or full-width parens)
    let mut rest = s;
    while let Some(open_idx) = rest.find(['(', '（']) {
        let after_open = &rest[open_idx + rest[open_idx..].chars().next()?.len_utf8()..];
        let Some(close_idx) = after_open.find([')', '）']) else {
            break;
        };
        let inside = after_open[..close_idx].trim();
        if let Some(range) = parse_line_range_inner(inside) {
            return Some(range);
        }
        rest = &after_open[close_idx + after_open[close_idx..].chars().next()?.len_utf8()..];
    }
    None
}

fn parse_line_range_inner(inside: &str) -> Option<(usize, usize)> {
    let trimmed = inside.trim();
    if !trimmed.starts_with(['L', 'l']) {
        return None;
    }
    let cleaned = trimmed.replace(['–', '—', '−'], "-");
    if let Some((a, b)) = cleaned.split_once('-') {
        let a = a.trim().trim_start_matches(['L', 'l']);
        let b = b.trim().trim_start_matches(['L', 'l']);
        let start = a.parse::<usize>().ok()?;
        let end = b.parse::<usize>().ok()?;
        if start > 0 && end >= start {
            return Some((start, end));
        }
    } else {
        let a = cleaned.trim().trim_start_matches(['L', 'l']);
        let start = a.parse::<usize>().ok()?;
        if start > 0 {
            return Some((start, start));
        }
    }
    None
}

fn strip_list_prefix(line: &str) -> &str {
    let t = line.trim();
    if let Some(rest) = t
        .strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))
        .or_else(|| t.strip_prefix("• "))
    {
        return rest.trim();
    }
    let digits = t.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 {
        let after = &t[digits..];
        if let Some(rest) = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
            return rest.trim();
        }
    }
    t
}

fn extract_leading_token(s: &str) -> (&str, &str) {
    let t = s.trim_start();
    if let Some(after) = t.strip_prefix('`') {
        if let Some(end) = after.find('`') {
            return (after[..end].trim(), after[end + 1..].trim_start());
        }
    }
    // Unquoted token up to whitespace or `::` or `(`
    let end = t
        .find(|c: char| c.is_whitespace() || c == '(' || c == '（')
        .unwrap_or(t.len());
    (t[..end].trim(), t[end..].trim_start())
}

fn looks_like_repo_path(p: &str) -> bool {
    if p.is_empty() || p.contains(char::is_whitespace) || p.contains("://") {
        return false;
    }
    p.contains('/') || p.contains('.')
}

/// Parse one L0 anchor line: `` `路径` :: `符号` (L起-L止) `` (with optional list marker & description).
pub fn parse_anchor_line(line: &str) -> Option<ParsedAnchor> {
    let body = strip_list_prefix(line);
    // Split off trailing description after ` — ` or ` - ` if present, only after `)`
    let (path_raw, after_path) = extract_leading_token(body);
    let path_clean = path_raw
        .trim_start_matches("./")
        .trim_end_matches(':')
        .to_string();
    if !looks_like_repo_path(&path_clean) {
        return None;
    }
    if let Some(after_colons) = after_path.strip_prefix("::") {
        let (sym_raw, after_sym) = extract_leading_token(after_colons);
        let (start_line, end_line) = parse_line_range_paren(after_sym)?;
        let symbol = sym_raw.trim().to_string();
        if symbol.is_empty() {
            return None;
        }
        return Some(ParsedAnchor {
            path: path_clean,
            symbol,
            start_line,
            end_line,
        });
    }
    // Also support path-only anchor like `` `skills/delegate/SKILL.md` (L65) ``
    if after_path.starts_with(['(', '（']) {
        let (start_line, end_line) = parse_line_range_paren(after_path)?;
        return Some(ParsedAnchor {
            path: path_clean,
            symbol: String::new(),
            start_line,
            end_line,
        });
    }
    None
}

/// Extract anchors from L0 section 1 ("落点"), or across L0 if section headings are omitted.
pub fn extract_l0_anchors(l0: &str) -> Vec<ParsedAnchor> {
    let mut sec1_lines: Vec<&str> = Vec::new();
    let mut in_sec1 = false;
    let mut saw_sec1 = false;
    let mut fence: Option<(char, usize)> = None;

    for line in l0.lines() {
        if let Some((fc, flen)) = fence {
            if let Some((c, len)) = fence_marker(line) {
                if c == fc && len >= flen && line.trim_start()[len..].trim().is_empty() {
                    fence = None;
                }
            }
            continue;
        } else if let Some(m) = fence_marker(line) {
            fence = Some(m);
            continue;
        }

        if !saw_sec1 {
            if let Some(inline) = is_l0_numbered_header(line, 1, &["落点"]) {
                saw_sec1 = true;
                in_sec1 = true;
                if !inline.is_empty() {
                    // Wait: inline could be an anchor if single-line
                    if let Some(a) = parse_anchor_line(&inline) {
                        sec1_lines.push(line);
                        let _ = a;
                    }
                }
                continue;
            }
        } else if in_sec1 {
            if is_l0_numbered_header(line, 2, &["跨文件关系", "跨文件"]).is_some()
                || is_l0_numbered_header(line, 3, &["与说明不符之处", "与说明不符"]).is_some()
                || line.trim_start().starts_with("### ")
                || line.trim_start().starts_with("#### ")
            {
                in_sec1 = false;
                continue;
            }
            sec1_lines.push(line);
        }
    }

    let source_lines: Vec<&str> = if saw_sec1 {
        sec1_lines
    } else {
        l0.lines().collect()
    };

    let mut out = Vec::new();
    for line in source_lines {
        if let Some(anchor) = parse_anchor_line(line) {
            out.push(anchor);
        }
    }
    out
}

/// Compute git blob SHA-1 (`sha1("blob <len>\0<bytes>")`) deterministically without spawning git.
pub fn git_blob_sha1(bytes: &[u8]) -> String {
    let mut hasher = sha1_smol::Sha1::new();
    let header = format!("blob {}\0", bytes.len());
    hasher.update(header.as_bytes());
    hasher.update(bytes);
    hasher.digest().to_string()
}

fn symbol_matches_window(symbol: &str, window_text: &str) -> bool {
    if symbol.is_empty() {
        return true;
    }
    let clean = symbol
        .trim()
        .trim_start_matches("fn ")
        .trim_start_matches("pub fn ")
        .trim_start_matches("def ")
        .trim_start_matches("class ")
        .trim_start_matches("struct ")
        .trim_start_matches("enum ")
        .trim_start_matches("impl ")
        .trim();
    let base = clean.split('(').next().unwrap_or(clean).trim();
    if base.is_empty() {
        return true;
    }
    if window_text.contains(base) {
        return true;
    }
    // For qualified names like `ChainIndex::load` or `DelegateTests.setUp`, check the leaf identifier.
    if let Some(leaf) = base.rsplit("::").next().filter(|x| !x.is_empty() && *x != base) {
        if window_text.contains(leaf) {
            return true;
        }
    }
    if let Some(leaf) = base.rsplit('.').next().filter(|x| !x.is_empty() && *x != base) {
        if window_text.contains(leaf) {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnchorValidation {
    pub total: usize,
    pub bad: usize,
    pub blobs: BTreeMap<String, String>,
    pub bad_details: Vec<String>,
}

impl AnchorValidation {
    /// Drop map when strictly more than 20% of anchors are bad (`bad / total > 0.20`).
    pub fn should_drop(&self) -> bool {
        self.total > 0 && self.bad * 5 > self.total
    }
}

pub fn validate_l0_anchors(l0: &str, repo_root: &Path) -> AnchorValidation {
    let anchors = extract_l0_anchors(l0);
    let mut validation = AnchorValidation {
        total: anchors.len(),
        ..Default::default()
    };
    let mut file_cache: BTreeMap<String, Option<String>> = BTreeMap::new();

    for anchor in anchors {
        if anchor.path.starts_with('/') || anchor.path.split('/').any(|c| c == "..") {
            validation.bad += 1;
            validation
                .bad_details
                .push(format!("非法路径 {}", anchor.path));
            continue;
        }
        let text_opt = file_cache
            .entry(anchor.path.clone())
            .or_insert_with(|| {
                let full = repo_root.join(&anchor.path);
                if !full.is_file() {
                    return None;
                }
                let bytes = fs::read(&full).ok()?;
                validation
                    .blobs
                    .insert(anchor.path.clone(), git_blob_sha1(&bytes));
                Some(String::from_utf8_lossy(&bytes).into_owned())
            })
            .clone();
        let Some(text) = text_opt else {
            validation.bad += 1;
            validation
                .bad_details
                .push(format!("路径不存在 {}", anchor.path));
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        let total_lines = lines.len();
        if total_lines == 0 || anchor.start_line == 0 || anchor.start_line > total_lines {
            validation.bad += 1;
            validation.bad_details.push(format!(
                "行号越界 {} (L{}-L{}, 共 {} 行)",
                anchor.path, anchor.start_line, anchor.end_line, total_lines
            ));
            continue;
        }
        let win_start = anchor.start_line.saturating_sub(5).max(1);
        let win_end = anchor.end_line.saturating_add(5).min(total_lines);
        let window_text = lines[win_start - 1..win_end].join("\n");
        if !symbol_matches_window(&anchor.symbol, &window_text) {
            validation.bad += 1;
            validation.bad_details.push(format!(
                "符号未命中 {} :: {} (L{}-L{})",
                anchor.path, anchor.symbol, anchor.start_line, anchor.end_line
            ));
        }
    }
    validation
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapOutcome {
    Accepted {
        l0: String,
        l1: Option<String>,
        mismatch: Option<String>,
        validation: AnchorValidation,
    },
    Dropped {
        code: &'static str,
        reason: String,
        bad_anchors: usize,
        total_anchors: usize,
        blobs: BTreeMap<String, String>,
    },
}

pub fn evaluate_map(raw_map: &str, repo_root: &Path) -> MapOutcome {
    let Some(parsed) = parse_sections(raw_map) else {
        return MapOutcome::Dropped {
            code: "missing_l0",
            reason: "缺 L0".to_string(),
            bad_anchors: 0,
            total_anchors: 0,
            blobs: BTreeMap::new(),
        };
    };
    let validation = validate_l0_anchors(&parsed.l0, repo_root);
    if validation.should_drop() {
        let pct = (validation.bad * 100) / validation.total.max(1);
        return MapOutcome::Dropped {
            code: "bad_anchors",
            reason: format!(
                "坏锚点比例过高（{}/{}，{}%）",
                validation.bad, validation.total, pct
            ),
            bad_anchors: validation.bad,
            total_anchors: validation.total,
            blobs: validation.blobs,
        };
    }
    MapOutcome::Accepted {
        l0: parsed.l0,
        l1: parsed.l1,
        mismatch: parsed.mismatch,
        validation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_l0_l1_and_detects_missing_l0() {
        let doc = "## L0\n### 1. 落点\n1. `src/a.rs` :: `foo` (L1-L3) — edit\n### 3. 与说明不符之处\n无\n\n## L1\ndetails\n";
        let p = parse_sections(doc).unwrap();
        assert!(p.l0.contains("`src/a.rs`"));
        assert_eq!(p.l1.as_deref(), Some("details"));
        assert!(p.mismatch.is_none());

        assert!(parse_sections("just prose without headings").is_none());
        assert!(parse_sections("## L1\nonly l1\n").is_none());
    }

    #[test]
    fn ignores_headings_inside_fenced_blocks() {
        let doc = "## L0\n```md\n## L1\nfake\n```\n### 3. 与说明不符之处\n无\n## L1\nreal l1\n";
        let p = parse_sections(doc).unwrap();
        assert!(p.l0.contains("fake"));
        assert_eq!(p.l1.as_deref(), Some("real l1"));
    }

    #[test]
    fn mismatch_section_detects_real_diff_and_ignores_none_variants() {
        for none_case in [
            "### 3. 与说明不符之处\n无\n### 4. 未核实项\n无",
            "### 3. 与说明不符之处\n- 无。\n### 4. 未核实项\n无",
            "3. 与说明不符之处：「无」\n4. 未核实项：无",
            "### 3. 与说明不符之处\n无（全部一致）\n### 4. 未核实项\n无",
        ] {
            assert_eq!(extract_mismatch_section(none_case), None, "case: {none_case}");
        }
        let real = "### 3. 与说明不符之处\n1. `work start` 不存在，实际是 `dispatch_work_auto`\n### 4. 未核实项\n无";
        let m = extract_mismatch_section(real).unwrap();
        assert!(m.contains("dispatch_work_auto"));
        assert!(!m.contains("未核实项"));

        let tricky = "3. 与说明不符之处：无此函数，实际叫 bar\n4. 未核实项：无";
        assert!(extract_mismatch_section(tricky).is_some());
    }

    #[test]
    fn validates_anchors_window_and_bad_ratio() {
        let dir = std::env::temp_dir().join(format!("delegate-scout-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        let file_content = "line1\nline2\nfn alpha() {}\nline4\nline5\nline6\nline7\nline8\nline9\nfn beta() {}\n";
        fs::write(dir.join("src/lib.rs"), file_content).unwrap();

        // 1/5 bad = 20% -> accepted
        let l0_ok = "\
### 1. 落点
1. `src/lib.rs` :: `alpha` (L3-L4) — exact
2. `src/lib.rs` :: `alpha` (L7-L8) — within start-5 window (7-5=2 <= 3)
3. `src/lib.rs` :: `beta` (L6-L6) — within end+5 window (6+5=11 >= 10)
4. `src/lib.rs` :: `Mod::beta` (L10-L10) — qualified leaf match
5. `src/missing.rs` :: `nope` (L1-L2) — missing file
";
        let v_ok = validate_l0_anchors(l0_ok, &dir);
        assert_eq!(v_ok.total, 5);
        assert_eq!(v_ok.bad, 1);
        assert!(!v_ok.should_drop());
        assert_eq!(
            v_ok.blobs.get("src/lib.rs").map(String::as_str),
            Some(git_blob_sha1(file_content.as_bytes()).as_str())
        );

        // 2/4 bad = 50% -> dropped
        let l0_bad = "\
### 1. 落点
1. `src/lib.rs` :: `alpha` (L3-L4) — ok
2. `src/lib.rs` :: `beta` (L10-L10) — ok
3. `src/lib.rs` :: `ghost` (L1-L5) — missing symbol
4. `src/missing.rs` :: `foo` (L1-L2) — missing file
";
        let v_bad = validate_l0_anchors(l0_bad, &dir);
        assert_eq!(v_bad.total, 4);
        assert_eq!(v_bad.bad, 2);
        assert!(v_bad.should_drop());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn counts_pre_edit_steps_before_first_edit_or_write() {
        let evs = vec![
            json!({"e":"session","id":"s1"}),
            json!({"e":"read","path":"a.rs"}),
            json!({"e":"tool","tool":"grep","arg":"foo"}),
            json!({"e":"bash","cmd":"ls"}),
            json!({"e":"bash_done","ok":true}),
            json!({"e":"turn","model":"m"}),
            json!({"e":"edit","path":"a.rs"}),
            json!({"e":"read","path":"b.rs"}),
            json!({"e":"write","path":"c.rs"}),
        ];
        assert_eq!(compute_pre_edit_steps(&evs), 3);
    }

    #[test]
    fn lightweight_brief_checks_all_conditions() {
        let ok = CheckReport {
            paths: vec!["a.rs".into(), "b.rs".into()],
            missing_paths: vec![],
            has_symbol_hints: false,
            diagnostics: vec![],
        };
        assert!(is_lightweight_brief("short brief", &ok));
        assert!(!is_lightweight_brief(&"x".repeat(2001), &ok));

        let mut no_paths = ok.clone();
        no_paths.paths.clear();
        assert!(!is_lightweight_brief("short", &no_paths));

        let mut four_paths = ok.clone();
        four_paths.paths = vec!["a.rs".into(), "b.rs".into(), "c.rs".into(), "d.rs".into()];
        assert!(!is_lightweight_brief("short", &four_paths));

        let mut missing = ok.clone();
        missing.missing_paths = vec!["a.rs".into()];
        assert!(!is_lightweight_brief("short", &missing));

        let mut sym_hint = ok.clone();
        sym_hint.has_symbol_hints = true;
        assert!(!is_lightweight_brief("short", &sym_hint));
    }
}
