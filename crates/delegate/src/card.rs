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
    "size_budget",
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
pub struct SizeBudgetRule {
    pub paths: Vec<String>,
    pub max_lines: Option<usize>,
    pub max_test_fn_lines: Option<usize>,
    pub max_tokens: Option<usize>,
    pub label: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RiskConfig {
    pub disabled: BTreeSet<String>,
    pub large_deletion_lines: usize,
    pub size_budgets: Vec<SizeBudgetRule>,
    pub extra: Vec<CustomRiskRule>,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            disabled: BTreeSet::new(),
            large_deletion_lines: DEFAULT_LARGE_DELETION_LINES,
            size_budgets: Vec::new(),
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
        if !["disable", "largeDeletionLines", "sizeBudgets", "extra"].contains(&key.as_str()) {
            return Err(format!(
                "riskRules has unknown field {key:?} (allowed: disable, largeDeletionLines, sizeBudgets, extra)"
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
    if let Some(sb) = obj.get("sizeBudgets") {
        let arr = sb
            .as_array()
            .ok_or("riskRules.sizeBudgets must be an array of size budget rules")?;
        for (idx, item) in arr.iter().enumerate() {
            let entry = item
                .as_object()
                .ok_or_else(|| format!("riskRules.sizeBudgets[{idx}] must be an object"))?;
            for key in entry.keys() {
                if !["paths", "maxLines", "maxTestFnLines", "maxTokens", "label"]
                    .contains(&key.as_str())
                {
                    return Err(format!(
                        "riskRules.sizeBudgets[{idx}] has unknown field {key:?} (allowed: paths, maxLines, maxTestFnLines, maxTokens, label)"
                    ));
                }
            }
            let Some(paths) = entry.get("paths").and_then(Value::as_array) else {
                return Err(format!(
                    "riskRules.sizeBudgets[{idx}].paths must be a non-empty array of glob strings"
                ));
            };
            if paths.is_empty()
                || !paths
                    .iter()
                    .all(|x| x.as_str().is_some_and(|s| !s.is_empty() && !s.contains('\0')))
            {
                return Err(format!(
                    "riskRules.sizeBudgets[{idx}].paths must be a non-empty array of non-empty strings"
                ));
            }
            let mut has_limit = false;
            for lim_key in ["maxLines", "maxTestFnLines", "maxTokens"] {
                if let Some(lv) = entry.get(lim_key) {
                    if !lv.as_u64().is_some_and(|n| n >= 1) {
                        return Err(format!(
                            "riskRules.sizeBudgets[{idx}].{lim_key} must be a positive integer"
                        ));
                    }
                    has_limit = true;
                }
            }
            if !has_limit {
                return Err(format!(
                    "riskRules.sizeBudgets[{idx}] must specify at least one of maxLines, maxTestFnLines, or maxTokens"
                ));
            }
            if let Some(lbl) = entry.get("label") {
                if !lbl
                    .as_str()
                    .is_some_and(|s| !s.trim().is_empty() && !s.contains('\0'))
                {
                    return Err(format!(
                        "riskRules.sizeBudgets[{idx}].label must be a non-empty string"
                    ));
                }
            }
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
        if let Some(arr) = val["sizeBudgets"].as_array() {
            for item in arr {
                let paths = strings(&item["paths"]);
                if paths.is_empty() {
                    continue;
                }
                let max_lines = item["maxLines"].as_u64().filter(|&n| n >= 1).map(|n| n as usize);
                let max_test_fn_lines = item["maxTestFnLines"]
                    .as_u64()
                    .filter(|&n| n >= 1)
                    .map(|n| n as usize);
                let max_tokens = item["maxTokens"]
                    .as_u64()
                    .filter(|&n| n >= 1)
                    .map(|n| n as usize);
                if max_lines.is_none() && max_test_fn_lines.is_none() && max_tokens.is_none() {
                    continue;
                }
                let label = {
                    let l = s(item, "label").trim();
                    (!l.is_empty()).then(|| l.to_string())
                };
                cfg.size_budgets.push(SizeBudgetRule {
                    paths,
                    max_lines,
                    max_test_fn_lines,
                    max_tokens,
                    label,
                });
            }
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

fn is_lockfile_self_version_only(file: &DiffFile) -> bool {
    if file.is_new || file.is_deleted || file.hunks.is_empty() {
        return false;
    }
    let lower = file.path.to_ascii_lowercase();
    let is_toml_lock = lower.ends_with("cargo.lock")
        || lower.ends_with("uv.lock")
        || lower.ends_with("poetry.lock");
    let is_pkg_lock = lower.ends_with("package-lock.json");
    if !is_toml_lock && !is_pkg_lock {
        return false;
    }

    let is_toml_version_line = |line: &str| -> bool {
        let t = line.trim();
        let Some((k, v)) = t.split_once('=') else {
            return false;
        };
        k.trim() == "version" && {
            let vt = v.trim();
            vt.len() >= 2 && vt.starts_with('"') && vt.ends_with('"')
        }
    };
    let is_json_version_line = |line: &str| -> bool {
        let t = line.trim().trim_end_matches(',');
        let Some((k, v)) = t.split_once(':') else {
            return false;
        };
        k.trim() == "\"version\"" && {
            let vt = v.trim();
            vt.len() >= 2 && vt.starts_with('"') && vt.ends_with('"')
        }
    };

    let mut add_count = 0usize;
    let mut del_count = 0usize;

    for hunk in &file.hunks {
        let mut hunk_has_change = false;
        for (idx, l) in hunk.lines.iter().enumerate() {
            if l.kind == DiffLineKind::Context {
                continue;
            }
            hunk_has_change = true;
            if is_toml_lock {
                if !is_toml_version_line(&l.text) {
                    return false;
                }
                // External registry/git packages in Cargo.lock/uv.lock have `source =` or
                // `checksum =` inside the same `[[package]]` block. Local workspace/self packages do not.
                for ctx in hunk.lines[idx + 1..].iter() {
                    let ct = ctx.text.trim();
                    if ct.is_empty() || ct.starts_with("[[") {
                        break;
                    }
                    if ct.starts_with("source =")
                        || ct.starts_with("source=")
                        || ct.starts_with("checksum =")
                        || ct.starts_with("checksum=")
                    {
                        return false;
                    }
                }
            } else if is_pkg_lock {
                if !is_json_version_line(&l.text) {
                    return false;
                }
                for ctx in hunk.lines[idx + 1..].iter() {
                    let ct = ctx.text.trim();
                    if ct == "}" || ct == "}," {
                        break;
                    }
                    if ct.starts_with("\"resolved\":") || ct.starts_with("\"integrity\":") {
                        return false;
                    }
                }
            }
            match l.kind {
                DiffLineKind::Add => add_count += 1,
                DiffLineKind::Del => del_count += 1,
                DiffLineKind::Context => {}
            }
        }
        if !hunk_has_change {
            return false;
        }
    }

    add_count > 0 && add_count == del_count
}

fn is_comment_line(trimmed: &str) -> bool {
    trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with("<!--")
        || (trimmed.starts_with("# ") || trimmed == "#")
}

/// Strip quoted string literals so keywords/numbers inside strings do not trigger code rules.
/// Preserves Rust lifetimes like `'static` / `'a` when `preserve_single_quotes` is true.
fn strip_string_literals(line: &str, preserve_single_quotes: bool) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            out.push(' ');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
        } else if b == b'\'' {
            if preserve_single_quotes {
                // Skip only Rust char literals like `'x'` or `'\n'`, keep lifetimes like `'static`.
                if i + 2 < bytes.len() && bytes[i + 1] != b'\\' && bytes[i + 2] == b'\'' {
                    out.push(' ');
                    i += 3;
                } else if i + 3 < bytes.len() && bytes[i + 1] == b'\\' && bytes[i + 3] == b'\'' {
                    out.push(' ');
                    i += 4;
                } else {
                    out.push('\'');
                    i += 1;
                }
            } else {
                out.push(' ');
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'\'' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
        } else if b == b'`' && !preserve_single_quotes {
            out.push(' ');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'`' {
                    i += 1;
                    break;
                }
                i += 1;
            }
        } else {
            let ch = line[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn has_token_prefix(code: &str, needle: &str) -> bool {
    let bytes = code.as_bytes();
    let mut start = 0;
    while let Some(pos) = code[start..].find(needle) {
        let abs = start + pos;
        let prev_ok = abs == 0
            || !(bytes[abs - 1].is_ascii_alphanumeric()
                || bytes[abs - 1] == b'_'
                || bytes[abs - 1] == b'.');
        if prev_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn is_skip_or_ignore_marker(path: &str, line: &str) -> Option<&'static str> {
    let trimmed = line.trim();
    if is_comment_line(trimmed) {
        return None;
    }
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".rs") {
        let code = strip_string_literals(trimmed, true);
        let bytes = code.as_bytes();
        let mut start = 0;
        while let Some(pos) = code[start..].find("#[ignore") {
            let abs = start + pos;
            let after = abs + "#[ignore".len();
            let next_ok = bytes
                .get(after)
                .is_some_and(|&b| matches!(b, b']' | b'=' | b'(' | b' ' | b'\t'));
            if next_ok {
                return Some("#[ignore]");
            }
            start = abs + 1;
        }
        return None;
    }
    if lower.ends_with(".py") {
        if trimmed.starts_with("@pytest.mark.skip") {
            return Some("@pytest.mark.skip");
        }
        if trimmed.starts_with("@unittest.skip") {
            return Some("@unittest.skip");
        }
        return None;
    }
    if lower.ends_with(".js")
        || lower.ends_with(".jsx")
        || lower.ends_with(".ts")
        || lower.ends_with(".tsx")
        || lower.ends_with(".mjs")
        || lower.ends_with(".cjs")
        || lower.ends_with(".mts")
        || lower.ends_with(".cts")
    {
        let code = strip_string_literals(trimmed, false);
        for (needle, label) in [
            ("test.skip(", "test.skip"),
            ("it.skip(", "it.skip"),
            ("describe.skip(", "describe.skip"),
            ("xit(", "xit"),
            ("xtest(", "xtest"),
            ("xdescribe(", "xdescribe"),
        ] {
            if has_token_prefix(&code, needle) {
                return Some(label);
            }
        }
        return None;
    }
    if lower.ends_with(".go") {
        let code = strip_string_literals(trimmed, false);
        for (needle, label) in [
            ("t.Skip(", "t.Skip"),
            ("t.SkipNow(", "t.SkipNow"),
            ("t.Skipf(", "t.Skipf"),
        ] {
            if has_token_prefix(&code, needle) {
                return Some(label);
            }
        }
        return None;
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
            let prev_ok = i == 0 || (!bytes[i - 1].is_ascii_alphabetic() && bytes[i - 1] != b'_');
            if prev_ok {
                return true;
            }
        }
    }
    false
}

/// Split an expression into lowercase identifier words (handling snake_case and camelCase),
/// ignoring `.min(` / `.max(` method calls such as `7.min(len)` or `len.max(1)`.
fn find_config_keyword(text: &str) -> Option<&'static str> {
    const CONFIG_KEYWORDS: &[&str] = &[
        "timeout",
        "limit",
        "max",
        "default",
        "threshold",
        "budget",
        "cap",
        "pool",
        "interval",
        "retries",
        "retry",
        "port",
        "concurrency",
        "workers",
        "quota",
        "ttl",
        "delay",
        "backoff",
    ];
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() {
            let start = i;
            let preceded_by_dot = start > 0 && bytes[start - 1] == b'.';
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let followed_by_paren = bytes.get(i) == Some(&b'(');
            let token = &text[start..i];
            // Skip method calls like `.min(len)` / `.max(1)` — slicing/clamping, not config names.
            if preceded_by_dot && followed_by_paren && matches!(token, "min" | "max") {
                continue;
            }
            // Split snake_case and camelCase into words.
            for part in token.split('_').filter(|p| !p.is_empty()) {
                let mut wstart = 0;
                let pbytes = part.as_bytes();
                for j in 1..=pbytes.len() {
                    let boundary = j == pbytes.len()
                        || (pbytes[j].is_ascii_uppercase()
                            && pbytes[j - 1].is_ascii_lowercase());
                    if boundary {
                        let word = part[wstart..j].to_ascii_lowercase();
                        if let Some(kw) = CONFIG_KEYWORDS.iter().find(|&&k| k == word) {
                            return Some(kw);
                        }
                        wstart = j;
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

fn is_const_or_static_decl(trimmed: &str) -> bool {
    let rest = if let Some(r) = trimmed.strip_prefix("pub") {
        let r = r.trim_start();
        if let Some(after_paren) = r.strip_prefix('(') {
            match after_paren.find(')') {
                Some(idx) => after_paren[idx + 1..].trim_start(),
                None => return false,
            }
        } else if r.len() < trimmed.len() - 3 {
            r
        } else {
            return false;
        }
    } else if let Some(r) = trimmed.strip_prefix("export ") {
        r.trim_start()
    } else {
        trimmed
    };
    rest.starts_with("const ") || rest.starts_with("static ")
}

fn call_arg_has_numeric_literal(code: &str, needle: &str) -> bool {
    let mut search_from = 0;
    while let Some(pos) = code[search_from..].find(needle) {
        let arg_start = search_from + pos + needle.len();
        search_from = arg_start;
        let bytes = code.as_bytes();
        let mut depth = 1usize;
        let mut i = arg_start;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            if depth > 0 {
                i += 1;
            }
        }
        let arg = &code[arg_start..i];
        if contains_numeric_literal(arg) {
            return true;
        }
    }
    false
}

fn config_constant_hit(path: &str, line: &str) -> Option<String> {
    if is_markdown_or_doc(path) || is_lockfile(path) || is_test_path(path) {
        return None;
    }
    let trimmed = line.trim();
    if trimmed.is_empty() || is_comment_line(trimmed) {
        return None;
    }
    let lower = path.to_ascii_lowercase();
    let is_rust = lower.ends_with(".rs");
    let is_config_file = lower.ends_with(".json")
        || lower.ends_with(".toml")
        || lower.ends_with(".yaml")
        || lower.ends_with(".yml")
        || lower.ends_with(".env");

    if is_config_file {
        let code = strip_string_literals(trimmed, false);
        if !contains_numeric_literal(&code) {
            return None;
        }
        if let Some(kw) = find_config_keyword(trimmed) {
            return Some(format!("配置数字项（{kw}）变化"));
        }
        return None;
    }

    let code = strip_string_literals(trimmed, is_rust);
    if !contains_numeric_literal(&code) {
        return None;
    }

    // `const` / `static` declarations: inspect the RHS value only so array type lengths like
    // `pub const ALERT_KINDS: [&str; 12] = [` are never treated as numeric constants.
    if is_const_or_static_decl(trimmed) {
        if let Some((_lhs, rhs)) = code.split_once('=') {
            if contains_numeric_literal(rhs) {
                return Some("数字常量定义变化（可能为硬上限/默认值）".into());
            }
        }
        return None;
    }

    // Module-level UPPER_SNAKE = <numeric> constants with config-like names (Python/JS/etc.).
    if let Some((lhs, rhs)) = code.split_once('=') {
        if !rhs.starts_with('=') && contains_numeric_literal(rhs) {
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
            if is_upper_const && find_config_keyword(ident).is_some() {
                return Some(format!("常量 {ident} 数值变化"));
            }
        }
    }

    // Timeout / default fallbacks: only when the call takes a numeric literal AND the line
    // references a config-like identifier/field name (timeout/limit/max/default/threshold/...).
    let has_timed_or_default_call = call_arg_has_numeric_literal(&code, "Duration::from_secs(")
        || call_arg_has_numeric_literal(&code, "Duration::from_millis(")
        || call_arg_has_numeric_literal(&code, "Duration::from_secs_f64(")
        || call_arg_has_numeric_literal(&code, "unwrap_or(");
    if has_timed_or_default_call && find_config_keyword(trimmed).is_some() {
        return Some("默认值/超时数字变化".into());
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
                if !is_lockfile_self_version_only(file) {
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
                }
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
                        .then(|| {
                            is_skip_or_ignore_marker(&file.path, &l.text)
                                .map(|m| (l.new_line.unwrap_or(1).max(1), m))
                        })
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

/// Estimate reading token budget using the same formula as `repo-governance/scripts/audit-context.py`.
pub fn estimate_tokens(text: &str) -> usize {
    let mut total_chars = 0usize;
    let mut cjk = 0usize;
    for ch in text.chars() {
        total_chars += 1;
        let cp = ch as u32;
        if (0x2E80..=0x9FFF).contains(&cp)
            || (0xAC00..=0xD7AF).contains(&cp)
            || (0xF900..=0xFAFF).contains(&cp)
            || (0xFF00..=0xFFEF).contains(&cp)
            || (0x3000..=0x303F).contains(&cp)
        {
            cjk += 1;
        }
    }
    cjk + (total_chars - cjk).div_ceil(4)
}

fn matches_path_list(patterns: &[String], path: &str) -> Option<bool> {
    let mut matched = false;
    let mut exact = false;
    for raw in patterns {
        if let Some(neg) = raw.strip_prefix('!') {
            if glob_match(neg, path) {
                return None;
            }
        } else if glob_match(raw, path) {
            matched = true;
            if !raw.contains(['*', '?', '[']) && !raw.ends_with('/') {
                exact = true;
            }
        }
    }
    matched.then_some(exact)
}

#[derive(Debug, Clone, Default)]
struct EffectiveSizeLimits {
    max_lines: Option<usize>,
    max_test_fn_lines: Option<usize>,
    max_tokens: Option<usize>,
    label: Option<String>,
}

fn resolve_size_limits(path: &str, rules: &[SizeBudgetRule]) -> EffectiveSizeLimits {
    let mut max_lines: Option<(bool, usize)> = None;
    let mut max_test_fn_lines: Option<(bool, usize)> = None;
    let mut max_tokens: Option<(bool, usize)> = None;
    let mut label: Option<(bool, String)> = None;

    let apply = |slot: &mut Option<(bool, usize)>, exact: bool, val: usize| match *slot {
        None => *slot = Some((exact, val)),
        Some((prev_exact, _)) => {
            if exact || !prev_exact {
                *slot = Some((exact, val));
            }
        }
    };

    for rule in rules {
        if let Some(exact) = matches_path_list(&rule.paths, path) {
            if let Some(v) = rule.max_lines {
                apply(&mut max_lines, exact, v);
            }
            if let Some(v) = rule.max_test_fn_lines {
                apply(&mut max_test_fn_lines, exact, v);
            }
            if let Some(v) = rule.max_tokens {
                apply(&mut max_tokens, exact, v);
            }
            if let Some(ref lbl) = rule.label {
                match &label {
                    None => label = Some((exact, lbl.clone())),
                    Some((prev_exact, _)) if exact || !*prev_exact => {
                        label = Some((exact, lbl.clone()));
                    }
                    _ => {}
                }
            }
        }
    }

    EffectiveSizeLimits {
        max_lines: max_lines.map(|(_, v)| v),
        max_test_fn_lines: max_test_fn_lines.map(|(_, v)| v),
        max_tokens: max_tokens.map(|(_, v)| v),
        label: label.map(|(_, v)| v),
    }
}

/// Load token budgets from `<repo>/.repo-governance.json` (if present).
pub fn load_repo_governance_size_budgets(repo_root: &Path) -> Vec<SizeBudgetRule> {
    let path = repo_root.join(".repo-governance.json");
    if !path.is_file() {
        return Vec::new();
    }
    let doc = json(&path);
    let Some(obj) = doc.as_object() else {
        return Vec::new();
    };
    let mut rules = vec![
        SizeBudgetRule {
            paths: vec!["AGENTS.md".into(), "CLAUDE.md".into()],
            max_lines: None,
            max_test_fn_lines: None,
            max_tokens: Some(5000),
            label: None,
        },
        SizeBudgetRule {
            paths: vec!["docs/current.md".into()],
            max_lines: None,
            max_test_fn_lines: None,
            max_tokens: Some(2500),
            label: None,
        },
    ];
    if let Some(budgets) = obj.get("budgets").and_then(Value::as_object) {
        let mut entries: Vec<(&String, usize)> = budgets
            .iter()
            .filter_map(|(pat, lim)| {
                let n = lim.as_u64().filter(|&v| v > 0)? as usize;
                (!pat.is_empty() && !pat.starts_with('/') && !pat.contains(".."))
                    .then_some((pat, n))
            })
            .collect();
        // Globs first, exact paths second so exact paths always take precedence.
        entries.sort_by_key(|(pat, _)| !pat.contains(['*', '?', '[']));
        for (pat, limit) in entries {
            rules.push(SizeBudgetRule {
                paths: vec![pat.clone()],
                max_lines: None,
                max_test_fn_lines: None,
                max_tokens: Some(limit),
                label: None,
            });
        }
    }
    rules
}

fn brace_delta(line: &str, preserve_single_quotes: bool) -> (usize, usize) {
    let stripped = strip_string_literals(line, preserve_single_quotes);
    let code = stripped.split("//").next().unwrap_or(&stripped);
    let opens = code.bytes().filter(|&b| b == b'{').count();
    let closes = code.bytes().filter(|&b| b == b'}').count();
    (opens, closes)
}

fn scan_brace_block_end(lines: &[&str], start_idx: usize, preserve_single_quotes: bool) -> usize {
    let mut depth: isize = 0;
    let mut opened = false;
    for (offset, line) in lines[start_idx..].iter().enumerate() {
        let (opens, closes) = brace_delta(line, preserve_single_quotes);
        if opens > 0 {
            opened = true;
        }
        depth += opens as isize - closes as isize;
        if opened && depth <= 0 {
            return start_idx + offset;
        }
    }
    lines.len().saturating_sub(1)
}

fn extract_quoted_arg(text: &str) -> Option<String> {
    let open = text.find('(')?;
    let rest = text[open + 1..].trim_start();
    let quote = rest.chars().next().filter(|&c| c == '"' || c == '\'' || c == '`')?;
    let end = rest[1..].find(quote)?;
    let name = rest[1..1 + end].trim();
    (!name.is_empty()).then(|| clip(name, 50))
}

/// Extract `(test_fn_name, start_line_1based, line_count)` spans for test functions in a source file.
pub fn extract_test_fn_spans(path: &str, text: &str) -> Vec<(String, usize, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let lower = path.to_ascii_lowercase();
    let mut spans = Vec::new();

    if lower.ends_with(".rs") {
        let mut i = 0;
        while i < lines.len() {
            let trimmed = lines[i].trim();
            if is_rust_test_attr(trimmed) || trimmed == "#[rstest]" || trimmed.starts_with("#[rstest(") {
                let mut fn_idx = None;
                for (k, line) in lines.iter().enumerate().skip(i + 1).take(8) {
                    if extract_rust_fn_name(line).is_some() {
                        fn_idx = Some(k);
                        break;
                    }
                    let t = line.trim();
                    if !t.is_empty() && !t.starts_with("#[") && !t.starts_with("//") {
                        break;
                    }
                }
                if let Some(start) = fn_idx {
                    let name = extract_rust_fn_name(lines[start]).unwrap_or_else(|| format!("test@L{}", start + 1));
                    let end = scan_brace_block_end(&lines, start, true);
                    spans.push((name, start + 1, end - start + 1));
                    i = end + 1;
                    continue;
                }
            }
            i += 1;
        }
    } else if lower.ends_with(".py") {
        let mut i = 0;
        while i < lines.len() {
            let raw = lines[i];
            let trimmed = raw.trim_start();
            let indent = raw.len() - trimmed.len();
            let def_name = trimmed
                .strip_prefix("def test_")
                .or_else(|| trimmed.strip_prefix("async def test_"))
                .and_then(|rest| {
                    let n: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    (!n.is_empty()).then(|| format!("test_{n}"))
                });
            if let Some(name) = def_name {
                let mut end = i;
                for (k, line) in lines.iter().enumerate().skip(i + 1) {
                    let t = line.trim_start();
                    if t.is_empty() || t.starts_with('#') {
                        continue;
                    }
                    let cur_indent = line.len() - t.len();
                    if cur_indent <= indent {
                        break;
                    }
                    end = k;
                }
                spans.push((name, i + 1, end - i + 1));
                i = end + 1;
                continue;
            }
            i += 1;
        }
    } else if lower.ends_with(".go") {
        let mut i = 0;
        while i < lines.len() {
            let trimmed = lines[i].trim();
            if let Some(rest) = trimmed.strip_prefix("func Test") {
                let name_tail: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !name_tail.is_empty() {
                    let end = scan_brace_block_end(&lines, i, false);
                    spans.push((format!("Test{name_tail}"), i + 1, end - i + 1));
                    i = end + 1;
                    continue;
                }
            }
            i += 1;
        }
    } else if lower.ends_with(".js")
        || lower.ends_with(".jsx")
        || lower.ends_with(".ts")
        || lower.ends_with(".tsx")
        || lower.ends_with(".mjs")
        || lower.ends_with(".cjs")
        || lower.ends_with(".mts")
        || lower.ends_with(".cts")
    {
        let mut i = 0;
        while i < lines.len() {
            let trimmed = lines[i].trim();
            if (trimmed.starts_with("it(")
                || trimmed.starts_with("test(")
                || trimmed.starts_with("Deno.test("))
                && trimmed.contains('{')
            {
                let name = extract_quoted_arg(trimmed).unwrap_or_else(|| format!("test@L{}", i + 1));
                let end = scan_brace_block_end(&lines, i, false);
                spans.push((name, i + 1, end - i + 1));
                i = end + 1;
                continue;
            }
            i += 1;
        }
    }

    spans
}

fn reconstruct_new_file_text(file: &DiffFile) -> String {
    let mut out = String::new();
    for hunk in &file.hunks {
        for l in &hunk.lines {
            if l.kind == DiffLineKind::Add {
                out.push_str(&l.text);
                out.push('\n');
            }
        }
    }
    out
}

/// Detect size-budget threshold crossings or further growth on touched files.
/// Only files touched in `files` are checked; already-over files that are untouched or did not grow
/// are never flagged.
pub fn detect_size_budget_risks(
    files: &[DiffFile],
    risk_cfg: &RiskConfig,
    snapshots: &BTreeMap<String, (Option<String>, Option<String>)>,
) -> Vec<RiskItem> {
    if risk_cfg.disabled.contains("size_budget") || risk_cfg.size_budgets.is_empty() {
        return Vec::new();
    }
    let mut items = Vec::new();

    for file in files {
        if file.is_deleted || file.is_binary {
            continue;
        }
        let limits = resolve_size_limits(&file.path, &risk_cfg.size_budgets);
        if limits.max_lines.is_none()
            && limits.max_test_fn_lines.is_none()
            && limits.max_tokens.is_none()
        {
            continue;
        }

        let (before_owned, after_owned): (String, String) = match snapshots.get(&file.path) {
            Some((b_opt, Some(a))) => (b_opt.clone().unwrap_or_default(), a.clone()),
            _ if file.is_new => (String::new(), reconstruct_new_file_text(file)),
            _ => continue,
        };

        let first_line = file
            .hunks
            .first()
            .map(DiffHunk::first_changed_line)
            .unwrap_or(1);
        let first_hunk = (!file.hunks.is_empty()).then_some(0);
        let label_prefix = limits
            .label
            .as_deref()
            .map(|l| format!("{l}："))
            .unwrap_or_default();

        if let Some(limit) = limits.max_lines {
            let before_lines = before_owned.lines().count();
            let after_lines = after_owned.lines().count();
            if after_lines > limit {
                if before_lines <= limit {
                    let reason = if file.is_new || before_lines == 0 {
                        format!("{label_prefix}新增文件行数超过阈值（{after_lines} 行，阈值 {limit}）")
                    } else {
                        format!("{label_prefix}本改动使文件行数跨过阈值（{before_lines}→{after_lines} 行，阈值 {limit}）")
                    };
                    items.push(RiskItem {
                        severity: Severity::Medium,
                        file: file.path.clone(),
                        line: first_line,
                        rule_id: "size_budget".into(),
                        category: "体量".into(),
                        source: "规则".into(),
                        reason,
                        hunk_index: first_hunk,
                        file_level: false,
                        verdict_file: None,
                    });
                } else if after_lines > before_lines {
                    items.push(RiskItem {
                        severity: Severity::Low,
                        file: file.path.clone(),
                        line: first_line,
                        rule_id: "size_budget".into(),
                        category: "体量".into(),
                        source: "规则".into(),
                        reason: format!(
                            "{label_prefix}文件已超行数阈值且本改动继续增长（{before_lines}→{after_lines} 行，阈值 {limit}）"
                        ),
                        hunk_index: first_hunk,
                        file_level: false,
                        verdict_file: None,
                    });
                }
            }
        }

        if let Some(limit) = limits.max_tokens {
            let before_toks = estimate_tokens(&before_owned);
            let after_toks = estimate_tokens(&after_owned);
            if after_toks > limit {
                if before_toks <= limit {
                    let reason = if file.is_new || before_toks == 0 {
                        format!("{label_prefix}新增文件估算 token 超过预算（~{after_toks} tokens，阈值 {limit}）")
                    } else {
                        format!("{label_prefix}本改动使估算 token 跨过预算（~{before_toks}→~{after_toks} tokens，阈值 {limit}）")
                    };
                    items.push(RiskItem {
                        severity: Severity::Medium,
                        file: file.path.clone(),
                        line: first_line,
                        rule_id: "size_budget".into(),
                        category: "体量".into(),
                        source: "规则".into(),
                        reason,
                        hunk_index: first_hunk,
                        file_level: false,
                        verdict_file: None,
                    });
                } else if after_toks > before_toks {
                    items.push(RiskItem {
                        severity: Severity::Low,
                        file: file.path.clone(),
                        line: first_line,
                        rule_id: "size_budget".into(),
                        category: "体量".into(),
                        source: "规则".into(),
                        reason: format!(
                            "{label_prefix}估算 token 已超预算且本改动继续增长（~{before_toks}→~{after_toks} tokens，阈值 {limit}）"
                        ),
                        hunk_index: first_hunk,
                        file_level: false,
                        verdict_file: None,
                    });
                }
            }
        }

        if let Some(limit) = limits.max_test_fn_lines {
            let before_spans = extract_test_fn_spans(&file.path, &before_owned);
            let after_spans = extract_test_fn_spans(&file.path, &after_owned);
            let mut before_map: BTreeMap<String, usize> = BTreeMap::new();
            for (name, _, len) in before_spans {
                before_map
                    .entry(name)
                    .and_modify(|cur| *cur = (*cur).max(len))
                    .or_insert(len);
            }
            for (name, start_line, after_len) in after_spans {
                if after_len <= limit {
                    continue;
                }
                let end_line = start_line + after_len.saturating_sub(1);
                let hunk_idx = file
                    .hunks
                    .iter()
                    .position(|h| {
                        let hs = h.new_start.max(1);
                        let he = hs + h.new_count.max(1);
                        hs <= end_line && he >= start_line
                    })
                    .or(first_hunk);
                match before_map.get(&name).copied() {
                    None => {
                        items.push(RiskItem {
                            severity: Severity::Medium,
                            file: file.path.clone(),
                            line: start_line,
                            rule_id: "size_budget".into(),
                            category: "体量".into(),
                            source: "规则".into(),
                            reason: format!(
                                "新增测试函数 {name} 超过行数阈值（{after_len} 行，阈值 {limit}）"
                            ),
                            hunk_index: hunk_idx,
                            file_level: false,
                            verdict_file: None,
                        });
                    }
                    Some(b_len) if b_len <= limit => {
                        items.push(RiskItem {
                            severity: Severity::Medium,
                            file: file.path.clone(),
                            line: start_line,
                            rule_id: "size_budget".into(),
                            category: "体量".into(),
                            source: "规则".into(),
                            reason: format!(
                                "本改动使测试函数 {name} 跨过行数阈值（{b_len}→{after_len} 行，阈值 {limit}）"
                            ),
                            hunk_index: hunk_idx,
                            file_level: false,
                            verdict_file: None,
                        });
                    }
                    Some(b_len) if after_len > b_len => {
                        items.push(RiskItem {
                            severity: Severity::Low,
                            file: file.path.clone(),
                            line: start_line,
                            rule_id: "size_budget".into(),
                            category: "体量".into(),
                            source: "规则".into(),
                            reason: format!(
                                "测试函数 {name} 已超行数阈值且本改动继续增长（{b_len}→{after_len} 行，阈值 {limit}）"
                            ),
                            hunk_index: hunk_idx,
                            file_level: false,
                            verdict_file: None,
                        });
                    }
                    Some(_) => {}
                }
            }
        }
    }

    items
}

fn git_show_file(top: &Path, tree_ref: &str, path: &str) -> Option<String> {
    if tree_ref.is_empty() || path.is_empty() {
        return None;
    }
    let spec = format!("{tree_ref}:{path}");
    let bytes = git(top, &["show", &spec]).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn collect_size_snapshots(
    files: &[DiffFile],
    risk_cfg: &RiskConfig,
    tree_refs: Option<&(PathBuf, String, String)>,
) -> BTreeMap<String, (Option<String>, Option<String>)> {
    let mut out = BTreeMap::new();
    if risk_cfg.disabled.contains("size_budget") || risk_cfg.size_budgets.is_empty() {
        return out;
    }
    let Some((top, before_ref, after_ref)) = tree_refs else {
        return out;
    };
    for file in files {
        if file.is_deleted || file.is_binary {
            continue;
        }
        let limits = resolve_size_limits(&file.path, &risk_cfg.size_budgets);
        if limits.max_lines.is_none()
            && limits.max_test_fn_lines.is_none()
            && limits.max_tokens.is_none()
        {
            continue;
        }
        let before = if file.is_new {
            Some(String::new())
        } else {
            git_show_file(top, before_ref, &file.path)
        };
        let after = git_show_file(top, after_ref, &file.path);
        out.insert(file.path.clone(), (before, after));
    }
    out
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

/// Extract up to 3 summary lines from `result.md` without modifying or truncating the original file,
/// alongside deterministic pointers to every preserved round result (`round-N-result.md` and/or
/// reply-chain ancestors).
pub fn extract_author_summary_with_rounds(run: &Path, include_chain: bool) -> Value {
    let rounds = runs::collect_result_rounds(run, include_chain);
    let rounds_json: Vec<Value> = rounds.iter().map(runs::ResultRound::to_json).collect();
    let path = run.join("result.md");
    let display_path = if path.is_file() {
        Some(path)
    } else {
        rounds.last().map(|r| r.path.clone())
    };
    let Some(path) = display_path else {
        return json!({
            "lines": [],
            "path": Value::Null,
            "totalLines": 0,
            "totalChars": 0,
            "rounds": rounds_json,
        });
    };
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
        "rounds": rounds_json,
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

struct ResolvedDiff {
    diff_text: String,
    tree_refs: Option<(PathBuf, String, String)>,
}

fn resolve_diff_and_trees(
    run: &Path,
    meta: &Value,
    round_only: bool,
    context_lines: usize,
) -> Res<ResolvedDiff> {
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
            return Ok(ResolvedDiff {
                diff_text: read(&p),
                tree_refs: None,
            });
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
                    return Ok(ResolvedDiff {
                        diff_text: read(&p),
                        tree_refs: None,
                    });
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
            Ok(ResolvedDiff {
                diff_text: String::from_utf8_lossy(&bytes).into_owned(),
                tree_refs: Some((top, before, after)),
            })
        }
        Err(err) => {
            if let Some(p) = archived_patch(run) {
                return Ok(ResolvedDiff {
                    diff_text: read(&p),
                    tree_refs: None,
                });
            }
            // If no changes.json (e.g. read-only run or zero changes), return empty diff instead of erroring.
            if !run.join("changes.json").is_file() && s(meta, "mode") == "read-only" {
                return Ok(ResolvedDiff {
                    diff_text: String::new(),
                    tree_refs: None,
                });
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

    let mut risk_cfg = if !meta["riskRules"].is_null() {
        parse_risk_config(&meta["riskRules"])
    } else if let Some(cfg) = &repo_cfg {
        parse_risk_config(&cfg["riskRules"])
    } else {
        RiskConfig::default()
    };
    if !top_str.is_empty() {
        let mut combined = load_repo_governance_size_budgets(Path::new(top_str));
        combined.extend(risk_cfg.size_budgets);
        risk_cfg.size_budgets = combined;
    }

    (controller_paths, protect_rules, allowed_rules, risk_cfg)
}

fn load_reviewer_info(run: &Path, review_doc: &Value) -> (Option<String>, Vec<ReviewerFocusItem>, Option<String>) {
    let raw_vf = s(review_doc, "verdictFile");
    let round_num = n(review_doc, "round");
    let round_vf = (round_num > 0).then(|| run.join("review").join(format!("round-{round_num}-verdict.md")));
    let verdict_file = if !raw_vf.is_empty() && Path::new(raw_vf).is_file() {
        Some(raw_vf.to_string())
    } else if let Some(ref rvf) = round_vf.filter(|p| p.is_file()) {
        Some(rvf.to_string_lossy().into_owned())
    } else if run.join("review.md").is_file() {
        Some(run.join("review.md").to_string_lossy().into_owned())
    } else if !raw_vf.is_empty() {
        Some(raw_vf.to_string())
    } else {
        None
    };
    let Some(vf) = verdict_file else {
        return (None, Vec::new(), None);
    };
    let raw = if Path::new(&vf).is_file() { read(&vf) } else { String::new() };
    let conclusion = {
        let c = s(review_doc, "conclusion");
        if !c.is_empty() {
            Some(c.to_string())
        } else if !raw.trim().is_empty() {
            extract_reviewer_conclusion(&raw)
        } else {
            None
        }
    };
    let focus = if raw.trim().is_empty() {
        Vec::new()
    } else {
        parse_reviewer_focus(&raw)
    };
    (conclusion, focus, Some(vf))
}

fn resolve_base_info(run: &Path, meta: &Value) -> Value {
    let cur_base = {
        let nb = s(meta, "netBase");
        if !nb.is_empty() {
            nb.to_string()
        } else {
            s(&meta["worktree"], "base").to_string()
        }
    };
    if cur_base.is_empty() {
        return Value::Null;
    }
    let short = cur_base[..cur_base.len().min(7)].to_string();
    let parent_id = s(meta, "parent");
    let prev_base = if !parent_id.is_empty() {
        let sibling_meta = run
            .parent()
            .map(|p| p.join(parent_id).join("meta.json"))
            .filter(|p| p.is_file())
            .map(json);
        let parent_meta = sibling_meta.or_else(|| {
            crate::runs::resolve(parent_id)
                .ok()
                .map(|p| json(p.join("meta.json")))
                .filter(|v| v.is_object())
        });
        let from_parent = parent_meta
            .as_ref()
            .map(|pm| {
                let nb = s(pm, "netBase");
                if !nb.is_empty() {
                    nb.to_string()
                } else {
                    s(&pm["worktree"], "base").to_string()
                }
            })
            .unwrap_or_default();
        if !from_parent.is_empty() {
            from_parent
        } else {
            s(&meta["syncedBase"], "from").to_string()
        }
    } else if b(meta, "netBaseRebased") {
        s(&meta["worktree"], "base").to_string()
    } else {
        String::new()
    };

    if !prev_base.is_empty() && prev_base != cur_base {
        let prev_short = prev_base[..prev_base.len().min(7)].to_string();
        json!({
            "sha": cur_base,
            "short": short,
            "rebased": true,
            "previousSha": prev_base,
            "previousShort": prev_short,
        })
    } else {
        json!({
            "sha": cur_base,
            "short": short,
            "rebased": false,
        })
    }
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

    let resolved = resolve_diff_and_trees(run, meta, round_only, 3).unwrap_or(ResolvedDiff {
        diff_text: String::new(),
        tree_refs: None,
    });
    let parsed_files = parse_unified_diff(&resolved.diff_text);
    let total_files = parsed_files.len();
    let total_added: usize = parsed_files.iter().map(|f| f.added).sum();
    let total_deleted: usize = parsed_files.iter().map(|f| f.deleted).sum();

    let (controller_paths, protect_rules, allowed_rules, risk_cfg) = load_effective_configs(meta);
    let mut rule_risks = detect_rule_risks(
        &parsed_files,
        &controller_paths,
        &protect_rules,
        &allowed_rules,
        &risk_cfg,
    );
    let size_snapshots =
        collect_size_snapshots(&parsed_files, &risk_cfg, resolved.tree_refs.as_ref());
    rule_risks.extend(detect_size_budget_risks(
        &parsed_files,
        &risk_cfg,
        &size_snapshots,
    ));

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
    let author_summary = extract_author_summary_with_rounds(run, !round_only);
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
        let mut sec = json!({
            "enabled": true,
            "status": review_doc["status"],
            "verdict": review_doc["verdict"],
            "originalVerdict": review_doc["originalVerdict"],
            "round": review_doc["round"],
            "reworkRounds": review_doc["reworkRounds"],
            "model": review_doc["model"],
            "modelName": review_doc["modelName"],
            "thinking": review_doc["thinking"],
            "upgradedBy": review_doc["upgradedBy"],
            "cancelledBy": review_doc["cancelledBy"],
            "conclusion": rev_conclusion,
            "verdictFile": rev_verdict_file,
        });
        for key in [
            "scope",
            "scopeFiles",
            "syncSkip",
            "fallback",
            "envError",
            "error",
            "retryCommand",
            "formatError",
            "formatRetried",
            "formatRetryFailed",
            "parseError",
            "extra",
        ] {
            if let Some(v) = review_doc.get(key) {
                if !v.is_null() {
                    sec[key] = v.clone();
                }
            }
        }
        sec
    } else {
        json!({"enabled": false})
    };

    let mut accept_val = if sum["accept"].is_object() {
        sum["accept"].clone()
    } else {
        Value::Null
    };
    if accept_val.is_object()
        && accept_val["ok"] == json!(false)
        && !b(&accept_val, "cancelled")
        && !b(&accept_val, "notExecuted")
        && !b(&accept_val, "infrastructureFailure")
        && !accept_val["failureAttribution"].is_object()
    {
        let mut changed_paths: Vec<String> = parsed_files.iter().map(|f| f.path.clone()).collect();
        if changed_paths.is_empty() {
            changed_paths = json(run.join("changes.json"))["changes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| s(c, "path").to_string())
                .filter(|p| !p.is_empty())
                .collect();
        }
        if let Some(attr) = crate::accept::analyze_failure_attribution(run, meta, &changed_paths) {
            accept_val["failureAttribution"] = attr;
        }
    }

    let base_info = resolve_base_info(run, meta);

    let mut next_cmds = vec![
        json!({"label": "仅看风险改动", "command": format!("delegate diff {run_id} --risky")}),
        json!({"label": "查看完整改动", "command": format!("delegate diff {run_id}{}", if meta["worktree"].is_object() { " --total" } else { "" })}),
    ];
    if accept_val["failureAttribution"].is_object()
        && b(&accept_val["failureAttribution"], "suspectedMainline")
    {
        let attr = &accept_val["failureAttribution"];
        let log_file = s(attr, "logFile");
        if let Some(first) = attr["outsideRunFiles"].as_array().and_then(|a| a.first()) {
            let line = n(first, "logLine").max(1);
            let start = line.saturating_sub(5).max(1);
            let end = line + 15;
            if !log_file.is_empty() {
                next_cmds.push(json!({
                    "label": "查看验收报错上下文",
                    "command": format!("sed -n '{start},{end}p' {}", shell_quote(log_file)),
                }));
            }
        }
        let failed_cmd = accept_val["layers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|l| s(l, "status") == "failed")
            .map(|l| s(l, "command"))
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| s(&accept_val, "command"));
        if !failed_cmd.is_empty() {
            next_cmds.push(json!({
                "label": "在主干复现验收确认是否主干已有失败",
                "command": format!("delegate lane {}", shell_quote(failed_cmd)),
            }));
        }
    }
    let is_review_env_error = review_doc.is_object()
        && (s(&review_doc, "verdict") == crate::review::REVIEW_VERDICT_ENV_ERROR
            || s(&review_doc, "status") == "env_error"
            || b(&review_doc, "envError"));
    if is_review_env_error {
        let retry_cmd = {
            let c = s(&review_doc, "retryCommand");
            if !c.is_empty() {
                c.to_string()
            } else {
                crate::review::review_env_error_retry_command(&run_id)
            }
        };
        next_cmds.push(json!({"label": "重试审查（审查环境错误）", "command": retry_cmd}));
    }
    if needs_sign {
        next_cmds.push(json!({"label": "受控路径签字", "command": format!("delegate sign {run_id}")}));
    }
    next_cmds.push(json!({"label": "确认无误合入", "command": format!("delegate apply {run_id}")}));

    let rule_count = risks.iter().filter(|r| r.source.contains("规则")).count();
    let rev_count = risks.iter().filter(|r| r.source.contains("审查员")).count();

    let mut card_doc = json!({
        "run": run_id,
        "name": name,
        "state": state,
        "policyRefused": b(sum, "policyRefused"),
        "base": base_info,
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
    });
    if sum["tokens"].as_object().is_some_and(|o| !o.is_empty()) {
        card_doc["tokens"] = sum["tokens"].clone();
    }
    if sum["tokensDelta"].as_object().is_some_and(|o| !o.is_empty()) {
        card_doc["tokensDelta"] = sum["tokensDelta"].clone();
    }
    if let Some(steps) = sum["preEditSteps"].as_u64() {
        card_doc["preEditSteps"] = json!(steps);
    }
    let scout_src = if sum["scout"].is_object() {
        &sum["scout"]
    } else if meta["scout"].is_object() {
        &meta["scout"]
    } else {
        &Value::Null
    };
    let scout_run_id = {
        let a = s(sum, "scoutRun");
        if !a.is_empty() {
            a
        } else {
            let b_id = s(scout_src, "run");
            if !b_id.is_empty() {
                b_id
            } else {
                s(meta, "scoutRun")
            }
        }
    };
    if scout_src.is_object() || !scout_run_id.is_empty() {
        let map_status = {
            let v = s(sum, "scoutMapStatus");
            if !v.is_empty() {
                v
            } else {
                s(scout_src, "status")
            }
        };
        let drop_reason = {
            let v = s(sum, "scoutMapDropReason");
            if !v.is_empty() {
                v
            } else {
                s(scout_src, "dropReason")
            }
        };
        let bad_anchors = sum["scoutBadAnchors"]
            .as_u64()
            .or_else(|| scout_src["badAnchors"].as_u64())
            .unwrap_or(0);
        let total_anchors = sum["scoutAnchorsTotal"]
            .as_u64()
            .or_else(|| scout_src["anchorsTotal"].as_u64())
            .unwrap_or(0);
        let scout_tokens = if sum["scoutTokens"].as_object().is_some_and(|o| !o.is_empty()) {
            sum["scoutTokens"].clone()
        } else if scout_src["tokens"].as_object().is_some_and(|o| !o.is_empty()) {
            scout_src["tokens"].clone()
        } else {
            Value::Null
        };
        let mut scout_card = json!({
            "run": scout_run_id,
            "mapStatus": map_status,
            "badAnchors": bad_anchors,
            "anchorsTotal": total_anchors,
        });
        if scout_tokens.is_object() {
            scout_card["tokens"] = scout_tokens;
        }
        if !drop_reason.is_empty() {
            scout_card["dropReason"] = json!(drop_reason);
        }
        if map_status == "dropped" {
            scout_card["dropped"] = json!(true);
            scout_card["diagnostic"] = json!("scout_map_dropped");
        }
        let map_path = s(scout_src, "mapPath");
        if !map_path.is_empty() {
            scout_card["mapPath"] = json!(map_path);
        }
        card_doc["scout"] = scout_card;
        if !scout_run_id.is_empty() {
            card_doc["scoutRun"] = json!(scout_run_id);
        }
    }
    if let Some(appends) = crate::pending_reply::summary_json(run) {
        card_doc["appends"] = appends;
    }
    for key in ["agyAccountSwitch", "reviewAgyAccountSwitch"] {
        if meta[key].is_object() {
            card_doc[key] = meta[key].clone();
        } else if sum[key].is_object() {
            card_doc[key] = sum[key].clone();
        }
    }
    Ok(card_doc)
}

/// Deterministically evaluate built-in and repo-configured risk rules against a unified diff.
/// Used both for card rendering and for pre-flight `review.extra` trigger checks (`risk:high`).
pub fn evaluate_diff_rule_risks(meta: &Value, diff_text: &str) -> Vec<RiskItem> {
    let parsed_files = parse_unified_diff(diff_text);
    let (controller_paths, protect_rules, allowed_rules, risk_cfg) = load_effective_configs(meta);
    detect_rule_risks(
        &parsed_files,
        &controller_paths,
        &protect_rules,
        &allowed_rules,
        &risk_cfg,
    )
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
    let state_zh = if b(card, "policyRefused") {
        "结论被模型服务拒绝"
    } else {
        match state {
            "delivered" => "已交付",
            "answered" => "已答复",
            "blocked_mainline" => "主干阻塞",
            "rejected" => "未通过",
            "running" => "运行中",
            "scouting" => "探路中",
            "stopped" => "已停止",
            "failed" => "失败",
            _ => state,
        }
    };
    let files = n(&card["changes"], "files");
    let added = n(&card["changes"], "added");
    let deleted = n(&card["changes"], "deleted");
    let ff = format_l0_fast_forward(&card["l0"], card.get("premerge"));
    let base_seg = if card["base"].is_object() {
        let short = s(&card["base"], "short");
        let prev = s(&card["base"], "previousShort");
        if !short.is_empty() && b(&card["base"], "rebased") && !prev.is_empty() && prev != short {
            format!(" · 基线 {short}（已 rebase：{prev}→{short}）")
        } else if !short.is_empty() {
            format!(" · 基线 {short}")
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let mut out = Vec::new();
    out.push(format!(
        "审查卡 · {name} ({run_id}) · {state_zh}{base_seg} · 改 {files} 个文件 +{added}/-{deleted} · {ff}"
    ));

    let accept_line = if card["accept"].is_object() {
        let mut lbl = human_accept_label(&card["accept"], false);
        if b(&card["accept"]["failureAttribution"], "suspectedMainline") {
            lbl.push_str("（报错文件不在本 run 改动中，疑似主干已有失败）");
        }
        lbl
    } else {
        "未设验收".into()
    };
    let rev = &card["review"];
    let rev_model_label = {
        let name = s(rev, "modelName");
        if !name.is_empty() {
            name.to_string()
        } else {
            s(rev, "model").to_string()
        }
    };
    let rev_extra = &rev["extra"];
    let extra_disagreed = rev_extra.is_object() && b(rev_extra, "disagreed");
    let format_retry_failed =
        b(rev, "formatRetryFailed") || (b(rev, "formatError") && b(rev, "formatRetried"));
    let sync_skip = b(rev, "syncSkip") || s(rev, "scope") == "sync_skip";
    let review_env_error = s(rev, "verdict") == crate::review::REVIEW_VERDICT_ENV_ERROR
        || s(rev, "status") == "env_error"
        || b(rev, "envError");
    let review_head = if b(rev, "enabled") {
        if sync_skip {
            "同步轮，沿用上一轮审查 PASS".to_string()
        } else if review_env_error {
            if !rev_model_label.is_empty() {
                format!("审查环境错误 · {rev_model_label}")
            } else {
                "审查环境错误".to_string()
            }
        } else {
            let v = s(rev, "verdict");
            let orig = s(rev, "originalVerdict");
            let st = s(rev, "status");
            let base = if st == "cancelled" {
                format!("已取消（{}）", s(rev, "cancelledBy"))
            } else if !v.is_empty() {
                let up = s(rev, "upgradedBy");
                let has_orig = !orig.is_empty() && orig != v;
                let mut notes = Vec::new();
                if has_orig {
                    notes.push(format!("原判 {orig}"));
                }
                if !up.is_empty() {
                    if has_orig {
                        notes.push(format!("经 {up} 升级"));
                    } else {
                        notes.push(up.to_string());
                    }
                }
                if format_retry_failed {
                    notes.push("补问 1 次仍无结论".into());
                }
                if extra_disagreed {
                    notes.push("意见不一致".into());
                }
                if notes.is_empty() {
                    v.to_string()
                } else {
                    format!("{v}（{}）", notes.join("，"))
                }
            } else {
                st.to_string()
            };
            if !rev_model_label.is_empty() {
                format!("{base} · {rev_model_label}")
            } else {
                base
            }
        }
    } else {
        "未启用".into()
    };
    let has_delta = card["tokensDelta"]
        .as_object()
        .is_some_and(|o| !o.is_empty());
    let tok_str = runs::effective_tokens_delta(card)
        .map(|t| runs::format_tokens_usage(&t))
        .unwrap_or_default();
    if tok_str.is_empty() {
        out.push(format!("验收：{accept_line} · 审查：{review_head}"));
    } else if has_delta {
        out.push(format!(
            "验收：{accept_line} · 审查：{review_head} · 用量 {tok_str}（本轮增量）"
        ));
    } else {
        out.push(format!(
            "验收：{accept_line} · 审查：{review_head} · 用量 {tok_str}"
        ));
    }
    if rev["fallback"].is_object() {
        let fb = &rev["fallback"];
        let to = {
            let n = s(fb, "to");
            if !n.is_empty() { n } else { s(fb, "toModel") }
        };
        let reason = s(fb, "reason");
        if !to.is_empty() {
            if reason.is_empty() {
                out.push(format!("审查改用 {to}"));
            } else {
                out.push(format!("审查改用 {to}（{reason}）"));
            }
        }
    }
    if let Some(line) =
        crate::agy_accounts::format_account_switch_line(&card["agyAccountSwitch"], false)
    {
        out.push(line);
    }
    if let Some(line) =
        crate::agy_accounts::format_account_switch_line(&card["reviewAgyAccountSwitch"], true)
    {
        out.push(line);
    }
    if card["scout"].is_object() {
        let sc = &card["scout"];
        let srun = s(sc, "run");
        let mstatus = s(sc, "mapStatus");
        let bad = n(sc, "badAnchors");
        let total = n(sc, "anchorsTotal");
        let status_label = if mstatus == "dropped" {
            let reason = s(sc, "dropReason");
            if reason.is_empty() {
                "地图弃图（scout_map_dropped）".to_string()
            } else {
                format!("地图弃图（scout_map_dropped：{reason}）")
            }
        } else {
            "地图采用".to_string()
        };
        let mut segs = Vec::new();
        if !srun.is_empty() {
            segs.push(srun.to_string());
        }
        segs.push(status_label);
        if total > 0 || bad > 0 {
            segs.push(format!("坏锚点 {bad}/{total}"));
        }
        if sc["tokens"].as_object().is_some_and(|o| !o.is_empty()) {
            let st = runs::format_tokens_usage(&sc["tokens"]);
            if !st.is_empty() {
                segs.push(format!("探路用量 {st}"));
            }
        }
        if let Some(steps) = card["preEditSteps"].as_u64() {
            segs.push(format!("首改前 {steps} 步"));
        }
        out.push(format!("探路：{}", segs.join(" · ")));
    } else if let Some(steps) = card["preEditSteps"].as_u64() {
        out.push(format!("首改前工具步数：{steps}"));
    }
    if rev_extra.is_object() && s(rev_extra, "status") == "skipped" {
        let skip_reason = s(rev_extra, "skipReason");
        if !skip_reason.is_empty() {
            out.push(skip_reason.to_string());
        }
    }
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
    if card["appends"].is_object() && n(&card["appends"], "total") > 0 {
        out.push(format!(
            "追加：已追加 {} 条，待投递 {} 条",
            n(&card["appends"], "total"),
            n(&card["appends"], "pending")
        ));
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
    let author_rounds = card["author"]["rounds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !result_path.is_empty() {
        if author_rounds.len() > 1 {
            out.push(format!(
                "   原文：{result_path}（共 {} 行 / {} 字；共 {} 轮，查看：delegate result {run_id} --round <1..={}> / --all）",
                n(&card["author"], "totalLines"),
                n(&card["author"], "totalChars"),
                author_rounds.len(),
                author_rounds.len()
            ));
        } else {
            out.push(format!(
                "   原文：{result_path}（共 {} 行 / {} 字）",
                n(&card["author"], "totalLines"),
                n(&card["author"], "totalChars")
            ));
        }
    }
    if author_rounds.len() > 1 {
        let multi_run = author_rounds
            .iter()
            .map(|r| s(r, "run"))
            .collect::<BTreeSet<_>>()
            .len()
            > 1;
        out.push("   各轮原文：".into());
        for r in &author_rounds {
            let rn = n(r, "round");
            let rpath = s(r, "path");
            let rlines = n(r, "totalLines");
            let rchars = n(r, "totalChars");
            let kind_zh = match s(r, "kind") {
                "rework" => "返工",
                "append" => "追加",
                "reply" => "reply",
                _ => "初稿",
            };
            let is_cur = b(r, "current");
            let mut tags = Vec::new();
            if multi_run {
                let rid = s(r, "run");
                if !rid.is_empty() {
                    tags.push(rid.to_string());
                }
            }
            tags.push(kind_zh.to_string());
            let tag_str = if is_cur {
                format!("{}，当前", tags.join(" · "))
            } else {
                tags.join(" · ")
            };
            out.push(format!(
                "     - 第 {rn} 轮（{tag_str}）：{rpath}（共 {rlines} 行 / {rchars} 字）"
            ));
        }
    }
    if b(rev, "enabled") {
        let verdict = s(rev, "verdict");
        let orig = s(rev, "originalVerdict");
        let up = s(rev, "upgradedBy");
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
        let v_prefix = if verdict.is_empty() {
            "—".to_string()
        } else if !orig.is_empty() && orig != verdict {
            if up.is_empty() {
                format!("{verdict}（原判 {orig}）")
            } else {
                format!("{verdict}（原判 {orig}，经 {up} 升级）")
            }
        } else {
            verdict.to_string()
        };
        let rev_label = if rev_model_label.is_empty() {
            "审查员".to_string()
        } else {
            format!("审查员（{rev_model_label}）")
        };
        let retry_note = if format_retry_failed {
            "〔补问 1 次仍无结论〕"
        } else {
            ""
        };
        if sync_skip {
            out.push(format!("   审查员：同步轮，沿用上一轮审查 PASS{vf_part}"));
        } else if review_env_error {
            let reason = {
                let e = s(rev, "error");
                if !e.is_empty() {
                    e
                } else {
                    let pe = s(rev, "parseError");
                    if !pe.is_empty() {
                        pe
                    } else {
                        "只读审查快照缺失"
                    }
                }
            };
            let retry_cmd = {
                let c = s(rev, "retryCommand");
                if !c.is_empty() {
                    c.to_string()
                } else {
                    crate::review::review_env_error_retry_command(run_id)
                }
            };
            out.push(format!(
                "   {rev_label}：审查环境错误 — {reason}（重试：{retry_cmd}）"
            ));
        } else {
            out.push(format!("   {rev_label}：{v_prefix}{retry_note} — {concl_part}{vf_part}"));
        }
        if rev_extra.is_object() {
            let em = {
                let n = s(rev_extra, "modelName");
                if !n.is_empty() { n } else { s(rev_extra, "model") }
            };
            let em_tag = if em.is_empty() {
                "补充审查".to_string()
            } else {
                format!("补充审查（{em}）")
            };
            match s(rev_extra, "status") {
                "done" => {
                    let ev = s(rev_extra, "verdict");
                    let disagree = if b(rev_extra, "disagreed") {
                        "（意见不一致）"
                    } else {
                        ""
                    };
                    let econcl = {
                        let c = s(rev_extra, "conclusion");
                        if c.is_empty() { "无正文结论" } else { c }
                    };
                    let evf = s(rev_extra, "verdictFile");
                    let evf_part = if evf.is_empty() {
                        String::new()
                    } else {
                        format!("（原文：{evf}）")
                    };
                    out.push(format!(
                        "   {em_tag}：{ev}{disagree} — {econcl}{evf_part}"
                    ));
                }
                "error" => {
                    let err = s(rev_extra, "error");
                    out.push(format!(
                        "   {em_tag}：失败（{}，以主审查为准）",
                        if err.is_empty() { "执行异常" } else { err }
                    ));
                }
                _ => {}
            }
        }
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
    let review_scope_suffix = if b(rev, "enabled") {
        match s(rev, "scope") {
            "full" => "（审查范围：全量）".to_string(),
            "incremental" => format!("（审查范围：增量（{} 个文件））", n(rev, "scopeFiles")),
            "sync_skip" => "（审查范围：同步轮未审）".to_string(),
            _ => String::new(),
        }
    } else {
        String::new()
    };
    out.push(format!("3. 测试与验收{review_scope_suffix}"));
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
                    "passed" => "通过".to_string(),
                    "failed" => "失败".to_string(),
                    "skipped" => "跳过".to_string(),
                    "reused" => {
                        let rn = n(layer, "reusedFromRound");
                        if rn > 0 {
                            format!("沿用第 {rn} 轮通过")
                        } else {
                            "沿用上轮通过".to_string()
                        }
                    }
                    "cancelled" => "取消".to_string(),
                    "not_run" => "未跑".to_string(),
                    _ => st.to_string(),
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
        let attr = &acc["failureAttribution"];
        if attr.is_object() && b(attr, "suspectedMainline") {
            let log_file = s(attr, "logFile");
            out.push("   ! 报错文件不在本 run 改动中（疑似主干已有失败）：".into());
            let outside = attr["outsideRunFiles"].as_array().cloned().unwrap_or_default();
            for item in outside.iter().take(5) {
                let p = s(item, "path");
                let ln = n(item, "logLine");
                if !log_file.is_empty() && ln > 0 {
                    out.push(format!("     - {p}（日志：{log_file}:{ln}）"));
                } else {
                    out.push(format!("     - {p}"));
                }
            }
            if outside.len() > 5 {
                out.push(format!("     … 另有 {} 个报错文件", outside.len() - 5));
            }
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
    let resolved = resolve_diff_and_trees(run, meta, round_only, DEFAULT_RISKY_CONTEXT_LINES)?;
    let parsed_files = parse_unified_diff(&resolved.diff_text);
    let (controller_paths, protect_rules, allowed_rules, risk_cfg) = load_effective_configs(meta);
    let mut rule_risks = detect_rule_risks(
        &parsed_files,
        &controller_paths,
        &protect_rules,
        &allowed_rules,
        &risk_cfg,
    );
    let size_snapshots =
        collect_size_snapshots(&parsed_files, &risk_cfg, resolved.tree_refs.as_ref());
    rule_risks.extend(detect_size_budget_risks(
        &parsed_files,
        &risk_cfg,
        &size_snapshots,
    ));
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
    let mut no_mark = false;
    let mut pos: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--json" => json_mode = true,
            "--round" => round_only = true,
            "--total" => round_only = false,
            "--no-mark" => no_mark = true,
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
    // Zero-lock wakeup if premerge is stale/crashed or a mainline-blocked run's HEAD advanced.
    let src = s(&meta["worktree"], "source");
    if !src.is_empty() {
        let src_path = Path::new(src);
        if s(&sum, "state") == "blocked_mainline"
            && sum["mainlineBlock"].is_object()
            && n(&sum["mainlineBlock"], "rerunCount") == 0
        {
            let cur_head = read_git_head(src_path).unwrap_or_default();
            if !cur_head.is_empty()
                && cur_head != s(&sum["mainlineBlock"], "blockedBaseHead")
                && cur_head != s(&sum["mainlineBlock"], "checkedHead")
            {
                crate::premerge::spawn_worker_force(src_path);
            }
        } else if card_doc["premerge"].is_object()
            && matches!(s(&card_doc["premerge"], "status"), "stale" | "crashed")
        {
            crate::premerge::spawn_worker(src_path);
        }
    }
    if json_mode {
        println!("{}", serde_json::to_string_pretty(&card_doc).unwrap_or_else(|_| card_doc.to_string()));
    } else {
        println!("{}", render_card_human(&card_doc));
    }
    // Reading the L1 review card counts as reading the delivery (clears `[未读交付]`),
    // matching the `下一步：delegate card <run>` affordance unless `--no-mark` is passed.
    if !no_mark
        && run.join("meta.json").is_file()
        && !runs::active(&runs::state(&run))
        && !b(&sum, "question")
        && !b(&sum, "policyRefused")
    {
        crate::mark_result_read(&run);
        touch(run.join(".delivered"));
        crate::board::refresh_for_run(&run);
        crate::events::mark_run_read(&crate::events::run_id_for(&run, &meta));
        crate::work::on_run_result_read(&run, &meta);
        crate::index::upsert_run(&run);
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
        for expected in BUILTIN_RULE_IDS.iter().copied().filter(|id| *id != "size_budget") {
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

    #[test]
    fn lockfile_self_version_bump_is_not_dependency_risk() {
        let self_bump = "diff --git a/crates/delegate/Cargo.lock b/crates/delegate/Cargo.lock\n\
            --- a/crates/delegate/Cargo.lock\n\
            +++ b/crates/delegate/Cargo.lock\n\
            @@ -4,7 +4,7 @@ version = 4\n\
             [[package]]\n\
             name = \"delegate\"\n\
            -version = \"6.34.0\"\n\
            +version = \"6.35.0\"\n\
             dependencies = [\n";
        let files = parse_unified_diff(self_bump);
        let risks = detect_rule_risks(&files, &[], &[], &[], &RiskConfig::default());
        assert!(risks.is_empty(), "self version bump should not flag dependency: {risks:?}");

        // External registry dependency version change (has `source =` / `checksum =`) MUST still flag.
        let ext_bump = "diff --git a/crates/delegate/Cargo.lock b/crates/delegate/Cargo.lock\n\
            --- a/crates/delegate/Cargo.lock\n\
            +++ b/crates/delegate/Cargo.lock\n\
            @@ -20,6 +20,6 @@\n\
             [[package]]\n\
             name = \"libc\"\n\
            -version = \"0.2.150\"\n\
            +version = \"0.2.160\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
        let ext_files = parse_unified_diff(ext_bump);
        let ext_risks = detect_rule_risks(&ext_files, &[], &[], &[], &RiskConfig::default());
        assert!(ext_risks.iter().any(|r| r.rule_id == "dependency"), "{ext_risks:?}");

        // Root package-lock.json self version bump is also exempt; resolved dependency change is not.
        let pkg_self = "diff --git a/package-lock.json b/package-lock.json\n\
            --- a/package-lock.json\n\
            +++ b/package-lock.json\n\
            @@ -2,4 +2,4 @@\n\
               \"name\": \"my-app\",\n\
            -  \"version\": \"1.0.0\",\n\
            +  \"version\": \"1.1.0\",\n\
               \"lockfileVersion\": 3,\n";
        let pkg_files = parse_unified_diff(pkg_self);
        assert!(detect_rule_risks(&pkg_files, &[], &[], &[], &RiskConfig::default()).is_empty());
    }

    #[test]
    fn skip_markers_respect_language_and_syntax_boundaries() {
        // Python `sys.exit(0)` must NOT trigger `xit(`!
        assert_eq!(is_skip_or_ignore_marker("tests/test_app.py", "    sys.exit(0)"), None);
        assert_eq!(is_skip_or_ignore_marker("tests/test_app.py", "    \"sys.exit(0)\""), None);
        // Python `#[ignore]` inside a string fixture must NOT trigger Rust marker.
        assert_eq!(is_skip_or_ignore_marker("tests/test_app.py", "    '#[ignore]\\n'"), None);
        // Real Python decorators DO trigger.
        assert_eq!(
            is_skip_or_ignore_marker("tests/test_app.py", "@unittest.skip('flaky')"),
            Some("@unittest.skip")
        );
        assert_eq!(
            is_skip_or_ignore_marker("tests/test_app.py", "@pytest.mark.skip(reason='wip')"),
            Some("@pytest.mark.skip")
        );
        // Rust `#[ignore]` triggers in .rs, not in strings.
        assert_eq!(is_skip_or_ignore_marker("crates/a/src/lib.rs", "#[ignore]"), Some("#[ignore]"));
        assert_eq!(
            is_skip_or_ignore_marker("crates/a/src/lib.rs", "#[ignore = \"slow\"]"),
            Some("#[ignore]")
        );
        assert_eq!(
            is_skip_or_ignore_marker("crates/a/src/lib.rs", "let s = \"#[ignore]\";"),
            None
        );
        // JS/TS `it.skip(` / `xit(` trigger only in JS/TS with word boundary, not `process.exit(`.
        assert_eq!(is_skip_or_ignore_marker("web/app.test.ts", "  it.skip('broken', () => {})"), Some("it.skip"));
        assert_eq!(is_skip_or_ignore_marker("web/app.test.ts", "  xit('broken', () => {})"), Some("xit"));
        assert_eq!(is_skip_or_ignore_marker("web/cli.ts", "  process.exit(0);"), None);
        assert_eq!(is_skip_or_ignore_marker("crates/a/src/lib.rs", "  xit(0);"), None);
        // Go `t.Skip(` triggers in .go only.
        assert_eq!(is_skip_or_ignore_marker("pkg/foo_test.go", "\tt.Skip(\"wip\")"), Some("t.Skip"));
        assert_eq!(is_skip_or_ignore_marker("tests/test_foo.py", "  t.Skip('wip')"), None);
    }

    #[test]
    fn config_constant_ignores_array_lengths_slices_and_plain_field_fallbacks() {
        // Array length type annotation change `[&str; 10]` -> `[&str; 12]` is NOT a config constant.
        assert!(config_constant_hit("crates/delegate/src/events.rs", "pub const ALERT_KINDS: [&str; 12] = [").is_none());
        // Slice clamping `7.min(len)` / `bh[..7.min(bh.len())]` is NOT a config constant.
        assert!(config_constant_hit("crates/delegate/src/main.rs", "let short = &bh[..7.min(bh.len())];").is_none());
        assert!(config_constant_hit("crates/delegate/src/main.rs", "let n = v.unwrap_or(7.min(len));").is_none());
        // Plain JSON field extraction fallbacks (ms/bytes/seconds) are NOT config defaults.
        assert!(config_constant_hit("crates/delegate/src/main.rs", "let ms = ff[\"ms\"].as_u64().unwrap_or(0);").is_none());
        assert!(config_constant_hit("crates/delegate/src/output.rs", ".map(|item| item[\"bytes\"].as_u64().unwrap_or(0))").is_none());
        assert!(config_constant_hit("crates/delegate/src/events.rs", "let secs = p[\"elapsedSeconds\"].as_f64().unwrap_or(0.0);").is_none());
        // Rust `&'static str` return signature is NOT a static definition.
        assert!(config_constant_hit("crates/delegate/src/premerge.rs", "pub fn code(&self) -> &'static str { \"0\" }").is_none());

        // Real config/constant/timeout changes MUST still trigger.
        assert!(config_constant_hit("crates/core/src/limits.rs", "const MAX_BATCH: usize = 500;").is_some());
        assert!(config_constant_hit("crates/core/src/limits.rs", "static DEFAULT_TIMEOUT_SECS: u64 = 30;").is_some());
        assert!(config_constant_hit("crates/core/src/net.rs", "let timeout = cfg.unwrap_or(30);").is_some());
        assert!(config_constant_hit("crates/core/src/net.rs", "let poll_interval = Duration::from_secs(60);").is_some());
        assert!(config_constant_hit("scripts/cfg.py", "DEFAULT_TIMEOUT = 45").is_some());
    }

    #[test]
    fn card_section1_links_all_round_results_without_cluttering_single_round() {
        let base = std::env::temp_dir().join(format!("delegate-card-rounds-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let single_dir = base.join("20261124-010000-single");
        let multi_dir = base.join("20261124-020000-multi");
        fs::create_dir_all(&single_dir).unwrap();
        fs::create_dir_all(&multi_dir).unwrap();
        fs::write(single_dir.join("meta.json"), r#"{"run":"20261124-010000-single","name":"single"}"#).unwrap();
        fs::write(single_dir.join("result.md"), "single round summary\n").unwrap();

        let single_author = extract_author_summary_with_rounds(&single_dir, true);
        let single_card = json!({
            "run": "20261124-010000-single",
            "name": "single",
            "state": "delivered",
            "changes": {"files": 1, "added": 2, "deleted": 0},
            "l0": {},
            "author": single_author,
            "review": {"enabled": false},
            "risks": [],
            "riskStats": {"total": 0, "ruleHits": 0, "reviewerHints": 0},
            "tests": {"added": 0, "modified": 0, "deleted": 0, "testFiles": 0},
            "accept": Value::Null,
            "otherChanges": [],
            "next": [],
        });
        let single_rendered = render_card_human(&single_card);
        assert!(!single_rendered.contains("各轮原文："), "{single_rendered}");

        fs::write(multi_dir.join("meta.json"), r#"{"run":"20261124-020000-multi","name":"multi"}"#).unwrap();
        fs::write(multi_dir.join("round-1-result.md"), "Round 1 bench: p99=12ms\n").unwrap();
        fs::write(multi_dir.join("round-2-result.md"), "Round 2 fix: null check\n").unwrap();
        fs::write(multi_dir.join("result.md"), "Round 2 fix: null check\n").unwrap();

        let multi_author = extract_author_summary_with_rounds(&multi_dir, true);
        assert_eq!(multi_author["rounds"].as_array().unwrap().len(), 2);
        let multi_card = json!({
            "run": "20261124-020000-multi",
            "name": "multi",
            "state": "delivered",
            "changes": {"files": 1, "added": 2, "deleted": 0},
            "l0": {},
            "author": multi_author,
            "review": {"enabled": false},
            "risks": [],
            "riskStats": {"total": 0, "ruleHits": 0, "reviewerHints": 0},
            "tests": {"added": 0, "modified": 0, "deleted": 0, "testFiles": 0},
            "accept": Value::Null,
            "otherChanges": [],
            "next": [],
        });
        let multi_rendered = render_card_human(&multi_card);
        assert!(multi_rendered.contains("各轮原文："), "{multi_rendered}");
        assert!(multi_rendered.contains("round-1-result.md"), "{multi_rendered}");
        assert!(multi_rendered.contains("round-2-result.md"), "{multi_rendered}");
        assert!(
            multi_rendered.contains("delegate result 20261124-020000-multi --round <1..=2> / --all"),
            "{multi_rendered}"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn card_renders_original_verdict_rebase_sha_and_mainline_failure_attribution() {
        let card = json!({
            "run": "20261124-023910-3d-camera-rebase",
            "name": "3d-camera-rebase",
            "state": "rejected",
            "base": {
                "sha": "37d22a51234567890",
                "short": "37d22a5",
                "rebased": true,
                "previousSha": "e80ed531234567890",
                "previousShort": "e80ed53",
            },
            "changes": {"files": 13, "added": 877, "deleted": 28},
            "l0": {},
            "author": {"lines": ["rebased camera fix"], "path": "/tmp/run/result.md", "totalLines": 1, "totalChars": 18, "rounds": []},
            "review": {
                "enabled": true,
                "status": "done",
                "verdict": "NEEDS_CONTROLLER",
                "originalVerdict": "REWORK",
                "upgradedBy": "controllerReviewPaths",
                "conclusion": "发现阻塞缺陷",
                "verdictFile": "/tmp/run/review/round-1-verdict.md",
            },
            "risks": [],
            "riskStats": {"total": 0, "ruleHits": 0, "reviewerHints": 0},
            "tests": {"added": 0, "modified": 0, "deleted": 0, "testFiles": 0},
            "accept": {
                "ok": false,
                "command": "pnpm check:web",
                "failureAttribution": {
                    "suspectedMainline": true,
                    "logFile": "/tmp/run/accept.log",
                    "inRunFiles": [],
                    "outsideRunFiles": [
                        {"path": "apps/web/src/styles/agent-birth.css", "logLine": 42, "excerpt": "Unknown word"}
                    ],
                },
            },
            "otherChanges": [],
            "next": [
                {"label": "查看验收报错上下文", "command": "sed -n '37,57p' /tmp/run/accept.log"},
            ],
        });
        let rendered = render_card_human(&card);
        let first_line = rendered.lines().next().unwrap_or("");
        assert!(
            first_line.contains("基线 37d22a5（已 rebase：e80ed53→37d22a5）"),
            "{first_line}"
        );
        assert!(rendered.contains("原判 REWORK"), "{rendered}");
        assert!(rendered.contains("round-1-verdict.md"), "{rendered}");
        assert!(
            rendered.contains("报错文件不在本 run 改动中（疑似主干已有失败）"),
            "{rendered}"
        );
        assert!(
            rendered.contains("apps/web/src/styles/agent-birth.css（日志：/tmp/run/accept.log:42）"),
            "{rendered}"
        );
    }

    #[test]
    fn card_renders_review_model_fallback_format_retry_and_extra_review() {
        let card = json!({
            "run": "20261124-203347-rev-demo",
            "name": "rev-demo",
            "state": "delivered",
            "changes": {"files": 1, "added": 5, "deleted": 1},
            "l0": {},
            "author": {"lines": ["demo change"], "path": "/tmp/run/result.md", "totalLines": 1, "totalChars": 11, "rounds": []},
            "review": {
                "enabled": true,
                "status": "done",
                "verdict": "NEEDS_CONTROLLER",
                "originalVerdict": "PASS",
                "upgradedBy": "extraReviewDisagreement",
                "model": "agy/rev-fallback",
                "modelName": "rev-fb",
                "fallback": {
                    "from": "hydrogen",
                    "to": "rev-fb",
                    "reason": "额度用尽"
                },
                "formatError": true,
                "formatRetried": true,
                "formatRetryFailed": true,
                "conclusion": "主审查结论",
                "verdictFile": "/tmp/run/review/round-1-verdict.md",
                "extra": {
                    "status": "done",
                    "modelName": "sonnet",
                    "verdict": "REWORK",
                    "disagreed": true,
                    "conclusion": "补充审查指出风险",
                    "verdictFile": "/tmp/run/review/round-1-extra-verdict.md"
                }
            },
            "risks": [],
            "riskStats": {"total": 0, "ruleHits": 0, "reviewerHints": 0},
            "tests": {"added": 0, "modified": 0, "deleted": 0, "testFiles": 0},
            "accept": {"ok": true, "command": "true"},
            "otherChanges": [],
            "next": [],
        });
        let rendered = render_card_human(&card);
        assert!(rendered.contains("rev-fb"), "{rendered}");
        assert!(rendered.contains("审查改用 rev-fb（额度用尽）"), "{rendered}");
        assert!(rendered.contains("补问 1 次仍无结论"), "{rendered}");
        assert!(rendered.contains("意见不一致"), "{rendered}");
        assert!(rendered.contains("补充审查（sonnet）：REWORK（意见不一致）"), "{rendered}");

        let skipped_card = json!({
            "run": "20261124-203347-skip-demo",
            "name": "skip-demo",
            "state": "delivered",
            "changes": {"files": 1, "added": 5, "deleted": 1},
            "l0": {},
            "author": {"lines": ["demo"], "path": "/tmp/run/result.md", "totalLines": 1, "totalChars": 4, "rounds": []},
            "review": {
                "enabled": true,
                "status": "done",
                "verdict": "PASS",
                "modelName": "hydrogen",
                "extra": {
                    "status": "skipped",
                    "modelName": "sonnet",
                    "skipReason": "补充审查跳过：Claude 池剩余 5 小时 40% / 本周 60%"
                }
            },
            "risks": [],
            "riskStats": {"total": 0, "ruleHits": 0, "reviewerHints": 0},
            "tests": {"added": 0, "modified": 0, "deleted": 0, "testFiles": 0},
            "accept": {"ok": true, "command": "true"},
            "otherChanges": [],
            "next": [],
        });
        let skipped_rendered = render_card_human(&skipped_card);
        assert!(
            skipped_rendered.contains("补充审查跳过：Claude 池剩余 5 小时 40% / 本周 60%"),
            "{skipped_rendered}"
        );
    }

    #[test]
    fn validates_size_budget_risk_rules_schema() {
        assert!(validate_risk_rules(Some(&json!({
            "disable": ["size_budget"],
            "sizeBudgets": [
                {"paths": ["crates/**/*.rs", "!**/generated/**"], "maxLines": 1000, "maxTestFnLines": 150},
                {"paths": ["skills/*/SKILL.md"], "maxTokens": 4000, "label": "Skill 预算"}
            ]
        })))
        .is_ok());

        // Missing any limit must fail
        assert!(validate_risk_rules(Some(&json!({
            "sizeBudgets": [{"paths": ["src/**/*.rs"]}]
        })))
        .is_err());

        // Zero limit must fail
        assert!(validate_risk_rules(Some(&json!({
            "sizeBudgets": [{"paths": ["src/**/*.rs"], "maxLines": 0}]
        })))
        .is_err());

        // Empty paths must fail
        assert!(validate_risk_rules(Some(&json!({
            "sizeBudgets": [{"paths": [], "maxLines": 100}]
        })))
        .is_err());

        // Unknown field must fail
        assert!(validate_risk_rules(Some(&json!({
            "sizeBudgets": [{"paths": ["src/**/*.rs"], "maxLines": 100, "bogus": true}]
        })))
        .is_err());
    }

    #[test]
    fn size_budget_flags_crossing_and_growth_but_skips_untouched_or_shrunk() {
        let cfg = parse_risk_config(&json!({
            "sizeBudgets": [
                {"paths": ["src/**/*.rs", "!src/generated.rs"], "maxLines": 10, "maxTestFnLines": 6},
                {"paths": ["skills/*/SKILL.md"], "maxTokens": 20}
            ]
        }));

        let make_diff = |path: &str| -> DiffFile {
            DiffFile {
                path: path.into(),
                header_lines: vec![],
                hunks: vec![DiffHunk {
                    header: "@@ -1,2 +1,3 @@".into(),
                    old_start: 1,
                    old_count: 2,
                    new_start: 1,
                    new_count: 3,
                    lines: vec![DiffLine {
                        kind: DiffLineKind::Add,
                        old_line: None,
                        new_line: Some(2),
                        text: "added".into(),
                    }],
                    added: 1,
                    deleted: 0,
                }],
                is_new: false,
                is_deleted: false,
                is_binary: false,
                added: 1,
                deleted: 0,
            }
        };

        let files = vec![
            make_diff("src/cross.rs"),
            make_diff("src/grew.rs"),
            make_diff("src/shrunk.rs"),
            make_diff("src/generated.rs"),
            make_diff("skills/demo/SKILL.md"),
            make_diff("src/tests.rs"),
        ];

        let mut snapshots = BTreeMap::new();
        // Crossed 9 -> 11 (> 10): Medium
        snapshots.insert(
            "src/cross.rs".into(),
            (Some("a\n".repeat(9)), Some("a\n".repeat(11))),
        );
        // Already over 12 -> 14 (> 10 and grew): Low
        snapshots.insert(
            "src/grew.rs".into(),
            (Some("a\n".repeat(12)), Some("a\n".repeat(14))),
        );
        // Already over 15 -> 13 (shrunk): NOT flagged
        snapshots.insert(
            "src/shrunk.rs".into(),
            (Some("a\n".repeat(15)), Some("a\n".repeat(13))),
        );
        // Excluded by !src/generated.rs: NOT flagged
        snapshots.insert(
            "src/generated.rs".into(),
            (Some("a\n".repeat(5)), Some("a\n".repeat(50))),
        );
        // Untouched file in snapshots (not in `files`): NOT flagged
        snapshots.insert(
            "src/untouched.rs".into(),
            (Some("a\n".repeat(50)), Some("a\n".repeat(50))),
        );
        // Token budget crossed: 15 CJK chars (15 tokens) -> 25 CJK chars (25 tokens > 20): Medium
        snapshots.insert(
            "skills/demo/SKILL.md".into(),
            (Some("中".repeat(15)), Some("中".repeat(25))),
        );
        // Test function: untouched_big stays 8 lines (not flagged), crossed_fn goes 5 -> 8 (Medium)
        let before_test = "\
#[test]
fn untouched_big() {
    let a = 1;
    let b = 2;
    let c = 3;
    let d = 4;
    let e = 5;
    assert_eq!(a + b + c + d + e, 15);
}

#[test]
fn crossed_fn() {
    let x = 1;
    let y = 2;
    assert_eq!(x + y, 3);
}
";
        let after_test = "\
#[test]
fn untouched_big() {
    let a = 1;
    let b = 2;
    let c = 3;
    let d = 4;
    let e = 5;
    assert_eq!(a + b + c + d + e, 15);
}

#[test]
fn crossed_fn() {
    let x = 1;
    let y = 2;
    let z = 3;
    let w = 4;
    let v = 5;
    assert_eq!(x + y + z + w + v, 15);
}
";
        snapshots.insert(
            "src/tests.rs".into(),
            (Some(before_test.into()), Some(after_test.into())),
        );

        let risks = detect_size_budget_risks(&files, &cfg, &snapshots);

        let cross = risks
            .iter()
            .find(|r| r.file == "src/cross.rs")
            .expect("cross.rs should be flagged");
        assert_eq!(cross.severity, Severity::Medium);
        assert_eq!(cross.category, "体量");
        assert!(cross.reason.contains("跨过阈值"), "{}", cross.reason);

        let grew = risks
            .iter()
            .find(|r| r.file == "src/grew.rs")
            .expect("grew.rs should be flagged");
        assert_eq!(grew.severity, Severity::Low);
        assert!(grew.reason.contains("继续增长"), "{}", grew.reason);

        assert!(!risks.iter().any(|r| r.file == "src/shrunk.rs"));
        assert!(!risks.iter().any(|r| r.file == "src/generated.rs"));
        assert!(!risks.iter().any(|r| r.file == "src/untouched.rs"));

        let skill = risks
            .iter()
            .find(|r| r.file == "skills/demo/SKILL.md")
            .expect("SKILL.md token crossing should be flagged");
        assert_eq!(skill.severity, Severity::Medium);
        assert!(skill.reason.contains("跨过预算"), "{}", skill.reason);

        let test_fn_risks: Vec<&RiskItem> = risks
            .iter()
            .filter(|r| r.file == "src/tests.rs" && r.reason.contains("测试函数"))
            .collect();
        assert_eq!(test_fn_risks.len(), 1, "{test_fn_risks:?}");
        assert!(test_fn_risks[0].reason.contains("crossed_fn"));
        assert_eq!(test_fn_risks[0].severity, Severity::Medium);
    }

    #[test]
    fn card_renders_review_scope_full_incremental_and_sync_skip() {
        let mk = |review: Value| {
            json!({
                "run": "20261126-scope",
                "name": "scope-demo",
                "state": "delivered",
                "files": [],
                "stats": {"files": 1, "added": 3, "deleted": 1},
                "author": {"lines": ["ok"], "path": "", "totalLines": 1, "totalChars": 2},
                "tests": {"added": 0, "modified": 0, "deleted": 0, "testFiles": 0},
                "accept": {"ok": true, "command": "true"},
                "review": review,
                "risks": [],
                "riskStats": {"total": 0, "ruleHits": 0, "reviewerHints": 0},
                "fastForward": {"status": "ready", "label": "可快进"},
            })
        };

        let full_out = render_card_human(&mk(json!({
            "enabled": true,
            "status": "done",
            "verdict": "PASS",
            "scope": "full",
            "scopeFiles": 4,
            "modelName": "rev",
        })));
        assert!(full_out.contains("3. 测试与验收（审查范围：全量）"), "{full_out}");

        let inc_out = render_card_human(&mk(json!({
            "enabled": true,
            "status": "done",
            "verdict": "PASS",
            "scope": "incremental",
            "scopeFiles": 2,
            "modelName": "rev",
        })));
        assert!(inc_out.contains("3. 测试与验收（审查范围：增量（2 个文件））"), "{inc_out}");

        let sync_out = render_card_human(&mk(json!({
            "enabled": true,
            "status": "done",
            "verdict": "PASS",
            "scope": "sync_skip",
            "scopeFiles": 0,
            "syncSkip": true,
            "modelName": "rev",
        })));
        assert!(sync_out.contains("同步轮，沿用上一轮审查 PASS"), "{sync_out}");
        assert!(sync_out.contains("3. 测试与验收（审查范围：同步轮未审）"), "{sync_out}");
    }

    #[test]
    fn card_renders_review_env_error_distinct_from_needs_controller() {
        let card = json!({
            "run": "20261127-env-err",
            "name": "env-err-demo",
            "state": "delivered",
            "files": [],
            "stats": {"files": 1, "added": 2, "deleted": 0},
            "author": {"lines": ["ok"], "path": "", "totalLines": 1, "totalChars": 2},
            "tests": {"added": 0, "modified": 0, "deleted": 0, "testFiles": 0},
            "accept": {"ok": true, "command": "true"},
            "review": {
                "enabled": true,
                "status": "env_error",
                "verdict": "ENV_ERROR",
                "envError": true,
                "error": "只读审查快照缺失（/tmp/review-ro/20261127-env-err）",
                "retryCommand": "delegate reply 20261127-env-err --review-full '重跑审查'",
                "scope": "incremental",
                "scopeFiles": 1,
                "modelName": "hydrogen",
            },
            "risks": [],
            "riskStats": {"total": 0, "ruleHits": 0, "reviewerHints": 0},
            "fastForward": {"status": "ready", "label": "可快进"},
            "next": [
                {"label": "重试审查（审查环境错误）", "command": "delegate reply 20261127-env-err --review-full '重跑审查'"},
                {"label": "确认无误合入", "command": "delegate apply 20261127-env-err"},
            ],
        });
        let out = render_card_human(&card);
        assert!(out.contains("审查：审查环境错误 · hydrogen"), "{out}");
        assert!(out.contains("只读审查快照缺失"), "{out}");
        assert!(
            out.contains("delegate reply 20261127-env-err --review-full '重跑审查'"),
            "{out}"
        );
        assert!(!out.contains("NEEDS_CONTROLLER"), "{out}");
    }
}
