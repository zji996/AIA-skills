//! Merge Review Card (合入审查卡): progressive disclosure L0 / L1 / L2.
//!
//! - L0: one-line summary in `status`, `wait`, and delivery notifications (`summary.json["card"]`).
//! - L1: `delegate card <run>` — <= 1 screen deterministic index (what changed + risk items +
//!   tests & acceptance layers + directory-folded remaining changes + next commands).
//! - L2: `delegate diff <run> --risky` — only risk-matched hunks with surrounding context.
//! - L3: `delegate diff <run>` — full diff (unchanged).

use crate::accept::human_accept_label;
use crate::archive;
use crate::changes;
use crate::common::*;
use crate::config;
use crate::history;
use crate::output;
use crate::refs;
use crate::review::glob_match;
use crate::runs;
use crate::worktree;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_LARGE_DELETION_LINES: usize = 40;
const DEFAULT_RISKY_CONTEXT_LINES: usize = 5;
const MAX_L1_RISK_ITEMS: usize = 10;
const MAX_L1_OTHER_DIRS: usize = 8;

const BUILTIN_RULE_IDS: &[&str] = &[
    "model_visible",
    "migration_schema",
    "weakened_test",
    "dependency",
    "protected_path",
    "config_constant",
    "concurrency_unsafe",
    "secret_internal",
    "large_deletion",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Critical = 0,
    High = 1,
    Medium = 2,
    Low = 3,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
        }
    }

    pub fn zh_label(self) -> &'static str {
        match self {
            Severity::Critical => "严重",
            Severity::High => "高",
            Severity::Medium => "中",
            Severity::Low => "低",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "critical" | "crit" | "p0" | "严重" => Some(Severity::Critical),
            "high" | "p1" | "高" => Some(Severity::High),
            "medium" | "med" | "p2" | "中" => Some(Severity::Medium),
            "low" | "p3" | "低" => Some(Severity::Low),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CustomRiskRule {
    pub id: String,
    pub label: String,
    pub severity: Severity,
    pub paths: Vec<String>,
    pub content: Vec<String>,
    pub match_removed: bool,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct RiskConfig {
    pub disabled: BTreeSet<String>,
    pub large_deletion_lines: usize,
    pub extra: Vec<CustomRiskRule>,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            disabled: BTreeSet::new(),
            large_deletion_lines: DEFAULT_LARGE_DELETION_LINES,
            extra: Vec::new(),
        }
    }
}

pub fn validate_risk_rules(value: Option<&Value>) -> Res<()> {
    let Some(v) = value else { return Ok(()) };
    if v.is_null() {
        return Ok(());
    }
    let validate_extra_list = |arr: &[Value], prefix: &str| -> Res<()> {
        for (idx, item) in arr.iter().enumerate() {
            let obj = item
                .as_object()
                .ok_or_else(|| format!("{prefix}[{idx}] must be an object"))?;
            for key in obj.keys() {
                if ![
                    "id",
                    "label",
                    "severity",
                    "paths",
                    "content",
                    "matchRemoved",
                    "reason",
                ]
                .contains(&key.as_str())
                {
                    return Err(format!("{prefix}[{idx}] has unknown field {key:?}"));
                }
            }
            if !obj
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty() && !s.contains(['\0', '\n', '\r']))
            {
                return Err(format!("{prefix}[{idx}].id must be a non-empty string"));
            }
            if let Some(sev) = obj.get("severity") {
                let Some(s) = sev.as_str().and_then(Severity::parse) else {
                    return Err(format!(
                        "{prefix}[{idx}].severity must be critical, high, medium, or low"
                    ));
                };
                let _ = s;
            }
            for arr_key in ["paths", "content"] {
                if let Some(av) = obj.get(arr_key) {
                    if !av.as_array().is_some_and(|a| {
                        a.iter()
                            .all(|x| x.as_str().is_some_and(|s| !s.is_empty() && !s.contains('\0')))
                    }) {
                        return Err(format!(
                            "{prefix}[{idx}].{arr_key} must be an array of non-empty strings"
                        ));
                    }
                }
            }
            let has_paths = obj
                .get("paths")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty());
            let has_content = obj
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty());
            if !has_paths && !has_content {
                return Err(format!(
                    "{prefix}[{idx}] must specify at least one of paths or content"
                ));
            }
            if let Some(mr) = obj.get("matchRemoved") {
                if !mr.is_boolean() {
                    return Err(format!("{prefix}[{idx}].matchRemoved must be a boolean"));
                }
            }
            for str_key in ["label", "reason"] {
                if let Some(sv) = obj.get(str_key) {
                    if !sv
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty() && !s.contains('\0'))
                    {
                        return Err(format!(
                            "{prefix}[{idx}].{str_key} must be a non-empty string"
                        ));
                    }
                }
            }
        }
        Ok(())
    };

    if let Some(arr) = v.as_array() {
        return validate_extra_list(arr, "riskRules");
    }
    let obj = v
        .as_object()
        .ok_or("riskRules must be an object or array of custom risk rules")?;
    for key in obj.keys() {
        if !["disable", "largeDeletionLines", "extra"].contains(&key.as_str()) {
            return Err(format!(
                "riskRules has unknown field {key:?} (allowed: disable, largeDeletionLines, extra)"
            ));
        }
    }
    if let Some(dis) = obj.get("disable") {
        let arr = dis
            .as_array()
            .ok_or("riskRules.disable must be an array of built-in rule IDs")?;
        for item in arr {
            let id = item
                .as_str()
                .ok_or("riskRules.disable entries must be strings")?;
            if !BUILTIN_RULE_IDS.contains(&id) {
                return Err(format!(
                    "riskRules.disable has unknown rule ID {id:?} (known: {})",
                    BUILTIN_RULE_IDS.join(", ")
                ));
            }
        }
    }
    if let Some(ldl) = obj.get("largeDeletionLines") {
        if !ldl.as_u64().is_some_and(|n| n >= 5) {
            return Err("riskRules.largeDeletionLines must be an integer >= 5".into());
        }
    }
    if let Some(extra) = obj.get("extra") {
        let arr = extra
            .as_array()
            .ok_or("riskRules.extra must be an array of custom risk rules")?;
        validate_extra_list(arr, "riskRules.extra")?;
    }
    Ok(())
}

fn parse_risk_config(val: &Value) -> RiskConfig {
    let mut cfg = RiskConfig::default();
    let parse_extra = |arr: &[Value], out: &mut Vec<CustomRiskRule>| {
        for item in arr {
            let id = s(item, "id").trim().to_string();
            if id.is_empty() {
                continue;
            }
            let label = {
                let l = s(item, "label").trim();
                if l.is_empty() {
                    id.clone()
                } else {
                    l.to_string()
                }
            };
            let severity = Severity::parse(s(item, "severity")).unwrap_or(Severity::Medium);
            let paths = strings(&item["paths"]);
            let content = strings(&item["content"]);
            if paths.is_empty() && content.is_empty() {
                continue;
            }
            let match_removed = b(item, "matchRemoved");
            let reason = {
                let r = s(item, "reason").trim();
                if r.is_empty() {
                    label.clone()
                } else {
                    r.to_string()
                }
            };
            out.push(CustomRiskRule {
                id,
                label,
                severity,
                paths,
                content,
                match_removed,
                reason,
            });
        }
    };
    if let Some(arr) = val.as_array() {
        parse_extra(arr, &mut cfg.extra);
    } else if val.is_object() {
        for id in strings(&val["disable"]) {
            cfg.disabled.insert(id);
        }
        if let Some(n) = val["largeDeletionLines"].as_u64().filter(|&n| n >= 5) {
            cfg.large_deletion_lines = n as usize;
        }
        if let Some(arr) = val["extra"].as_array() {
            parse_extra(arr, &mut cfg.extra);
        }
    }
    cfg
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Add,
    Del,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct DiffHunk {
    pub header: String,
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<DiffLine>,
    pub added: usize,
    pub deleted: usize,
}

impl DiffHunk {
    pub fn first_changed_line(&self) -> usize {
        for l in &self.lines {
            if l.kind != DiffLineKind::Context {
                if let Some(n) = l.new_line.filter(|&x| x > 0) {
                    return n;
                }
                if let Some(o) = l.old_line.filter(|&x| x > 0) {
                    return o;
                }
            }
        }
        self.new_start.max(self.old_start).max(1)
    }

    pub fn covers_line(&self, line: usize) -> bool {
        let start = self.new_start.max(1);
        let end = start.saturating_add(self.new_count.max(1));
        if line >= start.saturating_sub(5) && line <= end.saturating_add(5) {
            return true;
        }
        let ostart = self.old_start.max(1);
        let oend = ostart.saturating_add(self.old_count.max(1));
        line >= ostart.saturating_sub(5) && line <= oend.saturating_add(5)
    }
}

#[derive(Debug, Clone)]
pub struct DiffFile {
    pub path: String,
    pub header_lines: Vec<String>,
    pub hunks: Vec<DiffHunk>,
    pub is_new: bool,
    pub is_deleted: bool,
    pub is_binary: bool,
    pub added: usize,
    pub deleted: usize,
}

fn parse_hunk_range(token: &str) -> (usize, usize) {
    let trimmed = token.trim_start_matches(['-', '+']);
    if let Some((start, count)) = trimmed.split_once(',') {
        (
            start.parse().unwrap_or(0),
            count.parse().unwrap_or(0),
        )
    } else {
        (trimmed.parse().unwrap_or(0), 1)
    }
}

fn strip_diff_prefix(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('"');
    trimmed
        .strip_prefix("a/")
        .or_else(|| trimmed.strip_prefix("b/"))
        .unwrap_or(trimmed)
        .to_string()
}

pub fn parse_unified_diff(diff_text: &str) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut cur_file: Option<DiffFile> = None;
    let mut cur_hunk: Option<DiffHunk> = None;
    let mut old_cur: usize = 0;
    let mut new_cur: usize = 0;

    let flush_hunk = |file: &mut Option<DiffFile>, hunk: &mut Option<DiffHunk>| {
        if let (Some(f), Some(h)) = (file.as_mut(), hunk.take()) {
            f.added += h.added;
            f.deleted += h.deleted;
            f.hunks.push(h);
        }
    };

    for raw_line in diff_text.lines() {
        if raw_line.starts_with("diff --git ") {
            flush_hunk(&mut cur_file, &mut cur_hunk);
            if let Some(f) = cur_file.take() {
                files.push(f);
            }
            let path = raw_line
                .strip_prefix("diff --git ")
                .and_then(|rest| rest.split_once(" b/").map(|(_, b)| b.to_string()))
                .unwrap_or_default();
            cur_file = Some(DiffFile {
                path,
                header_lines: vec![raw_line.to_string()],
                hunks: Vec::new(),
                is_new: false,
                is_deleted: false,
                is_binary: false,
                added: 0,
                deleted: 0,
            });
            continue;
        }
        let Some(file) = cur_file.as_mut() else {
            continue;
        };
        if raw_line.starts_with("@@ ") && cur_hunk.is_none()
            || (raw_line.starts_with("@@ ") && raw_line.contains(" @@"))
        {
            flush_hunk(&mut cur_file, &mut cur_hunk);
            let file = cur_file.as_mut().unwrap();
            let header_body = raw_line
                .strip_prefix("@@ ")
                .and_then(|rest| rest.split_once(" @@"))
                .map(|(h, _)| h)
                .unwrap_or("");
            let mut parts = header_body.split_whitespace();
            let (old_start, old_count) = parse_hunk_range(parts.next().unwrap_or("-0,0"));
            let (new_start, new_count) = parse_hunk_range(parts.next().unwrap_or("+0,0"));
            old_cur = old_start;
            new_cur = new_start;
            cur_hunk = Some(DiffHunk {
                header: raw_line.to_string(),
                old_start,
                old_count,
                new_start,
                new_count,
                lines: Vec::new(),
                added: 0,
                deleted: 0,
            });
            let _ = file;
            continue;
        }
        if let Some(hunk) = cur_hunk.as_mut() {
            if let Some(text) = raw_line.strip_prefix('+') {
                hunk.lines.push(DiffLine {
                    kind: DiffLineKind::Add,
                    old_line: None,
                    new_line: Some(new_cur),
                    text: text.to_string(),
                });
                hunk.added += 1;
                new_cur += 1;
            } else if let Some(text) = raw_line.strip_prefix('-') {
                hunk.lines.push(DiffLine {
                    kind: DiffLineKind::Del,
                    old_line: Some(old_cur),
                    new_line: Some(new_cur.max(1)),
                    text: text.to_string(),
                });
                hunk.deleted += 1;
                old_cur += 1;
            } else if let Some(text) = raw_line.strip_prefix(' ') {
                hunk.lines.push(DiffLine {
                    kind: DiffLineKind::Context,
                    old_line: Some(old_cur),
                    new_line: Some(new_cur),
                    text: text.to_string(),
                });
                old_cur += 1;
                new_cur += 1;
            } else if raw_line.starts_with("\\ No newline") {
                // Keep quiet metadata lines inside hunk without shifting line counters.
            } else {
                hunk.lines.push(DiffLine {
                    kind: DiffLineKind::Context,
                    old_line: Some(old_cur),
                    new_line: Some(new_cur),
                    text: raw_line.to_string(),
                });
            }
        } else {
            file.header_lines.push(raw_line.to_string());
            if raw_line.starts_with("new file mode") {
                file.is_new = true;
            } else if raw_line.starts_with("deleted file mode") {
                file.is_deleted = true;
            } else if raw_line.starts_with("Binary files ") || raw_line == "GIT binary patch" {
                file.is_binary = true;
            } else if let Some(rest) = raw_line.strip_prefix("+++ ") {
                if rest.trim() != "/dev/null" {
                    file.path = strip_diff_prefix(rest);
                }
            } else if let Some(rest) = raw_line.strip_prefix("--- ") {
                if file.path.is_empty() && rest.trim() != "/dev/null" {
                    file.path = strip_diff_prefix(rest);
                }
            }
        }
    }

    flush_hunk(&mut cur_file, &mut cur_hunk);
    if let Some(f) = cur_file {
        files.push(f);
    }
    files
}

#[derive(Debug, Clone)]
pub struct RiskItem {
    pub severity: Severity,
    pub file: String,
    pub line: usize,
    pub rule_id: String,
    pub category: String,
    pub source: String, // "规则" | "审查员" | "规则+审查员"
    pub reason: String,
    pub hunk_index: Option<usize>,
    pub file_level: bool,
    pub verdict_file: Option<String>,
}

impl RiskItem {
    pub fn to_json(&self) -> Value {
        let mut obj = json!({
            "severity": self.severity.as_str(),
            "severityLabel": self.severity.zh_label(),
            "file": self.file,
            "line": self.line,
            "location": format!("{}:{}", self.file, self.line),
            "rule": self.rule_id,
            "category": self.category,
            "source": self.source,
            "reason": self.reason,
        });
        if let Some(vf) = &self.verdict_file {
            obj["verdictFile"] = json!(vf);
        }
        obj
    }
}

#[derive(Debug, Clone)]
pub struct ReviewerFocusItem {
    pub severity: Severity,
    pub file: String,
    pub line: usize,
    pub reason: String,
}

fn is_markdown_or_doc(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md")
        || lower.ends_with(".txt")
        || lower.ends_with(".rst")
        || lower.starts_with("docs/")
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let file_name = lower.rsplit('/').next().unwrap_or(&lower);
    lower.starts_with("tests/")
        || lower.contains("/tests/")
        || lower.contains("/__tests__/")
        || file_name.starts_with("test_")
        || file_name.contains("_test.")
        || file_name.contains(".test.")
        || file_name.contains(".spec.")
        || file_name.ends_with("_tests.rs")
        || file_name.ends_with("_tests.py")
}

fn is_lockfile(path: &str) -> bool {
    const LOCKS: &[&str] = &[
        "Cargo.lock",
        "**/Cargo.lock",
        "package-lock.json",
        "**/package-lock.json",
        "pnpm-lock.yaml",
        "**/pnpm-lock.yaml",
        "yarn.lock",
        "**/yarn.lock",
        "bun.lock",
        "**/bun.lock",
        "bun.lockb",
        "**/bun.lockb",
        "poetry.lock",
        "**/poetry.lock",
        "uv.lock",
        "**/uv.lock",
        "go.sum",
        "**/go.sum",
        "Gemfile.lock",
        "**/Gemfile.lock",
        "Pipfile.lock",
        "**/Pipfile.lock",
        "flake.lock",
        "**/flake.lock",
    ];
    LOCKS.iter().any(|pat| glob_match(pat, path))
}

fn is_manifest_file(path: &str) -> bool {
    const MANIFESTS: &[&str] = &[
        "Cargo.toml",
        "**/Cargo.toml",
        "package.json",
        "**/package.json",
        "pyproject.toml",
        "**/pyproject.toml",
        "go.mod",
        "**/go.mod",
        "requirements*.txt",
        "**/requirements*.txt",
    ];
    MANIFESTS.iter().any(|pat| glob_match(pat, path))
}

fn is_migration_or_schema_path(path: &str) -> bool {
    const PATS: &[&str] = &[
        "**/migrations/**",
        "migrations/**",
        "**/migration/**",
        "migration/**",
        "**/*migration*",
        "**/*.sql",
        "*.sql",
        "**/schema.sql",
        "**/schema.prisma",
        "**/schema.rs",
        "**/schema.ts",
        "**/schema.graphql",
        "**/*.graphql",
        "**/*.proto",
        "**/ddl/**",
        "**/alembic/**",
        "**/drizzle/**",
    ];
    PATS.iter().any(|pat| glob_match(pat, path))
}

fn has_sql_ddl(line: &str) -> bool {
    let up = line.to_ascii_uppercase();
    [
        "ALTER TABLE ",
        "DROP TABLE ",
        "DROP COLUMN ",
        "CREATE TABLE ",
        "CREATE INDEX ",
        "CREATE UNIQUE INDEX ",
        "TRUNCATE TABLE ",
    ]
    .iter()
    .any(|kw| up.contains(kw))
}

fn is_comment_line(trimmed: &str) -> bool {
    trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with("<!--")
        || (trimmed.starts_with("# ") || trimmed == "#")
}

fn is_skip_or_ignore_marker(line: &str) -> Option<&'static str> {
    let trimmed = line.trim();
    if is_comment_line(trimmed) {
        return None;
    }
    for (needle, label) in [
        ("#[ignore", "#[ignore]"),
        ("test.skip(", "test.skip"),
        ("it.skip(", "it.skip"),
        ("describe.skip(", "describe.skip"),
        ("xit(", "xit"),
        ("xtest(", "xtest"),
        ("xdescribe(", "xdescribe"),
        ("@pytest.mark.skip", "@pytest.mark.skip"),
        ("@unittest.skip", "@unittest.skip"),
        ("t.Skip(", "t.Skip"),
        ("t.SkipNow(", "t.SkipNow"),
    ] {
        if trimmed.contains(needle) {
            return Some(label);
        }
    }
    None
}

fn is_assert_line(line: &str) -> bool {
    let trimmed = line.trim();
    if is_comment_line(trimmed) {
        return false;
    }
    [
        "assert!(",
        "assert_eq!(",
        "assert_ne!(",
        "assert_matches!(",
        "debug_assert!(",
        "debug_assert_eq!(",
        "self.assert",
        "assert ",
        "expect(",
        ".toEqual(",
        ".toBe(",
        ".toThrow(",
        ".toContain(",
        ".rejects.",
        "assert.",
    ]
    .iter()
    .any(|kw| trimmed.contains(kw))
}

fn extract_test_def_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if is_comment_line(trimmed) {
        return None;
    }
    if trimmed == "#[test]"
        || trimmed.starts_with("#[tokio::test")
        || trimmed.starts_with("#[sqlx::test")
        || trimmed.starts_with("#[async_std::test")
    {
        return Some(trimmed.to_string());
    }
    for prefix in ["def test_", "async def test_", "func Test"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some(format!("{prefix}{name}"));
            }
        }
    }
    for prefix in ["it(", "test(", "it.each(", "test.each("] {
        if trimmed.starts_with(prefix) {
            return Some(trimmed.chars().take(60).collect());
        }
    }
    None
}

fn looks_like_manifest_dep_add(path: &str, line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('#')
        || trimmed.starts_with("//")
        || trimmed.starts_with('[')
    {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    if lower.ends_with("cargo.toml") {
        // `foo = "1.2"` or `foo = { version = ... }` or `foo.workspace = true`
        if let Some((k, v)) = trimmed.split_once('=') {
            let key = k.trim();
            let val = v.trim();
            if [
                "name",
                "version",
                "edition",
                "rust-version",
                "license",
                "description",
                "repository",
                "authors",
                "strip",
                "lto",
                "codegen-units",
                "panic",
                "opt-level",
                "debug",
                "resolver",
                "members",
                "exclude",
                "path",
            ]
            .contains(&key)
            {
                return false;
            }
            return val.starts_with('"')
                || val.starts_with('{')
                || val == "true" && key.ends_with(".workspace");
        }
    } else if lower.ends_with("package.json") {
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().trim_matches('"');
            let val = v.trim().trim_end_matches(',').trim_matches('"');
            if [
                "name",
                "version",
                "description",
                "main",
                "module",
                "type",
                "license",
                "private",
                "packageManager",
            ]
            .contains(&key)
            {
                return false;
            }
            return val.starts_with('^')
                || val.starts_with('~')
                || val.starts_with(">=")
                || val.starts_with("workspace:")
                || val.chars().next().is_some_and(|c| c.is_ascii_digit());
        }
    } else if lower.ends_with("go.mod") {
        return trimmed.contains(" v") && !trimmed.starts_with("go ") && !trimmed.starts_with("module ");
    } else if lower.contains("requirements") || lower.ends_with("pyproject.toml") {
        return trimmed.contains("==") || trimmed.contains(">=") || trimmed.contains("~=");
    }
    false
}

fn contains_numeric_literal(s: &str) -> bool {
    let bytes = s.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i].is_ascii_digit() {
            let prev_ok = i == 0 || !bytes[i - 1].is_ascii_alphabetic() && bytes[i - 1] != b'_';
            if prev_ok {
                return true;
            }
        }
    }
    false
}

fn config_constant_hit(path: &str, line: &str) -> Option<String> {
    if is_markdown_or_doc(path) || is_lockfile(path) {
        return None;
    }
    let trimmed = line.trim();
    if trimmed.is_empty() || is_comment_line(trimmed) {
        return None;
    }
    if !contains_numeric_literal(trimmed) {
        return None;
    }
    let lower = path.to_ascii_lowercase();
    let is_config_file = lower.ends_with(".json")
        || lower.ends_with(".toml")
        || lower.ends_with(".yaml")
        || lower.ends_with(".yml")
        || lower.ends_with(".env");

    const LIMIT_HINTS: &[&str] = &[
        "max",
        "min",
        "limit",
        "timeout",
        "budget",
        "cap",
        "pool",
        "threshold",
        "interval",
        "retries",
        "retry",
        "default",
        "size",
        "port",
        "concurrency",
        "workers",
        "quota",
        "ttl",
        "window",
        "rate",
        "delay",
        "backoff",
        "bytes",
        "chars",
        "lines",
        "seconds",
        "millis",
        "ms",
    ];

    if is_config_file && !is_test_path(path) {
        let lower_line = trimmed.to_ascii_lowercase();
        if let Some(kw) = LIMIT_HINTS.iter().find(|k| lower_line.contains(**k)) {
            return Some(format!("配置数字项（{kw}）变化"));
        }
        return None;
    }

    // Code files: named constants or explicit timeout/default numeric literals.
    if (trimmed.contains("const ") || trimmed.contains("static "))
        && trimmed.contains('=')
        && !is_test_path(path)
    {
        return Some("数字常量定义变化（可能为硬上限/默认值）".into());
    }
    if !is_test_path(path) {
        if let Some((lhs, _rhs)) = trimmed.split_once('=') {
            let ident = lhs
                .split_whitespace()
                .last()
                .unwrap_or("")
                .trim_end_matches(':');
            let is_upper_const = ident.len() >= 4
                && ident
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                && ident.contains('_');
            if is_upper_const {
                return Some(format!("常量 {ident} 数值变化"));
            }
        }
        if trimmed.contains("Duration::from_secs(")
            || trimmed.contains("Duration::from_millis(")
            || trimmed.contains("unwrap_or(")
        {
            let lower_line = trimmed.to_ascii_lowercase();
            if LIMIT_HINTS.iter().any(|k| lower_line.contains(*k)) {
                return Some("默认值/超时数字变化".into());
            }
        }
    }
    None
}

fn concurrency_or_unsafe_hit(path: &str, line: &str) -> Option<(Severity, &'static str)> {
    if is_markdown_or_doc(path) || is_lockfile(path) {
        return None;
    }
    let trimmed = line.trim();
    if trimmed.is_empty() || is_comment_line(trimmed) {
        return None;
    }
    for (kw, label) in [
        ("unsafe {", "unsafe"),
        ("unsafe fn ", "unsafe fn"),
        ("unsafe impl ", "unsafe impl"),
        ("unsafe trait ", "unsafe trait"),
    ] {
        if trimmed.contains(kw) {
            return Some((Severity::High, label));
        }
    }
    for (kw, label) in [
        ("Mutex", "Mutex"),
        ("RwLock", "RwLock"),
        ("flock(", "flock"),
        ("Condvar", "Condvar"),
        ("Semaphore", "Semaphore"),
        ("Barrier", "Barrier"),
        ("acquire_state", "acquire_state"),
        ("acquire_shell", "acquire_shell"),
        ("threading.Lock", "threading.Lock"),
        ("asyncio.Lock", "asyncio.Lock"),
        ("sync.Mutex", "sync.Mutex"),
        ("sync.RWMutex", "sync.RWMutex"),
    ] {
        if trimmed.contains(kw) {
            return Some((Severity::High, label));
        }
    }
    for (kw, label) in [
        ("Atomic", "Atomic"),
        ("Ordering::", "Ordering"),
        ("thread::spawn", "thread::spawn"),
        ("thread::scope", "thread::scope"),
        ("tokio::spawn", "tokio::spawn"),
        ("rayon::", "rayon"),
        ("Promise.all", "Promise.all"),
    ] {
        if trimmed.contains(kw) {
            return Some((Severity::Medium, label));
        }
    }
    None
}

fn detect_private_ipv4(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            // Skip version-like prefixes like `v10.0.1.2`.
            if i > 0 && (bytes[i - 1].is_ascii_alphabetic() || bytes[i - 1] == b'_') {
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                continue;
            }
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let token = &line[start..i];
            let parts: Vec<&str> = token.split('.').collect();
            if parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.len() <= 3) {
                if let (Ok(a), Ok(b), Ok(c), Ok(d)) = (
                    parts[0].parse::<u8>(),
                    parts[1].parse::<u8>(),
                    parts[2].parse::<u8>(),
                    parts[3].parse::<u8>(),
                ) {
                    let _ = (c, d);
                    let is_private = a == 10
                        || (a == 192 && b == 168)
                        || (a == 172 && (16..=31).contains(&b))
                        || (a == 100 && (64..=127).contains(&b));
                    if is_private {
                        return Some(token.to_string());
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

fn detect_internal_hostname(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    for suffix in [".internal", ".lan", ".corp", ".ts.net", ".home.arpa", ".local"] {
        let mut search_from = 0;
        while let Some(pos) = lower[search_from..].find(suffix) {
            let abs = search_from + pos;
            let after = abs + suffix.len();
            search_from = after;
            // Must be preceded by a valid hostname label char (not `/` or `.` or whitespace).
            if abs == 0 {
                continue;
            }
            let prev = lower.as_bytes()[abs - 1];
            if !(prev.is_ascii_alphanumeric() || prev == b'-') {
                continue;
            }
            // Boundary after suffix: end, `:`, `/`, whitespace, quote, `)`, `>`, `,`
            // For `.local`, exclude filesystem paths like `foo.local/run/` unless preceded by `://` or `@`!
            let next_byte = lower.as_bytes().get(after).copied();
            if let Some(nb) = next_byte {
                if nb.is_ascii_alphanumeric() || nb == b'-' || nb == b'_' || nb == b'.' {
                    continue;
                }
            }
            // Extract host token backwards.
            let mut hstart = abs;
            while hstart > 0 {
                let ch = lower.as_bytes()[hstart - 1];
                if ch.is_ascii_alphanumeric() || ch == b'-' || ch == b'.' {
                    hstart -= 1;
                } else {
                    break;
                }
            }
            let host = &line[hstart..after];
            if host.len() <= suffix.len() {
                continue;
            }
            if suffix == ".local" {
                // Skip obvious filesystem paths or Rust/JS code like `x.local` unless URL/SSH/port context.
                let before_host = &lower[..hstart];
                let url_or_ssh = before_host.ends_with("://")
                    || before_host.ends_with('@')
                    || next_byte == Some(b':');
                if !url_or_ssh {
                    continue;
                }
            }
            return Some(host.to_string());
        }
    }
    None
}

fn detect_secret_or_internal(line: &str) -> Option<String> {
    for kw in [
        "BEGIN PRIVATE KEY",
        "BEGIN RSA PRIVATE KEY",
        "BEGIN EC PRIVATE KEY",
        "BEGIN OPENSSH PRIVATE KEY",
        "BEGIN DSA PRIVATE KEY",
        "BEGIN PGP PRIVATE KEY",
    ] {
        if line.contains(kw) {
            return Some("疑似私钥内容".into());
        }
    }
    for (prefix, kind) in [
        ("ghp_", "GitHub Token"),
        ("gho_", "GitHub Token"),
        ("github_pat_", "GitHub PAT"),
        ("sk-proj-", "OpenAI API Key"),
        ("sk-ant-", "Anthropic API Key"),
        ("xoxb-", "Slack Token"),
        ("xoxp-", "Slack Token"),
        ("AKIA", "AWS Access Key"),
        ("AIza", "Google API Key"),
    ] {
        if let Some(pos) = line.find(prefix) {
            let rest = &line[pos + prefix.len()..];
            let token_len = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .count();
            let min_len = if prefix == "AKIA" { 16 } else if prefix == "AIza" { 30 } else { 12 };
            if token_len >= min_len {
                return Some(format!("疑似 {kind}"));
            }
        }
    }
    let lower = line.to_ascii_lowercase();
    for key in [
        "api_key",
        "apikey",
        "secret_key",
        "access_token",
        "auth_token",
        "private_key",
        "client_secret",
        "password",
        "passwd",
    ] {
        if let Some(pos) = lower.find(key) {
            let after = &line[pos + key.len()..];
            let trimmed = after.trim_start();
            if let Some(rest) = trimmed
                .strip_prefix('=')
                .or_else(|| trimmed.strip_prefix(':'))
            {
                let val_part = rest.trim_start();
                if let Some(q) = val_part.chars().next().filter(|&c| c == '"' || c == '\'') {
                    if let Some(end) = val_part[1..].find(q) {
                        let secret_val = &val_part[1..1 + end];
                        let sv_low = secret_val.to_ascii_lowercase();
                        let is_placeholder = secret_val.len() < 12
                            || sv_low.contains("example")
                            || sv_low.contains("placeholder")
                            || sv_low.contains("dummy")
                            || sv_low.contains("changeme")
                            || sv_low.contains("your_")
                            || sv_low.contains("xxxx")
                            || sv_low.contains("****")
                            || sv_low.starts_with('<')
                            || sv_low.starts_with('$')
                            || sv_low.contains("test");
                        if !is_placeholder {
                            return Some(format!("疑似硬编码凭据（{key}）"));
                        }
                    }
                }
            }
        }
    }
    if let Some(ip) = detect_private_ipv4(line) {
        return Some(format!("疑似内网 IP 地址（{ip}）"));
    }
    if let Some(host) = detect_internal_hostname(line) {
        return Some(format!("疑似内网主机名（{host}）"));
    }
    None
}

fn match_protect_rule(path: &str, rules: &[String]) -> Option<String> {
    for rule in rules {
        if rule.ends_with('/') {
            if path.starts_with(rule.as_str()) {
                return Some(rule.clone());
            }
        } else if path == rule || glob_match(rule, path) {
            return Some(rule.clone());
        }
    }
    None
}

/// Run all enabled built-in + custom deterministic risk rules over parsed diff files.
pub fn detect_rule_risks(
    files: &[DiffFile],
    controller_review_paths: &[String],
    protect_rules: &[String],
    allowed_rules: &[String],
    risk_cfg: &RiskConfig,
) -> Vec<RiskItem> {
    let mut items: Vec<RiskItem> = Vec::new();
    let enabled = |id: &str| !risk_cfg.disabled.contains(id);

    for file in files {
        let first_line = file
            .hunks
            .first()
            .map(DiffHunk::first_changed_line)
            .unwrap_or(1);

        // 1. Model-visible text (`controllerReviewPaths`)
        if enabled("model_visible") && !controller_review_paths.is_empty() {
            if let Some(pat) = controller_review_paths
                .iter()
                .find(|pat| glob_match(pat, &file.path))
            {
                items.push(RiskItem {
                    severity: Severity::High,
                    file: file.path.clone(),
                    line: first_line,
                    rule_id: "model_visible".into(),
                    category: "模型可见文字".into(),
                    source: "规则".into(),
                    reason: format!("命中 controllerReviewPaths（{pat}）"),
                    hunk_index: if file.hunks.is_empty() { None } else { Some(0) },
                    file_level: true,
                    verdict_file: None,
                });
            }
        }

        // 5. Protected paths
        if enabled("protected_path") && !protect_rules.is_empty() {
            if let Some(rule) = match_protect_rule(&file.path, protect_rules) {
                let allowed = worktree::matches_rule(&file.path, allowed_rules);
                items.push(RiskItem {
                    severity: Severity::High,
                    file: file.path.clone(),
                    line: first_line,
                    rule_id: "protected_path".into(),
                    category: "受保护路径".into(),
                    source: "规则".into(),
                    reason: if allowed {
                        format!("命中受保护路径（{rule}，本次 --allow 放开）")
                    } else {
                        format!("命中受保护路径（{rule}）")
                    },
                    hunk_index: if file.hunks.is_empty() { None } else { Some(0) },
                    file_level: true,
                    verdict_file: None,
                });
            }
        }

        // 2. Database migration / schema (file-level path match)
        let mut file_is_migration = false;
        if enabled("migration_schema") && is_migration_or_schema_path(&file.path) {
            file_is_migration = true;
            items.push(RiskItem {
                severity: Severity::High,
                file: file.path.clone(),
                line: first_line,
                rule_id: "migration_schema".into(),
                category: "迁移/Schema".into(),
                source: "规则".into(),
                reason: "涉及数据库迁移或 Schema 定义变更".into(),
                hunk_index: if file.hunks.is_empty() { None } else { Some(0) },
                file_level: true,
                verdict_file: None,
            });
        }

        // 4. Dependencies & lockfile
        if enabled("dependency") {
            if is_lockfile(&file.path) {
                items.push(RiskItem {
                    severity: Severity::Medium,
                    file: file.path.clone(),
                    line: first_line,
                    rule_id: "dependency".into(),
                    category: "依赖/Lockfile".into(),
                    source: "规则".into(),
                    reason: "Lockfile 变更".into(),
                    hunk_index: if file.hunks.is_empty() { None } else { Some(0) },
                    file_level: false,
                    verdict_file: None,
                });
            } else if is_manifest_file(&file.path) {
                'dep_scan: for (hi, hunk) in file.hunks.iter().enumerate() {
                    for l in &hunk.lines {
                        if l.kind == DiffLineKind::Add
                            && looks_like_manifest_dep_add(&file.path, &l.text)
                        {
                            items.push(RiskItem {
                                severity: Severity::Medium,
                                file: file.path.clone(),
                                line: l.new_line.unwrap_or(first_line).max(1),
                                rule_id: "dependency".into(),
                                category: "依赖/Lockfile".into(),
                                source: "规则".into(),
                                reason: format!("新增或修改依赖声明（{}）", clip(l.text.trim(), 50)),
                                hunk_index: Some(hi),
                                file_level: false,
                                verdict_file: None,
                            });
                            break 'dep_scan;
                        }
                    }
                }
            }
        }

        // 3. Deleted whole test file
        if enabled("weakened_test") && file.is_deleted && is_test_path(&file.path) {
            items.push(RiskItem {
                severity: Severity::High,
                file: file.path.clone(),
                line: 1,
                rule_id: "weakened_test".into(),
                category: "测试减弱".into(),
                source: "规则".into(),
                reason: format!("删除了测试文件（-{} 行）", file.deleted),
                hunk_index: if file.hunks.is_empty() { None } else { Some(0) },
                file_level: true,
                verdict_file: None,
            });
        }

        // 9. Deleted whole large file
        if enabled("large_deletion")
            && file.is_deleted
            && file.deleted >= risk_cfg.large_deletion_lines
            && !is_lockfile(&file.path)
        {
            items.push(RiskItem {
                severity: Severity::Medium,
                file: file.path.clone(),
                line: 1,
                rule_id: "large_deletion".into(),
                category: "大段删除".into(),
                source: "规则".into(),
                reason: format!("整文件删除（-{} 行）", file.deleted),
                hunk_index: if file.hunks.is_empty() { None } else { Some(0) },
                file_level: false,
                verdict_file: None,
            });
        }

        // Hunk-level checks
        let mut in_cfg_test = is_test_path(&file.path);
        for (hi, hunk) in file.hunks.iter().enumerate() {
            if hunk.header.contains("mod tests") || hunk.header.contains("cfg(test)") {
                in_cfg_test = true;
            }
            for l in &hunk.lines {
                if l.text.contains("#[cfg(test)]") || l.text.contains("mod tests") {
                    in_cfg_test = true;
                }
            }

            // 2b. SQL DDL in code
            if enabled("migration_schema") && !file_is_migration && !is_markdown_or_doc(&file.path) && !is_test_path(&file.path) {
                if let Some(l) = hunk
                    .lines
                    .iter()
                    .find(|l| l.kind != DiffLineKind::Context && has_sql_ddl(&l.text))
                {
                    items.push(RiskItem {
                        severity: Severity::High,
                        file: file.path.clone(),
                        line: l.new_line.or(l.old_line).unwrap_or(1).max(1),
                        rule_id: "migration_schema".into(),
                        category: "迁移/Schema".into(),
                        source: "规则".into(),
                        reason: format!("包含 SQL DDL 语句（{}）", clip(l.text.trim(), 50)),
                        hunk_index: Some(hi),
                        file_level: false,
                        verdict_file: None,
                    });
                }
            }

            // 3b. Weakened tests: skip/ignore markers, net deleted asserts, deleted test defs
            if enabled("weakened_test") && !file.is_deleted && !is_markdown_or_doc(&file.path) {
                if let Some((line_no, marker)) = hunk.lines.iter().find_map(|l| {
                    (l.kind == DiffLineKind::Add)
                        .then(|| is_skip_or_ignore_marker(&l.text).map(|m| (l.new_line.unwrap_or(1).max(1), m)))
                        .flatten()
                }) {
                    items.push(RiskItem {
                        severity: Severity::High,
                        file: file.path.clone(),
                        line: line_no,
                        rule_id: "weakened_test".into(),
                        category: "测试减弱".into(),
                        source: "规则".into(),
                        reason: format!("新增测试跳过/忽略标记（{marker}）"),
                        hunk_index: Some(hi),
                        file_level: false,
                        verdict_file: None,
                    });
                }

                let mut del_asserts = 0usize;
                let mut add_asserts = 0usize;
                let mut first_del_assert_line = None;
                let mut del_defs = 0usize;
                let mut add_defs = 0usize;
                let mut first_del_def_line = None;
                for l in &hunk.lines {
                    match l.kind {
                        DiffLineKind::Del => {
                            if is_assert_line(&l.text) {
                                del_asserts += 1;
                                first_del_assert_line.get_or_insert(l.new_line.or(l.old_line).unwrap_or(1).max(1));
                            }
                            if extract_test_def_name(&l.text).is_some() {
                                del_defs += 1;
                                first_del_def_line.get_or_insert(l.new_line.or(l.old_line).unwrap_or(1).max(1));
                            }
                        }
                        DiffLineKind::Add => {
                            if is_assert_line(&l.text) {
                                add_asserts += 1;
                            }
                            if extract_test_def_name(&l.text).is_some() {
                                add_defs += 1;
                            }
                        }
                        DiffLineKind::Context => {}
                    }
                }
                if del_defs > add_defs {
                    items.push(RiskItem {
                        severity: Severity::High,
                        file: file.path.clone(),
                        line: first_del_def_line.unwrap_or_else(|| hunk.first_changed_line()),
                        rule_id: "weakened_test".into(),
                        category: "测试减弱".into(),
                        source: "规则".into(),
                        reason: format!("删除了测试定义（-{del_defs}/+{add_defs}）"),
                        hunk_index: Some(hi),
                        file_level: false,
                        verdict_file: None,
                    });
                } else if (in_cfg_test || del_asserts >= 2) && del_asserts > add_asserts {
                    items.push(RiskItem {
                        severity: Severity::High,
                        file: file.path.clone(),
                        line: first_del_assert_line.unwrap_or_else(|| hunk.first_changed_line()),
                        rule_id: "weakened_test".into(),
                        category: "测试减弱".into(),
                        source: "规则".into(),
                        reason: format!("删减了测试断言（-{del_asserts}/+{add_asserts}）"),
                        hunk_index: Some(hi),
                        file_level: false,
                        verdict_file: None,
                    });
                }
            }

            // 6. Config defaults / numeric constants
            if enabled("config_constant") {
                if let Some((line_no, reason)) = hunk.lines.iter().find_map(|l| {
                    (l.kind == DiffLineKind::Add)
                        .then(|| {
                            config_constant_hit(&file.path, &l.text)
                                .map(|r| (l.new_line.unwrap_or(1).max(1), r))
                        })
                        .flatten()
                }) {
                    items.push(RiskItem {
                        severity: Severity::Medium,
                        file: file.path.clone(),
                        line: line_no,
                        rule_id: "config_constant".into(),
                        category: "配置/常量".into(),
                        source: "规则".into(),
                        reason,
                        hunk_index: Some(hi),
                        file_level: false,
                        verdict_file: None,
                    });
                }
            }

            // 7. Locks, concurrency, unsafe
            if enabled("concurrency_unsafe") && !is_test_path(&file.path) {
                let mut best_sev: Option<Severity> = None;
                let mut first_line_hit = None;
                let mut labels: Vec<&'static str> = Vec::new();
                for l in &hunk.lines {
                    if l.kind == DiffLineKind::Add {
                        if let Some((sev, lbl)) = concurrency_or_unsafe_hit(&file.path, &l.text) {
                            best_sev = Some(best_sev.map_or(sev, |cur| cur.min(sev)));
                            first_line_hit.get_or_insert(l.new_line.unwrap_or(1).max(1));
                            if !labels.contains(&lbl) {
                                labels.push(lbl);
                            }
                        }
                    }
                }
                if let Some(sev) = best_sev {
                    items.push(RiskItem {
                        severity: sev,
                        file: file.path.clone(),
                        line: first_line_hit.unwrap_or_else(|| hunk.first_changed_line()),
                        rule_id: "concurrency_unsafe".into(),
                        category: "锁/并发/unsafe".into(),
                        source: "规则".into(),
                        reason: format!("涉及锁/并发或 unsafe（{}）", labels.join("、")),
                        hunk_index: Some(hi),
                        file_level: false,
                        verdict_file: None,
                    });
                }
            }

            // 8. Suspected internal host/IP or secret
            if enabled("secret_internal") {
                if let Some((line_no, reason)) = hunk.lines.iter().find_map(|l| {
                    (l.kind == DiffLineKind::Add)
                        .then(|| {
                            detect_secret_or_internal(&l.text)
                                .map(|r| (l.new_line.unwrap_or(1).max(1), r))
                        })
                        .flatten()
                }) {
                    items.push(RiskItem {
                        severity: Severity::Critical,
                        file: file.path.clone(),
                        line: line_no,
                        rule_id: "secret_internal".into(),
                        category: "敏感信息/内网".into(),
                        source: "规则".into(),
                        reason,
                        hunk_index: Some(hi),
                        file_level: false,
                        verdict_file: None,
                    });
                }
            }

            // 9b. Large deletion hunk
            if enabled("large_deletion") && !file.is_deleted && !is_lockfile(&file.path) {
                let threshold = risk_cfg.large_deletion_lines;
                let net_del = hunk.deleted.saturating_sub(hunk.added);
                if hunk.deleted >= threshold && net_del >= (threshold * 5 / 8).max(1) {
                    items.push(RiskItem {
                        severity: Severity::Medium,
                        file: file.path.clone(),
                        line: hunk.first_changed_line(),
                        rule_id: "large_deletion".into(),
                        category: "大段删除".into(),
                        source: "规则".into(),
                        reason: format!("单处大段删除（-{}/+{} 行）", hunk.deleted, hunk.added),
                        hunk_index: Some(hi),
                        file_level: false,
                        verdict_file: None,
                    });
                }
            }

            // Custom `.delegate.json` risk rules
            for rule in &risk_cfg.extra {
                let path_ok = rule.paths.is_empty()
                    || rule.paths.iter().any(|pat| glob_match(pat, &file.path));
                if !path_ok {
                    continue;
                }
                if rule.content.is_empty() {
                    if hi == 0 {
                        items.push(RiskItem {
                            severity: rule.severity,
                            file: file.path.clone(),
                            line: hunk.first_changed_line(),
                            rule_id: rule.id.clone(),
                            category: rule.label.clone(),
                            source: "规则".into(),
                            reason: rule.reason.clone(),
                            hunk_index: Some(0),
                            file_level: true,
                            verdict_file: None,
                        });
                    }
                } else if let Some(l) = hunk.lines.iter().find(|l| {
                    let kind_ok = l.kind == DiffLineKind::Add
                        || (rule.match_removed && l.kind == DiffLineKind::Del);
                    kind_ok && rule.content.iter().any(|kw| l.text.contains(kw))
                }) {
                    items.push(RiskItem {
                        severity: rule.severity,
                        file: file.path.clone(),
                        line: l.new_line.or(l.old_line).unwrap_or(1).max(1),
                        rule_id: rule.id.clone(),
                        category: rule.label.clone(),
                        source: "规则".into(),
                        reason: rule.reason.clone(),
                        hunk_index: Some(hi),
                        file_level: false,
                        verdict_file: None,
                    });
                }
            }
        }
    }

    items
}

/// Parse the reviewer's one-line conclusion from the verdict markdown (after line 1 verdict word).
pub fn extract_reviewer_conclusion(verdict_text: &str) -> Option<String> {
    for raw in verdict_text.lines().skip(1) {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "---" || trimmed == "***" {
            continue;
        }
        if is_focus_heading(trimmed) {
            break;
        }
        if trimmed.starts_with('#') {
            continue;
        }
        let cleaned = trimmed
            .trim_start_matches(['-', '*', '>', ' '])
            .replace("**", "")
            .trim()
            .to_string();
        if !cleaned.is_empty() {
            return Some(clip(&cleaned, 140));
        }
    }
    None
}

fn is_focus_heading(line: &str) -> bool {
    let trimmed = line.trim().trim_start_matches('#').trim();
    let lower = trimmed.to_ascii_lowercase();
    trimmed.contains("主控重点看")
        || trimmed.contains("主控该重点看")
        || trimmed.contains("重点核查")
        || lower.contains("focus for controller")
        || lower.contains("controller focus")
        || lower.contains("key review points")
}

/// Extract `path:line` and trailing reason from a single bullet line in the focus section.
fn parse_focus_line(raw: &str) -> Option<ReviewerFocusItem> {
    let mut text = raw.trim();
    if text.is_empty() || text == "无" || text.eq_ignore_ascii_case("none") {
        return None;
    }
    // Strip bullet (`- `, `* `) or numbered list (`1. `, `2) `).
    if let Some(rest) = text.strip_prefix("- ").or_else(|| text.strip_prefix("* ")) {
        text = rest.trim();
    } else if let Some(idx) = text.find(['.', ')']) {
        if idx > 0 && idx <= 3 && text[..idx].bytes().all(|b| b.is_ascii_digit()) {
            text = text[idx + 1..].trim();
        }
    }
    if text == "无" || text.eq_ignore_ascii_case("none") {
        return None;
    }

    let mut severity = Severity::Medium;
    if let Some(rest) = text.strip_prefix('[') {
        if let Some((tag, after)) = rest.split_once(']') {
            if let Some(sev) = Severity::parse(tag) {
                severity = sev;
                text = after.trim();
            }
        }
    } else if let Some(rest) = text.strip_prefix('【') {
        if let Some((tag, after)) = rest.split_once('】') {
            if let Some(sev) = Severity::parse(tag) {
                severity = sev;
                text = after.trim();
            }
        }
    }

    // Find `path:line` token (optionally wrapped in backticks).
    let (loc_token, after_loc) = if let Some(rest) = text.strip_prefix('`') {
        let (inside, after) = rest.split_once('`')?;
        (inside.trim(), after.trim())
    } else {
        let split_at = text
            .find(|c: char| c.is_whitespace() || c == '—' || c == '：')
            .unwrap_or(text.len());
        (&text[..split_at], text[split_at..].trim())
    };

    let loc_clean = loc_token
        .trim_matches(|c| c == '`' || c == '*' || c == '(' || c == ')')
        .trim_end_matches([',', ';', '：', ':']);
    let (raw_file, raw_line) = loc_clean.rsplit_once(':')?;
    let file = raw_file.trim().trim_start_matches("./").to_string();
    if file.is_empty() || file.contains(' ') {
        return None;
    }
    let line_part = raw_line.trim().trim_start_matches(['L', 'l']);
    let line_digits: String = line_part
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let line: usize = line_digits.parse().ok()?;

    let reason = after_loc
        .trim_start_matches(['—', '–', '-', ':', '：', ' '])
        .trim()
        .to_string();
    Some(ReviewerFocusItem {
        severity,
        file,
        line: line.max(1),
        reason: if reason.is_empty() {
            "审查员标记需重点核查".into()
        } else {
            clip(&reason, 140)
        },
    })
}

/// Parse the `## 主控重点看` section from a reviewer verdict file.
pub fn parse_reviewer_focus(verdict_text: &str) -> Vec<ReviewerFocusItem> {
    let mut in_section = false;
    let mut out = Vec::new();
    for line in verdict_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            if is_focus_heading(trimmed) {
                in_section = true;
                continue;
            } else if in_section {
                break;
            }
        }
        if !in_section {
            continue;
        }
        if let Some(item) = parse_focus_line(trimmed) {
            out.push(item);
        }
    }
    out
}

/// Merge deterministic rule hits with reviewer focus items, deduplicating overlaps on the same
/// file+hunk or within +/- 5 lines, and sort by severity then location.
pub fn merge_and_dedup_risks(
    mut rule_items: Vec<RiskItem>,
    reviewer_items: Vec<ReviewerFocusItem>,
    files: &[DiffFile],
    verdict_file: Option<&str>,
) -> Vec<RiskItem> {
    let find_hunk = |file: &str, line: usize| -> Option<usize> {
        let df = files.iter().find(|f| f.path == file)?;
        df.hunks.iter().position(|h| h.covers_line(line))
    };

    for rev in reviewer_items {
        let rev_hunk = find_hunk(&rev.file, rev.line);
        let overlap_idx = rule_items.iter().position(|existing| {
            if existing.file != rev.file {
                return false;
            }
            let near_line = (existing.line as isize - rev.line as isize).abs() <= 5;
            let same_hunk = rev_hunk.is_some() && existing.hunk_index == rev_hunk;
            let file_level_hit = existing.file_level && (rev_hunk.is_some() || near_line);
            near_line || same_hunk || file_level_hit
        });

        if let Some(idx) = overlap_idx {
            let target = &mut rule_items[idx];
            if rev.severity < target.severity {
                target.severity = rev.severity;
            }
            if target.source == "规则" {
                target.source = "规则+审查员".into();
            }
            if !target.reason.contains(&rev.reason) {
                target.reason = format!("{}；审查员：{}", target.reason, rev.reason);
            }
            if target.verdict_file.is_none() {
                target.verdict_file = verdict_file.map(str::to_string);
            }
        } else {
            rule_items.push(RiskItem {
                severity: rev.severity,
                file: rev.file,
                line: rev.line,
                rule_id: "reviewer_focus".into(),
                category: "审查员提示".into(),
                source: "审查员".into(),
                reason: rev.reason,
                hunk_index: rev_hunk,
                file_level: false,
                verdict_file: verdict_file.map(str::to_string),
            });
        }
    }

    // Deduplicate identical (file, line, rule_id) entries if any.
    rule_items.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.rule_id.cmp(&b.rule_id))
    });
    rule_items.dedup_by(|a, b| a.file == b.file && a.line == b.line && a.rule_id == b.rule_id);
    rule_items
}

#[derive(Debug, Clone, Default)]
pub struct TestChangeStats {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub test_files: usize,
}

fn is_rust_test_attr(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed == "#[test]"
        || trimmed.starts_with("#[tokio::test")
        || trimmed.starts_with("#[sqlx::test")
        || trimmed.starts_with("#[async_std::test")
}

fn extract_rust_fn_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = trimmed
        .strip_prefix("async fn ")
        .or_else(|| trimmed.strip_prefix("fn "))
        .or_else(|| trimmed.strip_prefix("pub fn "))
        .or_else(|| trimmed.strip_prefix("pub async fn "))?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then(|| format!("fn {name}"))
}

fn extract_test_def_at(lines: &[DiffLine], idx: usize) -> Option<String> {
    let cur = &lines[idx];
    if is_rust_test_attr(&cur.text) {
        // Look ahead for the matching `fn <name>` of the same diff kind (skipping extra attrs).
        for next in lines.iter().skip(idx + 1).take(6) {
            if next.kind != cur.kind {
                continue;
            }
            if let Some(fn_name) = extract_rust_fn_name(&next.text) {
                return Some(fn_name);
            }
            let t = next.text.trim();
            if !t.is_empty() && !t.starts_with("#[") && !t.starts_with("//") {
                break;
            }
        }
        return Some(format!("{}@{}", cur.text.trim(), idx));
    }
    extract_test_def_name(&cur.text)
}

/// Deterministically count added, modified, and deleted tests across parsed diff files.
pub fn count_test_changes(files: &[DiffFile]) -> TestChangeStats {
    let mut stats = TestChangeStats::default();
    for file in files {
        let mut file_is_test = is_test_path(&file.path);
        let mut added_defs: Vec<String> = Vec::new();
        let mut deleted_defs: Vec<String> = Vec::new();
        let mut touched_test_hunks = 0usize;

        let mut in_cfg_test = file_is_test;
        for hunk in &file.hunks {
            if hunk.header.contains("mod tests") || hunk.header.contains("cfg(test)") {
                in_cfg_test = true;
                file_is_test = true;
            }
            let mut hunk_add_defs = 0usize;
            let mut hunk_del_defs = 0usize;
            let mut hunk_has_changes = false;
            for (li, l) in hunk.lines.iter().enumerate() {
                if l.text.contains("#[cfg(test)]") || l.text.contains("mod tests") {
                    in_cfg_test = true;
                    file_is_test = true;
                }
                match l.kind {
                    DiffLineKind::Add => {
                        hunk_has_changes = true;
                        if let Some(name) = extract_test_def_at(&hunk.lines, li) {
                            file_is_test = true;
                            hunk_add_defs += 1;
                            added_defs.push(name);
                        }
                    }
                    DiffLineKind::Del => {
                        hunk_has_changes = true;
                        if let Some(name) = extract_test_def_at(&hunk.lines, li) {
                            file_is_test = true;
                            hunk_del_defs += 1;
                            deleted_defs.push(name);
                        }
                    }
                    DiffLineKind::Context => {}
                }
            }
            // If existing lines in a test hunk were modified (without pure add/del of defs), count modified.
            let has_non_def_edits = hunk_has_changes && (hunk_add_defs == 0 && hunk_del_defs == 0);
            if in_cfg_test && has_non_def_edits {
                touched_test_hunks += 1;
            }
        }

        if !file_is_test {
            continue;
        }
        stats.test_files += 1;
        let mut remaining_del = deleted_defs.clone();
        let mut pure_added = 0usize;
        let mut matched_modified = 0usize;
        for name in &added_defs {
            if let Some(pos) = remaining_del.iter().position(|d| d == name) {
                remaining_del.remove(pos);
                matched_modified += 1;
            } else {
                pure_added += 1;
            }
        }
        stats.added += pure_added;
        stats.deleted += remaining_del.len();
        stats.modified += matched_modified + touched_test_hunks;
    }
    stats
}

/// Extract up to 3 summary lines from `result.md` without modifying or truncating the original file.
pub fn extract_author_summary(run: &Path) -> Value {
    let path = run.join("result.md");
    if !path.is_file() {
        return json!({
            "lines": [],
            "path": Value::Null,
            "totalLines": 0,
            "totalChars": 0,
        });
    }
    let raw = read(&path);
    let total_lines = raw.lines().count();
    let total_chars = raw.chars().count();
    let mut picked: Vec<String> = Vec::new();
    let mut in_code_fence = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_code_fence = !in_code_fence;
            continue;
        }
        if in_code_fence || trimmed.is_empty() || trimmed == "---" || trimmed == "***" {
            continue;
        }
        let cleaned = trimmed
            .trim_start_matches('#')
            .trim_start_matches('>')
            .trim()
            .replace("**", "");
        if cleaned.is_empty() {
            continue;
        }
        picked.push(clip(&cleaned, 110));
        if picked.len() >= 3 {
            break;
        }
    }
    json!({
        "lines": picked,
        "path": path.to_string_lossy(),
        "totalLines": total_lines,
        "totalChars": total_chars,
    })
}

fn dir_group_key(path: &str) -> String {
    let p = Path::new(path);
    let parent = p.parent().unwrap_or(Path::new("."));
    let key = parent
        .components()
        .take(2)
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if key.is_empty() {
        ".".into()
    } else {
        format!("{key}/")
    }
}

/// Fold files that did NOT hit any risk item by directory (one line per directory).
pub fn fold_remaining_changes(files: &[DiffFile], risky_files: &BTreeSet<String>) -> Vec<Value> {
    let mut groups: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    for f in files {
        if risky_files.contains(&f.path) {
            continue;
        }
        let dir = dir_group_key(&f.path);
        let entry = groups.entry(dir).or_insert((0, 0, 0));
        entry.0 += 1;
        entry.1 += f.added;
        entry.2 += f.deleted;
    }
    let mut sorted: Vec<(String, (usize, usize, usize))> = groups.into_iter().collect();
    sorted.sort_by(|a, b| {
        (b.1 .1 + b.1 .2)
            .cmp(&(a.1 .1 + a.1 .2))
            .then_with(|| b.1 .0.cmp(&a.1 .0))
            .then_with(|| a.0.cmp(&b.0))
    });
    sorted
        .into_iter()
        .map(|(dir, (file_count, added, deleted))| {
            json!({
                "dir": dir,
                "files": file_count,
                "added": added,
                "deleted": deleted,
            })
        })
        .collect()
}

/// Resolve the unified diff text for a run (total net diff by default, matching `apply`/`review`).
pub fn resolve_diff_text(
    run: &Path,
    meta: &Value,
    round_only: bool,
    context_lines: usize,
) -> Res<String> {
    let archived_patch = |dir: &Path| -> Option<PathBuf> {
        for name in ["applied.patch", "changes.patch"] {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
        None
    };

    if archive::is_record(run) {
        if let Some(p) = archived_patch(run) {
            return Ok(read(&p));
        }
    }

    match changes::chain_changes(run) {
        Ok((chain_meta, top, mut before, after)) => {
            let before_label = if !round_only && chain_meta["worktree"].is_object() {
                let (total_before, label) = worktree::effective_total_base(&top, &chain_meta);
                before = total_before;
                label
            } else if b(&chain_meta, "netBaseAdvanced") {
                if let Some(tree) = changes::net_base_tree(&top, &chain_meta) {
                    before = tree;
                    "net-base"
                } else {
                    "base"
                }
            } else {
                "base"
            };
            let missing = refs::missing_in_top(
                &top,
                run,
                &chain_meta,
                &[(before_label, &before), ("after", &after)],
            );
            if !missing.is_empty() {
                if let Some(p) = archived_patch(run) {
                    return Ok(read(&p));
                }
                return Err(refs::recovery_message(run, &chain_meta, &missing));
            }
            let ctx = format!("-U{context_lines}");
            let bytes = git(
                &top,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-renames",
                    "--color=never",
                    &ctx,
                    &before,
                    &after,
                ],
            )?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        }
        Err(err) => {
            if let Some(p) = archived_patch(run) {
                return Ok(read(&p));
            }
            // If no changes.json (e.g. read-only run or zero changes), return empty diff instead of erroring.
            if !run.join("changes.json").is_file() && s(meta, "mode") == "read-only" {
                return Ok(String::new());
            }
            Err(err)
        }
    }
}

fn load_effective_configs(meta: &Value) -> (Vec<String>, Vec<String>, Vec<String>, RiskConfig) {
    let source = s(&meta["worktree"], "source");
    let top_str = if !source.is_empty() {
        source
    } else {
        s(meta, "top")
    };
    let repo_cfg = if !top_str.is_empty() {
        config::load(Some(Path::new(top_str))).ok().map(|c| c.value)
    } else {
        None
    };

    let controller_paths = {
        let from_meta = strings(&meta["controllerReviewPaths"]);
        if !from_meta.is_empty() {
            from_meta
        } else {
            repo_cfg
                .as_ref()
                .map(|c| strings(&c["controllerReviewPaths"]))
                .unwrap_or_default()
        }
    };

    let protect_rules = {
        let from_meta = strings(&meta["protect"]);
        if !from_meta.is_empty() {
            from_meta
        } else {
            repo_cfg
                .as_ref()
                .map(|c| strings(&c["defaults"]["protect"]))
                .unwrap_or_default()
        }
    };
    let allowed_rules = strings(&meta["allow"]);

    let risk_cfg = if !meta["riskRules"].is_null() {
        parse_risk_config(&meta["riskRules"])
    } else if let Some(cfg) = &repo_cfg {
        parse_risk_config(&cfg["riskRules"])
    } else {
        RiskConfig::default()
    };

    (controller_paths, protect_rules, allowed_rules, risk_cfg)
}

fn load_reviewer_info(run: &Path, review_doc: &Value) -> (Option<String>, Vec<ReviewerFocusItem>, Option<String>) {
    let verdict_file = {
        let vf = s(review_doc, "verdictFile");
        if !vf.is_empty() && Path::new(vf).is_file() {
            Some(vf.to_string())
        } else if run.join("review.md").is_file() {
            Some(run.join("review.md").to_string_lossy().into_owned())
        } else {
            None
        }
    };
    let Some(vf) = verdict_file else {
        return (None, Vec::new(), None);
    };
    let raw = read(&vf);
    if raw.trim().is_empty() {
        return (None, Vec::new(), Some(vf));
    }
    let conclusion = {
        let c = s(review_doc, "conclusion");
        if !c.is_empty() {
            Some(c.to_string())
        } else {
            extract_reviewer_conclusion(&raw)
        }
    };
    let focus = parse_reviewer_focus(&raw);
    (conclusion, focus, Some(vf))
}

/// Build the compact L0 summary (`< 500 bytes`) stored inside `summary.json["card"]` and `index.json`.
pub fn build_l0_summary(risks: &[RiskItem]) -> Value {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for item in risks {
        if let Some(entry) = counts.iter_mut().find(|(cat, _)| cat == &item.category) {
            entry.1 += 1;
        } else {
            counts.push((item.category.clone(), 1));
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let by_cat: Vec<Value> = counts
        .iter()
        .map(|(cat, cnt)| json!({"category": cat, "count": cnt}))
        .collect();
    json!({
        "riskTotal": risks.len(),
        "riskByCategory": by_cat,
        "fastForward": Value::Null,
    })
}

/// Format L0 risk count phrase, e.g. `风险 3：模型可见文字 2、迁移/Schema 1` or `风险 0`.
pub fn format_l0_risk(card: &Value) -> String {
    if !card.is_object() {
        return String::new();
    }
    let total = n(card, "riskTotal");
    if total <= 0 {
        return "风险 0".into();
    }
    let cats: Vec<String> = card["riskByCategory"]
        .as_array()
        .into_iter()
        .flatten()
        .take(3)
        .map(|item| format!("{} {}", s(item, "category"), n(item, "count")))
        .collect();
    let total_cats = card["riskByCategory"].as_array().map_or(0, Vec::len);
    if cats.is_empty() {
        format!("风险 {total}")
    } else if total_cats > 3 {
        format!("风险 {total}：{} 等", cats.join("、"))
    } else {
        format!("风险 {total}：{}", cats.join("、"))
    }
}

/// Format L0/L1 fast-forward / premerge status.
pub fn format_l0_fast_forward(card: &Value, premerge: Option<&Value>) -> String {
    if let Some(pm) = premerge.filter(|v| v.is_object()) {
        return crate::premerge::format_l0(pm);
    }
    if card["premerge"].is_object() {
        return crate::premerge::format_l0(&card["premerge"]);
    }
    match card.get("fastForward") {
        Some(Value::Bool(true)) => "快进 ✓".into(),
        Some(Value::Bool(false)) => "快进 ✗".into(),
        _ => "快进 —".into(),
    }
}

/// Build the full L1 card JSON for a run (deterministic host computation).
pub fn compute_card(run: &Path, meta: &Value, sum: &Value, round_only: bool) -> Res<Value> {
    let run_id = {
        let r = s(meta, "run");
        if !r.is_empty() {
            r.to_string()
        } else {
            run.file_name().unwrap_or_default().to_string_lossy().into_owned()
        }
    };
    let name = s(meta, "name").to_string();
    let state = {
        let st = s(sum, "state");
        if !st.is_empty() {
            st.to_string()
        } else {
            runs::state(run)
        }
    };

    let diff_text = resolve_diff_text(run, meta, round_only, 3).unwrap_or_default();
    let parsed_files = parse_unified_diff(&diff_text);
    let total_files = parsed_files.len();
    let total_added: usize = parsed_files.iter().map(|f| f.added).sum();
    let total_deleted: usize = parsed_files.iter().map(|f| f.deleted).sum();

    let (controller_paths, protect_rules, allowed_rules, risk_cfg) = load_effective_configs(meta);
    let rule_risks = detect_rule_risks(
        &parsed_files,
        &controller_paths,
        &protect_rules,
        &allowed_rules,
        &risk_cfg,
    );

    let review_doc = {
        let f = json(run.join("review.json"));
        if f.is_object() {
            f
        } else if sum["review"].is_object() {
            sum["review"].clone()
        } else {
            Value::Null
        }
    };
    let (rev_conclusion, rev_focus, rev_verdict_file) = load_reviewer_info(run, &review_doc);
    let risks = merge_and_dedup_risks(
        rule_risks,
        rev_focus,
        &parsed_files,
        rev_verdict_file.as_deref(),
    );

    let risky_files: BTreeSet<String> = risks.iter().map(|r| r.file.clone()).collect();
    let test_stats = count_test_changes(&parsed_files);
    let other_changes = fold_remaining_changes(&parsed_files, &risky_files);
    let author_summary = extract_author_summary(run);
    let mut l0 = build_l0_summary(&risks);
    let premerge_live = crate::premerge::status_for_run(run, meta, sum, None);
    let ff_bool = match premerge_live.as_ref().map(|pm| s(pm, "status")) {
        Some("ready") => json!(true),
        Some("stale" | "conflict" | "failed") => json!(false),
        _ => Value::Null,
    };
    l0["fastForward"] = ff_bool.clone();
    if let Some(ref pm) = premerge_live {
        l0["premerge"] = pm.clone();
    }

    let sign_status = crate::review::sign_status(run, meta, &review_doc, false);
    let needs_sign = sign_status
        .as_ref()
        .is_some_and(|sg| b(sg, "required") && !b(sg, "valid"));

    let review_section = if review_doc.is_object() {
        json!({
            "enabled": true,
            "status": review_doc["status"],
            "verdict": review_doc["verdict"],
            "round": review_doc["round"],
            "reworkRounds": review_doc["reworkRounds"],
            "upgradedBy": review_doc["upgradedBy"],
            "cancelledBy": review_doc["cancelledBy"],
            "conclusion": rev_conclusion,
            "verdictFile": rev_verdict_file,
        })
    } else {
        json!({"enabled": false})
    };

    let accept_val = if sum["accept"].is_object() {
        sum["accept"].clone()
    } else {
        Value::Null
    };

    let mut next_cmds = vec![
        json!({"label": "仅看风险改动", "command": format!("delegate diff {run_id} --risky")}),
        json!({"label": "查看完整改动", "command": format!("delegate diff {run_id}{}", if meta["worktree"].is_object() { " --total" } else { "" })}),
    ];
    if needs_sign {
        next_cmds.push(json!({"label": "受控路径签字", "command": format!("delegate sign {run_id}")}));
    }
    next_cmds.push(json!({"label": "确认无误合入", "command": format!("delegate apply {run_id}")}));

    let rule_count = risks.iter().filter(|r| r.source.contains("规则")).count();
    let rev_count = risks.iter().filter(|r| r.source.contains("审查员")).count();

    Ok(json!({
        "run": run_id,
        "name": name,
        "state": state,
        "changes": {
            "files": total_files,
            "added": total_added,
            "deleted": total_deleted,
        },
        "l0": l0,
        "fastForward": ff_bool,
        "premerge": premerge_live.unwrap_or(Value::Null),
        "author": author_summary,
        "review": review_section,
        "risks": risks.iter().map(RiskItem::to_json).collect::<Vec<_>>(),
        "riskStats": {
            "total": risks.len(),
            "ruleHits": rule_count,
            "reviewerHints": rev_count,
        },
        "tests": {
            "added": test_stats.added,
            "modified": test_stats.modified,
            "deleted": test_stats.deleted,
            "testFiles": test_stats.test_files,
        },
        "accept": accept_val,
        "otherChanges": other_changes,
        "next": next_cmds,
    }))
}

/// Compute and persist `run/card.json` + compact `sum["card"]` during supervisor settlement.
/// Never fails the run: auxiliary failure degrades gracefully with a diagnostic warning.
pub fn generate_and_save(run: &Path, meta: &Value, sum: &mut Value) {
    if s(meta, "mode") != "write" {
        return;
    }
    match compute_card(run, meta, sum, false) {
        Ok(card_doc) => {
            let _ = write_json(run.join("card.json"), &card_doc);
            sum["card"] = card_doc["l0"].clone();
        }
        Err(err) => {
            let _ = append(run.join("stderr.log"), &format!("card generation note: {err}\n"));
        }
    }
}

/// Render L1 merge review card as concise human-readable text (<= 1 screen).
pub fn render_card_human(card: &Value) -> String {
    let run_id = s(card, "run");
    let name = s(card, "name").replace(['\n', '\r'], " ");
    let state = s(card, "state");
    let state_zh = match state {
        "delivered" => "已交付",
        "answered" => "已答复",
        "rejected" => "未通过",
        "running" => "运行中",
        "stopped" => "已停止",
        "failed" => "失败",
        _ => state,
    };
    let files = n(&card["changes"], "files");
    let added = n(&card["changes"], "added");
    let deleted = n(&card["changes"], "deleted");
    let ff = format_l0_fast_forward(&card["l0"], card.get("premerge"));

    let mut out = Vec::new();
    out.push(format!(
        "审查卡 · {name} ({run_id}) · {state_zh} · 改 {files} 个文件 +{added}/-{deleted} · {ff}"
    ));

    let accept_line = if card["accept"].is_object() {
        human_accept_label(&card["accept"], false)
    } else {
        "未设验收".into()
    };
    let rev = &card["review"];
    let review_head = if b(rev, "enabled") {
        let v = s(rev, "verdict");
        let st = s(rev, "status");
        if st == "cancelled" {
            format!("已取消（{}）", s(rev, "cancelledBy"))
        } else if !v.is_empty() {
            let up = s(rev, "upgradedBy");
            if up.is_empty() {
                v.to_string()
            } else {
                format!("{v}（{up}）")
            }
        } else {
            st.to_string()
        }
    } else {
        "未启用".into()
    };
    out.push(format!("验收：{accept_line} · 审查：{review_head}"));
    if card["premerge"].is_object() {
        let pm = &card["premerge"];
        let pm_st = s(pm, "status");
        let pm_label = crate::premerge::format_l0(pm);
        let mut extra_parts = Vec::new();
        if let Some(secs) = pm["elapsedSeconds"].as_f64() {
            extra_parts.push(format!("{secs:.1}s"));
        }
        let ran = strings(&pm["ranLayers"]);
        if !ran.is_empty() {
            let tag = if b(pm, "reusedAccept") {
                format!("沿用 {}", ran.join("、"))
            } else {
                format!("已跑 {}", ran.join("、"))
            };
            extra_parts.push(tag);
        }
        if matches!(pm_st, "conflict" | "failed") && !s(pm, "log").is_empty() {
            extra_parts.push(format!("日志 {}", s(pm, "log")));
        }
        if extra_parts.is_empty() {
            out.push(format!("预合并：{pm_label}"));
        } else {
            out.push(format!("预合并：{pm_label}（{}）", extra_parts.join(" · ")));
        }
    }
    out.push(String::new());

    // 1. 做了什么
    out.push("1. 做了什么".into());
    let author_lines = strings(&card["author"]["lines"]);
    if author_lines.is_empty() {
        out.push("   （无作者自述）".into());
    } else {
        for l in &author_lines {
            out.push(format!("   {l}"));
        }
    }
    let result_path = s(&card["author"], "path");
    if !result_path.is_empty() {
        out.push(format!(
            "   原文：{result_path}（共 {} 行 / {} 字）",
            n(&card["author"], "totalLines"),
            n(&card["author"], "totalChars")
        ));
    }
    if b(rev, "enabled") {
        let verdict = s(rev, "verdict");
        let concl = s(rev, "conclusion");
        let vf = s(rev, "verdictFile");
        let concl_part = if concl.is_empty() {
            if s(rev, "status") == "cancelled" {
                format!("审查已取消（{}）", s(rev, "cancelledBy"))
            } else {
                "无正文结论".into()
            }
        } else {
            concl.to_string()
        };
        let vf_part = if vf.is_empty() {
            String::new()
        } else {
            format!("（原文：{vf}）")
        };
        let v_prefix = if verdict.is_empty() { "—" } else { verdict };
        out.push(format!("   审查员：{v_prefix} — {concl_part}{vf_part}"));
    }
    out.push(String::new());

    // 2. 风险项
    let risks = card["risks"].as_array().cloned().unwrap_or_default();
    let r_total = risks.len();
    let r_rule = n(&card["riskStats"], "ruleHits");
    let r_rev = n(&card["riskStats"], "reviewerHints");
    out.push(format!(
        "2. 风险项（共 {r_total} 项；规则 {r_rule} / 审查员 {r_rev}）"
    ));
    if risks.is_empty() {
        out.push("   无".into());
    } else {
        for item in risks.iter().take(MAX_L1_RISK_ITEMS) {
            out.push(format!(
                "   - [{}] {}  {}（{}）— {}",
                s(item, "severityLabel"),
                s(item, "location"),
                s(item, "category"),
                s(item, "source"),
                s(item, "reason")
            ));
        }
        if risks.len() > MAX_L1_RISK_ITEMS {
            out.push(format!(
                "   … 另有 {} 项（完整见 delegate card {run_id} --json 或 delegate diff {run_id} --risky）",
                risks.len() - MAX_L1_RISK_ITEMS
            ));
        }
    }
    out.push(String::new());

    // 3. 测试与验收
    out.push("3. 测试与验收".into());
    let t = &card["tests"];
    out.push(format!(
        "   测试变化：新增 +{} / 修改 ~{} / 删除 -{}（涉及 {} 个测试文件）",
        n(t, "added"),
        n(t, "modified"),
        n(t, "deleted"),
        n(t, "testFiles")
    ));
    let acc = &card["accept"];
    if acc.is_object() {
        if b(acc, "failClosed") {
            out.push(format!(
                "   全量兜底（fail-closed）：{}",
                s(acc, "failClosedReason")
            ));
        }
        if let Some(layers) = acc["layers"].as_array().filter(|l| !l.is_empty()) {
            for layer in layers {
                let st = s(layer, "status");
                let st_zh = match st {
                    "passed" => "通过",
                    "failed" => "失败",
                    "skipped" => "跳过",
                    "reused" => "复用",
                    "cancelled" => "取消",
                    "not_run" => "未跑",
                    _ => st,
                };
                let lname = s(layer, "name");
                let tier = s(layer, "tier");
                let cmd = clip(s(layer, "command"), 70);
                let reason = s(layer, "reason");
                let secs = layer["seconds"]
                    .as_f64()
                    .map(|v| format!(" {v:.1}s"))
                    .unwrap_or_default();
                if matches!(st, "skipped" | "reused" | "not_run" | "cancelled") {
                    out.push(format!("   - [{st_zh}] {lname} ({tier}): {reason}"));
                } else {
                    out.push(format!("   - [{st_zh}] {lname} ({tier}{secs}): {cmd}"));
                }
            }
        } else {
            let st_zh = if b(acc, "ok") {
                "通过"
            } else if b(acc, "cancelled") {
                "取消"
            } else if b(acc, "notExecuted") {
                "未执行"
            } else {
                "失败"
            };
            out.push(format!(
                "   - [{st_zh}] accept: {}",
                clip(s(acc, "command"), 80)
            ));
        }
    } else {
        out.push("   验收：未配置".into());
    }
    out.push(String::new());

    // 4. 其余改动
    out.push("4. 其余改动（未命中风险项，按目录折叠）".into());
    let others = card["otherChanges"].as_array().cloned().unwrap_or_default();
    if others.is_empty() {
        out.push("   无".into());
    } else {
        for g in others.iter().take(MAX_L1_OTHER_DIRS) {
            out.push(format!(
                "   - {}  {} 个文件 +{}/-{}",
                s(g, "dir"),
                n(g, "files"),
                n(g, "added"),
                n(g, "deleted")
            ));
        }
        if others.len() > MAX_L1_OTHER_DIRS {
            out.push(format!("   … 另有 {} 个目录", others.len() - MAX_L1_OTHER_DIRS));
        }
    }
    out.push(String::new());

    // 5. 下一步
    out.push("5. 下一步".into());
    if let Some(next_list) = card["next"].as_array() {
        for item in next_list {
            out.push(format!("   - {}：{}", s(item, "label"), s(item, "command")));
        }
    }
    out.join("\n")
}

/// Render L2 (`delegate diff <run> --risky`): only risk-matched hunks with surrounding context.
pub fn render_risky_diff(
    run: &Path,
    meta: &Value,
    round_only: bool,
    filter_paths: &[String],
) -> Res<(String, usize)> {
    let diff_text = resolve_diff_text(run, meta, round_only, DEFAULT_RISKY_CONTEXT_LINES)?;
    let parsed_files = parse_unified_diff(&diff_text);
    let (controller_paths, protect_rules, allowed_rules, risk_cfg) = load_effective_configs(meta);
    let rule_risks = detect_rule_risks(
        &parsed_files,
        &controller_paths,
        &protect_rules,
        &allowed_rules,
        &risk_cfg,
    );
    let review_doc = {
        let f = json(run.join("review.json"));
        if f.is_object() {
            f
        } else {
            json(run.join("summary.json"))["review"].clone()
        }
    };
    let (_, rev_focus, rev_verdict_file) = load_reviewer_info(run, &review_doc);
    let risks = merge_and_dedup_risks(
        rule_risks,
        rev_focus,
        &parsed_files,
        rev_verdict_file.as_deref(),
    );

    let run_id = {
        let r = s(meta, "run");
        if !r.is_empty() {
            r.to_string()
        } else {
            run.file_name().unwrap_or_default().to_string_lossy().into_owned()
        }
    };

    if risks.is_empty() {
        return Ok((
            format!("无命中风险项的改动；完整改动：delegate diff {run_id}\n"),
            0,
        ));
    }

    let path_allowed = |path: &str| -> bool {
        if filter_paths.is_empty() {
            return true;
        }
        filter_paths
            .iter()
            .any(|fp| path == fp || path.starts_with(&format!("{}/", fp.trim_end_matches('/'))))
    };

    let mut out = String::new();
    let mut emitted_hunks = 0usize;
    let mut covered_risk_files = BTreeSet::new();

    for file in &parsed_files {
        if !path_allowed(&file.path) {
            continue;
        }
        let file_risks: Vec<&RiskItem> = risks.iter().filter(|r| r.file == file.path).collect();
        if file_risks.is_empty() {
            continue;
        }
        covered_risk_files.insert(file.path.clone());
        let include_all_hunks = file_risks.iter().any(|r| r.file_level);
        let mut selected_hunks: BTreeMap<usize, Vec<&RiskItem>> = BTreeMap::new();
        if include_all_hunks {
            for hi in 0..file.hunks.len() {
                selected_hunks.entry(hi).or_default();
            }
        }
        for r in &file_risks {
            if let Some(hi) = r.hunk_index {
                if hi < file.hunks.len() {
                    selected_hunks.entry(hi).or_default().push(r);
                }
            } else if !file.hunks.is_empty() {
                // Attach to nearest hunk.
                let nearest = file
                    .hunks
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, h)| (h.first_changed_line() as isize - r.line as isize).unsigned_abs())
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                selected_hunks.entry(nearest).or_default().push(r);
            }
        }

        // File-level risk banner
        for r in file_risks.iter().filter(|r| r.file_level || file.hunks.is_empty()) {
            out.push_str(&format!(
                "# 风险命中: [{}] {}:{} {}（{}）— {}\n",
                r.severity.zh_label(),
                r.file,
                r.line,
                r.category,
                r.source,
                r.reason
            ));
        }
        for hl in &file.header_lines {
            out.push_str(hl);
            out.push('\n');
        }
        for (hi, hrisks) in selected_hunks {
            for r in hrisks.iter().filter(|r| !r.file_level) {
                out.push_str(&format!(
                    "# 风险命中: [{}] {}:{} {}（{}）— {}\n",
                    r.severity.zh_label(),
                    r.file,
                    r.line,
                    r.category,
                    r.source,
                    r.reason
                ));
            }
            let hunk = &file.hunks[hi];
            out.push_str(&hunk.header);
            out.push('\n');
            for l in &hunk.lines {
                let prefix = match l.kind {
                    DiffLineKind::Context => ' ',
                    DiffLineKind::Add => '+',
                    DiffLineKind::Del => '-',
                };
                out.push(prefix);
                out.push_str(&l.text);
                out.push('\n');
            }
            emitted_hunks += 1;
        }
    }

    // Any reviewer focus items pointing to files outside the diff (never silently drop).
    for r in risks.iter().filter(|r| !covered_risk_files.contains(&r.file) && path_allowed(&r.file)) {
        out.push_str(&format!(
            "# 风险命中（不在 diff hunk 内）: [{}] {}:{} {}（{}）— {}\n",
            r.severity.zh_label(),
            r.file,
            r.line,
            r.category,
            r.source,
            r.reason
        ));
    }

    out.push_str(&format!(
        "\n# 下一步：完整改动 delegate diff {run_id}{} · 合入 delegate apply {run_id}\n",
        if meta["worktree"].is_object() { " --total" } else { "" }
    ));

    Ok((out, emitted_hunks.max(risks.len())))
}

/// CLI entrypoint for `delegate card [run] [--json] [--round]`.
pub fn card_command(args: &[String]) -> Res<i32> {
    let mut json_mode = output::json_enabled();
    let mut round_only = false;
    let mut pos: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--json" => json_mode = true,
            "--round" => round_only = true,
            "--total" => round_only = false,
            other if other.starts_with('-') => {
                return Err(format!("card: unknown flag {other}"));
            }
            _ => pos.push(arg.clone()),
        }
    }
    if pos.len() > 1 {
        return Err("card accepts at most one run reference".into());
    }
    let reference = pos.first().map(String::as_str).unwrap_or("last");
    let run = match runs::resolve_head(reference) {
        Ok(r) => r,
        Err(err) => match history::resolve_archive_record(reference) {
            Some(rec) => rec,
            None => return Err(err),
        },
    };
    let meta = json(run.join("meta.json"));
    let sum = json(run.join("summary.json"));
    let card_doc = compute_card(&run, &meta, &sum, round_only)?;
    // Zero-lock wakeup if premerge is stale or worker died.
    if card_doc["premerge"].is_object()
        && matches!(s(&card_doc["premerge"], "status"), "stale" | "crashed")
    {
        let src = s(&meta["worktree"], "source");
        if !src.is_empty() {
            crate::premerge::spawn_worker(Path::new(src));
        }
    }
    if json_mode {
        println!("{}", serde_json::to_string_pretty(&card_doc).unwrap_or_else(|_| card_doc.to_string()));
    } else {
        println!("{}", render_card_human(&card_doc));
    }
    Ok(0)
}

/// Helper for reading `fs::metadata` cleanly if needed later.
#[allow(dead_code)]
fn _keep_fs_import() {
    let _ = fs::metadata(".");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_risk_rules_schema() {
        assert!(validate_risk_rules(None).is_ok());
        assert!(validate_risk_rules(Some(&json!({
            "disable": ["config_constant"],
            "largeDeletionLines": 30,
            "extra": [{
                "id": "sandbox",
                "label": "沙箱",
                "severity": "high",
                "paths": ["crates/sandbox/**"],
                "content": ["seccomp"],
                "reason": "沙箱配置变化"
            }]
        })))
        .is_ok());
        assert!(validate_risk_rules(Some(&json!([{
            "id": "custom",
            "paths": ["foo/**"]
        }])))
        .is_ok());
        for bad in [
            json!({"disable": ["unknown_rule"]}),
            json!({"largeDeletionLines": 2}),
            json!({"extra": [{"id": "x"}]}),
            json!({"extra": [{"id": "x", "paths": ["a"], "severity": "extreme"}]}),
            json!({"badField": true}),
        ] {
            assert!(validate_risk_rules(Some(&bad)).is_err(), "should reject {bad}");
        }
    }

    #[test]
    fn detects_all_nine_builtin_risk_rules() {
        let mut big_del = String::new();
        for i in 1..=45 {
            big_del.push_str(&format!("-let old_line_{i} = {i};\n"));
        }
        let sample_diff = format!(
            "diff --git a/skills/foo/SKILL.md b/skills/foo/SKILL.md\n\
             --- a/skills/foo/SKILL.md\n\
             +++ b/skills/foo/SKILL.md\n\
             @@ -1,2 +1,3 @@\n\
              # Foo\n\
             +New prompt text\n\
              end\n\
             diff --git a/migrations/0042_add_col.sql b/migrations/0042_add_col.sql\n\
             new file mode 100644\n\
             --- /dev/null\n\
             +++ b/migrations/0042_add_col.sql\n\
             @@ -0,0 +1,2 @@\n\
             +ALTER TABLE runs ADD COLUMN priority INT;\n\
             diff --git a/tests/unit_test.rs b/tests/unit_test.rs\n\
             --- a/tests/unit_test.rs\n\
             +++ b/tests/unit_test.rs\n\
             @@ -10,5 +10,5 @@\n\
              #[test]\n\
             +#[ignore]\n\
              fn check_something() {{\n\
             -    assert_eq!(1, 1);\n\
             -    assert_eq!(2, 2);\n\
              }}\n\
             diff --git a/Cargo.lock b/Cargo.lock\n\
             --- a/Cargo.lock\n\
             +++ b/Cargo.lock\n\
             @@ -1,2 +1,3 @@\n\
              version = 3\n\
             +name = \"extra-dep\"\n\
             diff --git a/docs/decision/001.md b/docs/decision/001.md\n\
             --- a/docs/decision/001.md\n\
             +++ b/docs/decision/001.md\n\
             @@ -1,2 +1,3 @@\n\
              # ADR\n\
             +edited protected doc\n\
             diff --git a/crates/core/src/limits.rs b/crates/core/src/limits.rs\n\
             --- a/crates/core/src/limits.rs\n\
             +++ b/crates/core/src/limits.rs\n\
             @@ -1,4 +1,6 @@\n\
             -const MAX_BATCH: usize = 100;\n\
             +const MAX_BATCH: usize = 500;\n\
             +let guard = Mutex::new(0);\n\
             +let endpoint = \"http://10.20.30.40:8080\";\n\
             diff --git a/crates/core/src/legacy.rs b/crates/core/src/legacy.rs\n\
             --- a/crates/core/src/legacy.rs\n\
             +++ b/crates/core/src/legacy.rs\n\
             @@ -1,46 +1,2 @@\n\
             {big_del}\
             +let kept = 0;\n"
        );
        let files = parse_unified_diff(&sample_diff);
        let risks = detect_rule_risks(
            &files,
            &["skills/*/SKILL.md".into()],
            &["docs/decision/".into()],
            &[],
            &RiskConfig::default(),
        );
        let ids: BTreeSet<&str> = risks.iter().map(|r| r.rule_id.as_str()).collect();
        for expected in BUILTIN_RULE_IDS {
            assert!(ids.contains(expected), "missing rule {expected}, got {ids:?}");
        }
    }

    #[test]
    fn parses_reviewer_focus_and_deduplicates_with_rules() {
        let verdict = "PASS\n\
            实现完整，边界清晰，仅一处并发锁需主控确认。\n\n\
            ## 主控重点看\n\
            - [高] `crates/core/src/limits.rs:2` — 锁粒度扩大，确认不会与上游死锁\n\
            - [中] crates/core/src/other.rs:15 — 新增重试分支未覆盖超时场景\n";
        let concl = extract_reviewer_conclusion(verdict).unwrap();
        assert!(concl.contains("实现完整"));
        let focus = parse_reviewer_focus(verdict);
        assert_eq!(focus.len(), 2);
        assert_eq!(focus[0].file, "crates/core/src/limits.rs");
        assert_eq!(focus[0].line, 2);
        assert_eq!(focus[0].severity, Severity::High);

        let diff = "diff --git a/crates/core/src/limits.rs b/crates/core/src/limits.rs\n\
            --- a/crates/core/src/limits.rs\n\
            +++ b/crates/core/src/limits.rs\n\
            @@ -1,2 +1,3 @@\n\
             fn lock_it() {\n\
            +    let g = Mutex::new(1);\n\
             }\n\
            diff --git a/crates/core/src/other.rs b/crates/core/src/other.rs\n\
            --- a/crates/core/src/other.rs\n\
            +++ b/crates/core/src/other.rs\n\
            @@ -12,4 +12,6 @@\n\
             fn retry() {\n\
            +    if attempts > 0 { return; }\n\
             }\n";
        let files = parse_unified_diff(diff);
        let rule_risks = detect_rule_risks(&files, &[], &[], &[], &RiskConfig::default());
        assert_eq!(rule_risks.len(), 1);

        let merged = merge_and_dedup_risks(rule_risks, focus, &files, Some("review/round-1-verdict.md"));
        // 1 rule hit + 2 reviewer items, where 1 overlaps -> total 2 (deduplicated!).
        assert_eq!(merged.len(), 2, "{merged:?}");
        assert_eq!(merged[0].file, "crates/core/src/limits.rs");
        assert_eq!(merged[0].source, "规则+审查员");
        assert!(merged[0].reason.contains("锁粒度扩大"));
        assert_eq!(merged[1].file, "crates/core/src/other.rs");
        assert_eq!(merged[1].source, "审查员");
    }

    #[test]
    fn counts_added_modified_and_deleted_tests() {
        let diff = "diff --git a/tests/sample_test.rs b/tests/sample_test.rs\n\
            --- a/tests/sample_test.rs\n\
            +++ b/tests/sample_test.rs\n\
            @@ -1,6 +1,10 @@\n\
             #[test]\n\
             fn existing_modified() {\n\
            -    assert_eq!(1, 1);\n\
            +    assert_eq!(1, 1);\n\
            +    assert_eq!(2, 2);\n\
             }\n\
            +#[test]\n\
            +fn brand_new_test() {\n\
            +    assert!(true);\n\
            +}\n\
            @@ -20,4 +24,0 @@\n\
            -#[test]\n\
            -fn deleted_test() {\n\
            -    assert!(true);\n\
            -}\n";
        let files = parse_unified_diff(diff);
        let stats = count_test_changes(&files);
        assert_eq!(stats.test_files, 1);
        assert_eq!(stats.added, 1);
        assert_eq!(stats.deleted, 1);
    }

    #[test]
    fn does_not_flag_local_dir_paths_as_internal_hostname() {
        assert!(detect_secret_or_internal("path = \"/home/user/.local/run/delegate/x\"").is_none());
        assert!(detect_secret_or_internal("host = \"https://build.internal/api\"").is_some());
        assert!(detect_secret_or_internal("host = \"ssh://dev@box.local:22\"").is_some());
        assert!(detect_secret_or_internal("ip = \"192.168.1.50\"").is_some());
        assert!(detect_secret_or_internal("ip = \"127.0.0.1\"").is_none());
    }
}
