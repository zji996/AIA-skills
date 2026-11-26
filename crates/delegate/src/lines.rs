//! Deterministic git line-change statistics across registered repositories (`delegate lines`).
//! Zero model calls; cached per `(repo HEAD, exclude rules, covered window)` in `<state>/line-stats-cache.json`.
use crate::common::*;
use crate::review;
use crate::usage::{self, TokenCounts};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_EXCLUDE_GLOBS: &[&str] = &[
    "**/*.lock",
    "**/Cargo.lock",
    "**/pnpm-lock.yaml",
    "**/package-lock.json",
    "**/generated/**",
    "**/__snapshots__/**",
    "**/*.snap",
    "**/*.golden.md",
];

const TEST_GLOBS: &[&str] = &[
    "tests/**",
    "**/tests/**",
    "**/*_test.rs",
    "**/*_tests.rs",
    "**/*.test.ts",
    "**/*.test.tsx",
    "**/*.spec.ts",
    "**/test_*.py",
];

const DAY_SECS: i64 = 86_400;
const SEVEN_DAYS_SECS: i64 = 7 * DAY_SECS;
const CACHE_VERSION: u64 = 1;
const MINUS_SIGN: char = '\u{2212}'; // U+2212 '−'

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineCategory {
    Code,
    Test,
    Docs,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LineCounts {
    pub added: u64,
    pub deleted: u64,
}

impl LineCounts {
    pub fn churn(self) -> u64 {
        self.added.saturating_add(self.deleted)
    }

    pub fn add(&mut self, other: Self) {
        self.added = self.added.saturating_add(other.added);
        self.deleted = self.deleted.saturating_add(other.deleted);
    }

    pub fn to_json(self) -> Value {
        json!({
            "added": self.added,
            "deleted": self.deleted,
            "churn": self.churn(),
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CategoryCounts {
    pub delegated: LineCounts,
    pub other: LineCounts,
}

impl CategoryCounts {
    pub fn total(self) -> LineCounts {
        LineCounts {
            added: self.delegated.added.saturating_add(self.other.added),
            deleted: self.delegated.deleted.saturating_add(self.other.deleted),
        }
    }

    pub fn add(&mut self, other: Self) {
        self.delegated.add(other.delegated);
        self.other.add(other.other);
    }

    pub fn to_json(self) -> Value {
        json!({
            "total": self.total().to_json(),
            "delegated": self.delegated.to_json(),
            "other": self.other.to_json(),
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Breakdown {
    pub code: CategoryCounts,
    pub test: CategoryCounts,
    pub docs: CategoryCounts,
}

impl Breakdown {
    pub fn total(self) -> LineCounts {
        let mut t = self.code.total();
        t.add(self.test.total());
        t.add(self.docs.total());
        t
    }

    pub fn delegated(self) -> LineCounts {
        let mut t = self.code.delegated;
        t.add(self.test.delegated);
        t.add(self.docs.delegated);
        t
    }

    pub fn other(self) -> LineCounts {
        let mut t = self.code.other;
        t.add(self.test.other);
        t.add(self.docs.other);
        t
    }

    pub fn add(&mut self, other: Self) {
        self.code.add(other.code);
        self.test.add(other.test);
        self.docs.add(other.docs);
    }

    pub fn by_category_json(self) -> Value {
        json!({
            "code": self.code.to_json(),
            "test": self.test.to_json(),
            "docs": self.docs.to_json(),
        })
    }

    pub fn by_attribution_json(self) -> Value {
        json!({
            "delegated": self.delegated().to_json(),
            "other": self.other().to_json(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommitStat {
    ct: i64,
    runs: Vec<String>,
    code: LineCounts,
    test: LineCounts,
    docs: LineCounts,
}

impl CommitStat {
    fn is_delegated(&self) -> bool {
        !self.runs.is_empty()
    }

    fn total(&self) -> LineCounts {
        let mut t = self.code;
        t.add(self.test);
        t.add(self.docs);
        t
    }

    fn to_json(&self) -> Value {
        json!({
            "ct": self.ct,
            "runs": self.runs,
            "code": [self.code.added, self.code.deleted],
            "test": [self.test.added, self.test.deleted],
            "docs": [self.docs.added, self.docs.deleted],
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        let ct = v.get("ct")?.as_i64()?;
        let runs = v
            .get("runs")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let pair = |key: &str| -> Option<LineCounts> {
            let a = v.get(key)?.as_array()?;
            if a.len() != 2 {
                return None;
            }
            Some(LineCounts {
                added: a[0].as_u64()?,
                deleted: a[1].as_u64()?,
            })
        };
        Some(Self {
            ct,
            runs,
            code: pair("code")?,
            test: pair("test")?,
            docs: pair("docs")?,
        })
    }
}

#[derive(Debug, Clone)]
struct RepoCacheEntry {
    head: String,
    exclude_rules: Vec<String>,
    covered_since: i64,
    commits: Vec<CommitStat>,
}

impl RepoCacheEntry {
    fn to_json(&self) -> Value {
        json!({
            "head": self.head,
            "excludeRules": self.exclude_rules,
            "coveredSince": self.covered_since,
            "commits": self.commits.iter().map(CommitStat::to_json).collect::<Vec<_>>(),
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        let head = v.get("head")?.as_str()?.to_string();
        let exclude_rules = v
            .get("excludeRules")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let covered_since = v.get("coveredSince")?.as_i64()?;
        let commits = v
            .get("commits")?
            .as_array()?
            .iter()
            .filter_map(CommitStat::from_json)
            .collect();
        Some(Self {
            head,
            exclude_rules,
            covered_since,
            commits,
        })
    }
}

/// Validate repository-only `lineStats` config: `{"exclude": ["<glob>", "!<glob>"]}`.
/// Fails closed and names the invalid field.
pub fn validate_line_stats_config(value: Option<&Value>) -> Res<()> {
    let Some(v) = value else {
        return Ok(());
    };
    if v.is_null() {
        return Ok(());
    }
    let Some(obj) = v.as_object() else {
        return Err("lineStats must be an object {\"exclude\": [...]}".into());
    };
    for key in obj.keys() {
        if key != "exclude" {
            return Err(format!("lineStats: unknown field '{key}'"));
        }
    }
    if let Some(excl) = obj.get("exclude") {
        if excl.is_null() {
            return Ok(());
        }
        let Some(arr) = excl.as_array() else {
            return Err("lineStats.exclude must be a list of non-empty glob strings".into());
        };
        for (idx, item) in arr.iter().enumerate() {
            let Some(raw) = item.as_str() else {
                return Err(format!("lineStats.exclude[{idx}] must be a string"));
            };
            if raw.contains('\0') {
                return Err(format!("lineStats.exclude[{idx}] must not contain NUL"));
            }
            let trimmed = raw.trim();
            let pattern = trimmed.strip_prefix('!').unwrap_or(trimmed).trim();
            if pattern.is_empty() {
                return Err(format!(
                    "lineStats.exclude[{idx}] ({raw:?}) has an empty glob pattern"
                ));
            }
        }
    }
    Ok(())
}

/// Load and validate `lineStats.exclude` from `<repo>/.delegate.json`.
pub fn load_repo_exclude_rules(repo: &Path) -> Res<Vec<String>> {
    let cfg_path = repo.join(".delegate.json");
    let Some(doc) = crate::config::read_file(&cfg_path)? else {
        return Ok(Vec::new());
    };
    validate_line_stats_config(doc.get("lineStats"))
        .map_err(|e| format!("{}: {e}", cfg_path.display()))?;
    let mut rules = Vec::new();
    if let Some(arr) = doc
        .get("lineStats")
        .and_then(|v| v.get("exclude"))
        .and_then(Value::as_array)
    {
        for item in arr {
            if let Some(raw) = item.as_str() {
                rules.push(raw.trim().to_string());
            }
        }
    }
    Ok(rules)
}

/// Check if a repo-relative path is excluded by default globs and repo `lineStats.exclude`.
/// Evaluated in order (default excludes first, then repo rules); later rules override earlier ones,
/// and `!` re-includes matching paths (no `add:` prefix).
pub fn is_excluded(path: &str, repo_rules: &[String]) -> bool {
    let norm = path.trim_start_matches("./");
    let mut excluded = false;
    for pat in DEFAULT_EXCLUDE_GLOBS {
        if review::glob_match(pat, norm) {
            excluded = true;
            break;
        }
    }
    for raw in repo_rules {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (negated, pattern) = match trimmed.strip_prefix('!') {
            Some(rest) => (true, rest.trim()),
            None => (false, trimmed),
        };
        if pattern.is_empty() {
            continue;
        }
        if review::glob_match(pattern, norm) {
            excluded = !negated;
        }
    }
    excluded
}

/// Classify a non-excluded repo-relative path into Test, Docs, or Code.
/// Tests are checked before Docs so `tests/**/*.md` counts as Test.
pub fn classify_path(path: &str) -> LineCategory {
    let norm = path.trim_start_matches("./");
    for pat in TEST_GLOBS {
        if review::glob_match(pat, norm) {
            return LineCategory::Test;
        }
    }
    if review::glob_match("**/*.md", norm) {
        return LineCategory::Docs;
    }
    LineCategory::Code
}

/// Extract `Delegate-Run: <run>` trailer values from a commit message.
pub fn extract_delegate_run_trailers(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some((key, val)) = trimmed.split_once(':') {
            if key.trim().eq_ignore_ascii_case("delegate-run") {
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

fn is_hex_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn resolve_git_dirs(repo: &Path) -> Option<(PathBuf, PathBuf)> {
    let dot_git = repo.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else if dot_git.is_file() {
        let raw = fs::read_to_string(&dot_git).ok()?;
        let rest = raw.trim().strip_prefix("gitdir:")?.trim();
        if rest.is_empty() {
            return None;
        }
        let p = PathBuf::from(rest);
        let abs = if p.is_absolute() { p } else { repo.join(p) };
        if abs.is_dir() {
            abs
        } else {
            return None;
        }
    } else {
        return None;
    };
    let common_dir = {
        let cd_file = git_dir.join("commondir");
        if cd_file.is_file() {
            let raw = fs::read_to_string(&cd_file).ok()?;
            let rel = raw.trim();
            if rel.is_empty() {
                git_dir.clone()
            } else {
                let p = PathBuf::from(rel);
                if p.is_absolute() {
                    p
                } else {
                    git_dir.join(p)
                }
            }
        } else {
            git_dir.clone()
        }
    };
    if common_dir.join("reftable").exists() {
        return None;
    }
    Some((git_dir, common_dir))
}

fn read_packed_ref(common_dir: &Path, ref_name: &str) -> Option<String> {
    let text = fs::read_to_string(common_dir.join("packed-refs")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let sha = parts.next()?;
        let name = parts.next()?;
        if name == ref_name && is_hex_sha(sha) {
            return Some(sha.to_string());
        }
    }
    None
}

fn read_ref_raw(git_dir: &Path, common_dir: &Path, ref_name: &str) -> Option<String> {
    let file = if ref_name == "HEAD" {
        git_dir.join("HEAD")
    } else {
        common_dir.join(ref_name)
    };
    if let Ok(text) = fs::read_to_string(&file) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if ref_name != "HEAD" {
        return read_packed_ref(common_dir, ref_name);
    }
    None
}

fn resolve_ref_fast(git_dir: &Path, common_dir: &Path, start_ref: &str) -> Option<String> {
    let mut cur = start_ref.to_string();
    for _ in 0..8 {
        let raw = read_ref_raw(git_dir, common_dir, &cur)?;
        if let Some(target) = raw.strip_prefix("ref:") {
            cur = target.trim().to_string();
            continue;
        }
        if is_hex_sha(&raw) {
            return Some(raw);
        }
        return None;
    }
    None
}

/// Resolve the default branch HEAD commit SHA (`origin/HEAD` -> `main` -> `master` -> `HEAD`).
/// Returns `Ok(None)` for a valid empty repository (unborn HEAD with 0 commits),
/// and `Err(reason)` if `repo` cannot be read as a git repository.
fn resolve_default_branch_head(repo: &Path) -> Res<Option<String>> {
    if !repo.is_dir() {
        return Err("目录不存在".into());
    }
    // Fast pure-Rust ref resolution (zero git subprocesses when HEAD is unchanged).
    if let Some((git_dir, common_dir)) = resolve_git_dirs(repo) {
        if let Some(raw_origin_head) = read_ref_raw(&git_dir, &common_dir, "refs/remotes/origin/HEAD")
        {
            if let Some(sym) = raw_origin_head.strip_prefix("ref:").map(str::trim) {
                if let Some(branch) = sym.strip_prefix("refs/remotes/origin/") {
                    if let Some(sha) =
                        resolve_ref_fast(&git_dir, &common_dir, &format!("refs/heads/{branch}"))
                            .or_else(|| {
                                resolve_ref_fast(
                                    &git_dir,
                                    &common_dir,
                                    &format!("refs/remotes/origin/{branch}"),
                                )
                            })
                    {
                        return Ok(Some(sha));
                    }
                }
            }
        }
        for branch in ["main", "master"] {
            if let Some(sha) =
                resolve_ref_fast(&git_dir, &common_dir, &format!("refs/heads/{branch}"))
                    .or_else(|| {
                        resolve_ref_fast(
                            &git_dir,
                            &common_dir,
                            &format!("refs/remotes/origin/{branch}"),
                        )
                    })
            {
                return Ok(Some(sha));
            }
        }
        if let Some(sha) = resolve_ref_fast(&git_dir, &common_dir, "HEAD") {
            return Ok(Some(sha));
        }
    }

    // Fallback to git subprocess (handles reftables, unusual worktree layouts, unborn HEAD, and errors).
    let check = git_command()
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--git-dir"])
        .output()
        .map_err(|e| e.to_string())?;
    if !check.status.success() {
        let msg = String::from_utf8_lossy(&check.stderr).trim().to_string();
        return Err(if msg.is_empty() {
            "不是有效的 git 仓库".into()
        } else {
            msg
        });
    }

    let rev = |name: &str| -> Option<String> {
        git_text(repo, &["rev-parse", "-q", "--verify", &format!("{name}^{{commit}}")])
            .ok()
            .filter(|s| !s.is_empty())
    };
    if let Ok(sym) = git_text(repo, &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"]) {
        if let Some(branch) = sym.strip_prefix("refs/remotes/origin/") {
            if let Some(sha) = rev(&format!("refs/heads/{branch}"))
                .or_else(|| rev(&format!("refs/remotes/origin/{branch}")))
            {
                return Ok(Some(sha));
            }
        }
    }
    for branch in ["main", "master"] {
        if let Some(sha) = rev(&format!("refs/heads/{branch}"))
            .or_else(|| rev(&format!("refs/remotes/origin/{branch}")))
        {
            return Ok(Some(sha));
        }
    }
    if let Some(sha) = rev("HEAD") {
        return Ok(Some(sha));
    }
    // Valid git repository with unborn HEAD (0 commits).
    if git_text(repo, &["symbolic-ref", "-q", "HEAD"]).is_ok() {
        return Ok(None);
    }
    Err("无法解析默认分支 HEAD".into())
}

/// Parse the output of `git log --no-merges -M -l0 --numstat -z --format=%x1e%H%x1f%ct%x1f%B%x1e`.
fn parse_git_log_numstat_z(raw: &[u8], exclude_rules: &[String]) -> Vec<CommitStat> {
    let mut commits = Vec::new();
    let mut i = 0usize;
    let n = raw.len();

    while i < n {
        // Advance to next commit record marker 0x1e.
        while i < n && raw[i] != 0x1e {
            i += 1;
        }
        if i >= n {
            break;
        }
        i += 1; // skip opening 0x1e
        let hdr_start = i;
        while i < n && raw[i] != 0x1e {
            i += 1;
        }
        if i >= n {
            break;
        }
        let hdr_bytes = &raw[hdr_start..i];
        i += 1; // skip closing 0x1e
        if i < n && raw[i] == 0 {
            i += 1; // skip NUL terminator emitted by `-z` after format
        }

        // Split header: sha \x1f ct \x1f body
        let mut parts = hdr_bytes.splitn(3, |&b| b == 0x1f);
        let _sha = parts.next();
        let ct_bytes = parts.next().unwrap_or_default();
        let body_bytes = parts.next().unwrap_or_default();
        let ct = std::str::from_utf8(ct_bytes)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0);
        let body = String::from_utf8_lossy(body_bytes);
        let runs = extract_delegate_run_trailers(&body);

        let mut code = LineCounts::default();
        let mut test = LineCounts::default();
        let mut docs = LineCounts::default();

        // Parse numstat entries until EOF or next 0x1e commit header.
        loop {
            while i < n && raw[i] == b'\n' {
                i += 1;
            }
            if i >= n || raw[i] == 0x1e {
                break;
            }
            let entry_start = i;
            while i < n && raw[i] != 0 {
                i += 1;
            }
            let entry = &raw[entry_start..i];
            if i < n && raw[i] == 0 {
                i += 1;
            }
            let mut fields = entry.splitn(3, |&b| b == b'\t');
            let Some(add_b) = fields.next() else {
                continue;
            };
            let Some(del_b) = fields.next() else {
                continue;
            };
            let Some(path_b) = fields.next() else {
                continue;
            };

            let target_path_bytes = if path_b.is_empty() {
                // Rename entry: `<added>\t<deleted>\t\0<old>\0<new>\0`
                while i < n && raw[i] != 0 {
                    i += 1;
                }
                if i < n && raw[i] == 0 {
                    i += 1;
                }
                let new_start = i;
                while i < n && raw[i] != 0 {
                    i += 1;
                }
                let new_b = &raw[new_start..i];
                if i < n && raw[i] == 0 {
                    i += 1;
                }
                new_b
            } else {
                path_b
            };

            // Binary files report `-` for added/deleted.
            if add_b == b"-" || del_b == b"-" {
                continue;
            }
            let added = std::str::from_utf8(add_b)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            let deleted = std::str::from_utf8(del_b)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            let path = String::from_utf8_lossy(target_path_bytes);
            if is_excluded(&path, exclude_rules) {
                continue;
            }
            let counts = LineCounts { added, deleted };
            match classify_path(&path) {
                LineCategory::Code => code.add(counts),
                LineCategory::Test => test.add(counts),
                LineCategory::Docs => docs.add(counts),
            }
        }

        commits.push(CommitStat {
            ct,
            runs,
            code,
            test,
            docs,
        });
    }

    commits
}

fn fetch_repo_commits_git(
    repo: &Path,
    head_sha: &str,
    since_epoch: i64,
    exclude_rules: &[String],
) -> Res<Vec<CommitStat>> {
    // 24h slop ensures minor out-of-order committer dates don't cause git's `--since` to stop early.
    let fetch_since = since_epoch.saturating_sub(DAY_SECS).max(0);
    let since_arg = format!("--since=@{fetch_since}");
    let out = git_command()
        .arg("-C")
        .arg(repo)
        .args([
            "log",
            "--no-merges",
            "-M",
            "-l0",
            "--numstat",
            "-z",
            &since_arg,
            "--format=%x1e%H%x1f%ct%x1f%B%x1e",
            head_sha,
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("git log 失败（退出码 {:?}）", out.status.code())
        } else {
            stderr
        });
    }
    Ok(parse_git_log_numstat_z(&out.stdout, exclude_rules))
}

fn cache_path(state: &Path) -> PathBuf {
    state.join("line-stats-cache.json")
}

fn load_cache(state: &Path) -> HashMap<String, RepoCacheEntry> {
    let path = cache_path(state);
    if !path.is_file() {
        return HashMap::new();
    }
    let doc = json(&path);
    if doc.get("version").and_then(Value::as_u64) != Some(CACHE_VERSION) {
        return HashMap::new();
    }
    let mut out = HashMap::new();
    if let Some(map) = doc.get("entries").and_then(Value::as_object) {
        for (k, v) in map {
            if let Some(entry) = RepoCacheEntry::from_json(v) {
                out.insert(k.clone(), entry);
            }
        }
    }
    out
}

fn save_cache(state: &Path, cache: &HashMap<String, RepoCacheEntry>) {
    let mut entries = Map::new();
    for (k, v) in cache {
        entries.insert(k.clone(), v.to_json());
    }
    let doc = json!({
        "version": CACHE_VERSION,
        "entries": Value::Object(entries),
    });
    let _ = write_json(cache_path(state), &doc);
}

struct ChainIndex {
    parent_of: HashMap<String, String>,
    children_of: HashMap<String, Vec<String>>,
    scout_of: HashMap<String, Vec<String>>,
    history_tokens: HashMap<String, TokenCounts>,
}

impl ChainIndex {
    /// Build parent/child & scoutRun links across active run directories, `<cache>/archive/*/record/meta.json`,
    /// and `<state>/history/*/history.jsonl`, plus fallback token totals from `history.jsonl`.
    fn load(state: &Path, runs_dirs: &[PathBuf]) -> Self {
        let mut parent_of: HashMap<String, String> = HashMap::new();
        let mut children_of: HashMap<String, Vec<String>> = HashMap::new();
        let mut scout_of: HashMap<String, Vec<String>> = HashMap::new();
        let mut history_tokens: HashMap<String, TokenCounts> = HashMap::new();

        let mut link = |run_id: String, parent_id: String| {
            if run_id.is_empty() || parent_id.is_empty() || run_id == parent_id {
                return;
            }
            if !parent_of.contains_key(&run_id) {
                parent_of.insert(run_id.clone(), parent_id.clone());
                let kids = children_of.entry(parent_id).or_default();
                if !kids.contains(&run_id) {
                    kids.push(run_id);
                }
            }
        };

        let mut link_scout = |run_id: &str, scout_id: &str| {
            if run_id.is_empty() || scout_id.is_empty() || run_id == scout_id {
                return;
            }
            let list = scout_of.entry(run_id.to_string()).or_default();
            if !list.iter().any(|x| x == scout_id) {
                list.push(scout_id.to_string());
            }
        };

        // 1. Active run directories across discovered repos
        for rdir in runs_dirs {
            for run in crate::runs::all_runs_in(rdir) {
                let meta = json(run.join("meta.json"));
                if !meta.is_object() {
                    continue;
                }
                let run_id = {
                    let r = s(&meta, "run");
                    if !r.is_empty() {
                        r.to_string()
                    } else {
                        run.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    }
                };
                let parent = s(&meta, "parent").to_string();
                let sum = json(run.join("summary.json"));
                let scout = {
                    let v = s(&sum, "scoutRun");
                    if !v.is_empty() {
                        v.to_string()
                    } else {
                        s(&meta, "scoutRun").to_string()
                    }
                };
                let scout_tok = if sum["scoutTokens"].is_object() {
                    Some(TokenCounts::from_json(&sum["scoutTokens"]))
                } else if meta["scout"]["tokens"].is_object() {
                    Some(TokenCounts::from_json(&meta["scout"]["tokens"]))
                } else {
                    None
                };
                link_scout(&run_id, &scout);
                if let Some(tok) = scout_tok.filter(|t| !t.is_zero()) {
                    if !scout.is_empty() {
                        history_tokens.entry(scout).or_insert(tok);
                    }
                }
                link(run_id, parent);
            }
        }

        // 2. Archived run records (`<cache>/archive/*/record/meta.json`)
        let arch_root = cache_dir().join("archive");
        if let Ok(rd) = fs::read_dir(&arch_root) {
            for ent in rd.flatten() {
                let p = ent.path();
                if !p.is_dir() {
                    continue;
                }
                let rec = p.join("record");
                let meta = json(rec.join("meta.json"));
                if !meta.is_object() {
                    continue;
                }
                let run_id = {
                    let r = s(&meta, "run");
                    if !r.is_empty() {
                        r.to_string()
                    } else {
                        p.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    }
                };
                let parent = s(&meta, "parent").to_string();
                let sum = json(rec.join("summary.json"));
                let scout = {
                    let v = s(&sum, "scoutRun");
                    if !v.is_empty() {
                        v.to_string()
                    } else {
                        s(&meta, "scoutRun").to_string()
                    }
                };
                let scout_tok = if sum["scoutTokens"].is_object() {
                    Some(TokenCounts::from_json(&sum["scoutTokens"]))
                } else if meta["scout"]["tokens"].is_object() {
                    Some(TokenCounts::from_json(&meta["scout"]["tokens"]))
                } else {
                    None
                };
                link_scout(&run_id, &scout);
                if let Some(tok) = scout_tok.filter(|t| !t.is_zero()) {
                    if !scout.is_empty() {
                        history_tokens.entry(scout).or_insert(tok);
                    }
                }
                link(run_id, parent);
            }
        }

        // 3. Persistent history logs (`<state>/history/*/history.jsonl`)
        let hist_root = state.join("history");
        if let Ok(rd) = fs::read_dir(&hist_root) {
            for ent in rd.flatten() {
                let hp = ent.path().join("history.jsonl");
                if !hp.is_file() {
                    continue;
                }
                for line in read(&hp).lines() {
                    let Ok(v) = serde_json::from_str::<Value>(line) else {
                        continue;
                    };
                    let run_id = s(&v, "run").to_string();
                    if run_id.is_empty() {
                        continue;
                    }
                    let parent = s(&v, "parent").to_string();
                    let scout = s(&v, "scoutRun").to_string();
                    link_scout(&run_id, &scout);
                    link(run_id.clone(), parent);
                    let tok_v = if v["tokensDelta"].is_object() {
                        &v["tokensDelta"]
                    } else {
                        &v["tokens"]
                    };
                    let tok = TokenCounts::from_json(tok_v);
                    if !tok.is_zero() {
                        history_tokens.insert(run_id, tok);
                    }
                }
            }
        }

        Self {
            parent_of,
            children_of,
            scout_of,
            history_tokens,
        }
    }

    /// Expand a set of run IDs to include every round in their reply chains (both ancestors and descendants)
    /// as well as any associated `scoutRun` read-only scout runs.
    fn expand_chains(&self, seeds: &BTreeSet<String>) -> BTreeSet<String> {
        let mut visited: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        for id in seeds {
            if visited.insert(id.clone()) {
                queue.push_back(id.clone());
            }
        }
        while let Some(cur) = queue.pop_front() {
            if let Some(parent) = self.parent_of.get(&cur) {
                if visited.insert(parent.clone()) {
                    queue.push_back(parent.clone());
                }
            }
            if let Some(kids) = self.children_of.get(&cur) {
                for kid in kids {
                    if visited.insert(kid.clone()) {
                        queue.push_back(kid.clone());
                    }
                }
            }
            if let Some(scouts) = self.scout_of.get(&cur) {
                for sc in scouts {
                    if visited.insert(sc.clone()) {
                        queue.push_back(sc.clone());
                    }
                }
            }
        }
        visited
    }
}

struct RepoData {
    label: String,
    path: PathBuf,
    head: String,
    commits: Vec<CommitStat>,
}

struct RepoError {
    label: String,
    path: PathBuf,
    error: String,
}

struct WindowSpec {
    key: String,
    label: String,
    since_secs: i64,
    since_epoch: i64,
}

struct RepoWindowResult {
    label: String,
    path: PathBuf,
    head: String,
    breakdown: Breakdown,
}

struct WindowResult {
    spec: WindowSpec,
    breakdown: Breakdown,
    delegated_share_pct: Option<f64>,
    delegated_runs: BTreeSet<String>,
    chain_runs: BTreeSet<String>,
    tokens: TokenCounts,
    lines_per_million_tokens: Option<f64>,
    by_repo: Vec<RepoWindowResult>,
}

struct DailyPoint {
    date: String,
    counts: LineCounts,
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn format_window_label(raw: &str, secs: i64) -> String {
    if secs == DAY_SECS {
        return "24 小时".into();
    }
    if secs == SEVEN_DAYS_SECS {
        return "7 天".into();
    }
    if let Some(n) = raw.strip_suffix('d') {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return format!("{n} 天");
        }
    }
    if let Some(n) = raw.strip_suffix('h') {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return format!("{n} 小时");
        }
    }
    if let Some(n) = raw.strip_suffix('m') {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return format!("{n} 分钟");
        }
    }
    raw.to_string()
}

fn assign_repo_labels(repos: &[(PathBuf, PathBuf)]) -> Vec<String> {
    let basenames: Vec<String> = repos
        .iter()
        .map(|(p, _)| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| p.display().to_string())
        })
        .collect();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for b in &basenames {
        *counts.entry(b.as_str()).or_default() += 1;
    }
    repos
        .iter()
        .zip(basenames.iter())
        .map(|((p, _), base)| {
            if counts.get(base.as_str()).copied().unwrap_or(0) > 1 {
                if let Some(parent) = p.parent().and_then(|pp| pp.file_name()) {
                    return format!("{}/{base}", parent.to_string_lossy());
                }
            }
            base.clone()
        })
        .collect()
}

fn format_commas(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (idx, &b) in bytes.iter().enumerate() {
        if idx > 0 && (bytes.len() - idx).is_multiple_of(3) {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            let cp = c as u32;
            if (0x1100..=0x115F).contains(&cp)
                || (0x2E80..=0xA4CF).contains(&cp)
                || (0xAC00..=0xD7A3).contains(&cp)
                || (0xF900..=0xFAFF).contains(&cp)
                || (0xFE10..=0xFE19).contains(&cp)
                || (0xFE30..=0xFE6F).contains(&cp)
                || (0xFF00..=0xFF60).contains(&cp)
                || (0xFFE0..=0xFFE6).contains(&cp)
                || (0x20000..=0x2FFFD).contains(&cp)
                || (0x30000..=0x3FFFD).contains(&cp)
            {
                2
            } else {
                1
            }
        })
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

fn pad_left(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(width - w))
    }
}

fn pad_center(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        s.to_string()
    } else {
        let total = width - w;
        let left = total / 2;
        let right = total - left;
        format!("{}{s}{}", " ".repeat(left), " ".repeat(right))
    }
}

fn render_human(
    windows: &[WindowResult],
    by_repo: bool,
    repo_order: &[String],
    errors: &[RepoError],
) -> String {
    let mut out = String::new();
    out.push_str("代码改动（各仓库默认分支，不含合并提交与生成文件）\n");

    let mut label_w = display_width("每百万 token");
    if by_repo {
        for r in repo_order {
            label_w = label_w.max(display_width(&format!("[{r}]")));
        }
    }

    // Per-column plus/minus widths so `/` aligns vertically across every numeric row.
    struct ColLayout {
        plus_w: usize,
        minus_w: usize,
        width: usize,
    }

    let mut cols: Vec<ColLayout> = Vec::with_capacity(windows.len());
    for w in windows {
        let mut plus_w = 0usize;
        let mut minus_w = 0usize;
        let mut observe = |c: LineCounts| {
            plus_w = plus_w.max(format!("+{}", format_commas(c.added)).len());
            minus_w = minus_w.max(display_width(&format!("{MINUS_SIGN}{}", format_commas(c.deleted))));
        };
        observe(w.breakdown.code.total());
        observe(w.breakdown.test.total());
        observe(w.breakdown.docs.total());
        observe(w.breakdown.total());
        if by_repo {
            for r in &w.by_repo {
                observe(r.breakdown.total());
            }
        }
        let pair_w = plus_w + 3 + minus_w; // ` / ` is 3 cols
        let header_w = display_width(&w.spec.label);
        let pct_str = format_pct(w.delegated_share_pct);
        let tok_str = format_lines_per_m(w.lines_per_million_tokens);
        let width = pair_w
            .max(header_w)
            .max(display_width(&pct_str))
            .max(display_width(&tok_str));
        cols.push(ColLayout {
            plus_w,
            minus_w,
            width,
        });
    }

    let col_gap = "    ";
    let format_pair_cell = |c: LineCounts, lay: &ColLayout| -> String {
        let left = pad_left(&format!("+{}", format_commas(c.added)), lay.plus_w);
        let right = pad_right(
            &format!("{MINUS_SIGN}{}", format_commas(c.deleted)),
            lay.minus_w,
        );
        let cell = format!("{left} / {right}");
        pad_center(&cell, lay.width)
    };

    // Header row
    {
        let mut line = " ".repeat(label_w);
        for (idx, w) in windows.iter().enumerate() {
            line.push_str(col_gap);
            line.push_str(&pad_center(&w.spec.label, cols[idx].width));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }

    // Category rows + total
    type RowPicker = (&'static str, fn(&Breakdown) -> LineCounts);
    let fixed_rows: [RowPicker; 4] = [
        ("代码", |b| b.code.total()),
        ("测试", |b| b.test.total()),
        ("文档", |b| b.docs.total()),
        ("合计", |b| b.total()),
    ];
    for (label, pick) in fixed_rows {
        let mut line = pad_right(label, label_w);
        for (idx, w) in windows.iter().enumerate() {
            line.push_str(col_gap);
            line.push_str(&format_pair_cell(pick(&w.breakdown), &cols[idx]));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }

    // Optional per-repo rows right after 合计
    if by_repo {
        for rlabel in repo_order {
            let mut line = pad_right(&format!("[{rlabel}]"), label_w);
            for (idx, w) in windows.iter().enumerate() {
                let c = w
                    .by_repo
                    .iter()
                    .find(|r| &r.label == rlabel)
                    .map(|r| r.breakdown.total())
                    .unwrap_or_default();
                line.push_str(col_gap);
                line.push_str(&format_pair_cell(c, &cols[idx]));
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }

    // 委派占比
    {
        let mut line = pad_right("委派占比", label_w);
        for (idx, w) in windows.iter().enumerate() {
            line.push_str(col_gap);
            line.push_str(&pad_center(&format_pct(w.delegated_share_pct), cols[idx].width));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }

    // 每百万 token
    {
        let mut line = pad_right("每百万 token", label_w);
        for (idx, w) in windows.iter().enumerate() {
            line.push_str(col_gap);
            line.push_str(&pad_center(
                &format_lines_per_m(w.lines_per_million_tokens),
                cols[idx].width,
            ));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }

    for err in errors {
        out.push_str(&format!("（{} 读取失败：{}）\n", err.label, err.error));
    }

    out
}

fn format_pct(pct: Option<f64>) -> String {
    match pct {
        Some(v) => format!("{}%", v.round() as u64),
        None => "—".into(),
    }
}

fn format_lines_per_m(v: Option<f64>) -> String {
    match v {
        Some(val) => format!("{} 行", format_commas(val.round() as u64)),
        None => "—".into(),
    }
}

fn render_json(
    now_ep: i64,
    windows: &[WindowResult],
    daily_7d: &[DailyPoint],
    errors: &[RepoError],
) -> Value {
    let windows_json: Vec<Value> = windows
        .iter()
        .map(|w| {
            let repos_json: Vec<Value> = w
                .by_repo
                .iter()
                .map(|r| {
                    json!({
                        "repo": r.label,
                        "path": r.path.display().to_string(),
                        "head": r.head,
                        "total": r.breakdown.total().to_json(),
                        "byCategory": r.breakdown.by_category_json(),
                        "byAttribution": r.breakdown.by_attribution_json(),
                    })
                })
                .collect();
            json!({
                "key": w.spec.key,
                "label": w.spec.label,
                "sinceSeconds": w.spec.since_secs,
                "sinceEpoch": w.spec.since_epoch,
                "total": w.breakdown.total().to_json(),
                "byCategory": w.breakdown.by_category_json(),
                "byAttribution": w.breakdown.by_attribution_json(),
                "delegatedSharePct": w.delegated_share_pct.map(round2),
                "delegatedRuns": w.delegated_runs.iter().cloned().collect::<Vec<_>>(),
                "chainRuns": w.chain_runs.iter().cloned().collect::<Vec<_>>(),
                "tokens": w.tokens.to_json_with_total(),
                "linesPerMillionTokens": w.lines_per_million_tokens.map(round2),
                "byRepo": repos_json,
            })
        })
        .collect();
    let daily_json: Vec<Value> = daily_7d
        .iter()
        .map(|d| {
            json!({
                "date": d.date,
                "added": d.counts.added,
                "deleted": d.counts.deleted,
                "churn": d.counts.churn(),
            })
        })
        .collect();
    let errors_json: Vec<Value> = errors
        .iter()
        .map(|e| {
            json!({
                "repo": e.label,
                "path": e.path.display().to_string(),
                "error": e.error,
            })
        })
        .collect();
    json!({
        "generatedAt": format_epoch(now_ep, false, "%Y-%m-%dT%H:%M:%SZ"),
        "generatedEpoch": now_ep,
        "windows": windows_json,
        "daily7d": daily_json,
        "errors": errors_json,
    })
}

pub fn command(args: &[String]) -> Res<i32> {
    let (pos, flags, kv) = crate::parse_simple(args, &["--json"], &["--since", "--by"])?;
    if let Some(extra) = pos.first() {
        return Err(format!("unrecognized arguments: {extra}"));
    }
    let json_mode = crate::has(&flags, "--json");
    let by_repo = match crate::value(&kv, "--by") {
        None => false,
        Some("repo") => true,
        Some(other) => {
            return Err(format!("invalid --by value: {other:?} (expected: repo)"));
        }
    };

    let now_ep = epoch() as i64;
    let specs: Vec<WindowSpec> = if let Some(since_raw) = crate::value(&kv, "--since") {
        let secs = seconds(since_raw)?.round() as i64;
        if secs <= 0 {
            return Err(format!("invalid positive duration: {since_raw}"));
        }
        vec![WindowSpec {
            key: since_raw.to_string(),
            label: format_window_label(since_raw, secs),
            since_secs: secs,
            since_epoch: now_ep.saturating_sub(secs),
        }]
    } else {
        vec![
            WindowSpec {
                key: "24h".into(),
                label: "24 小时".into(),
                since_secs: DAY_SECS,
                since_epoch: now_ep.saturating_sub(DAY_SECS),
            },
            WindowSpec {
                key: "7d".into(),
                label: "7 天".into(),
                since_secs: SEVEN_DAYS_SECS,
                since_epoch: now_ep.saturating_sub(SEVEN_DAYS_SECS),
            },
        ]
    };

    let state = state_dir();
    let discovered = crate::index::discover_snapshot_repos(&state, &[], None);
    let mut runs_dirs: Vec<PathBuf> = Vec::new();
    let mut dedup_repos: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut seen_repos: HashSet<PathBuf> = HashSet::new();
    for (repo, runs_dir) in discovered {
        if !runs_dirs.contains(&runs_dir) {
            runs_dirs.push(runs_dir.clone());
        }
        if seen_repos.insert(repo.clone()) {
            dedup_repos.push((repo, runs_dir));
        }
    }

    let labels = assign_repo_labels(&dedup_repos);

    // Build 7 local calendar days ending today for `daily7d`.
    let mut daily_days: Vec<String> = Vec::with_capacity(7);
    for d in (0..7).rev() {
        let ymd = format_epoch(now_ep - d * DAY_SECS, true, "%Y-%m-%d");
        if daily_days.last().is_none_or(|prev| prev != &ymd) {
            daily_days.push(ymd);
        }
    }
    let day_index: HashMap<String, usize> = daily_days
        .iter()
        .enumerate()
        .map(|(i, ymd)| (ymd.clone(), i))
        .collect();

    // Earliest timestamp we need commits from (covers all requested windows + full 7 local days).
    let earliest_needed = specs
        .iter()
        .map(|sp| sp.since_epoch)
        .min()
        .unwrap_or(now_ep - SEVEN_DAYS_SECS)
        .min(now_ep - SEVEN_DAYS_SECS);

    let mut cache = load_cache(&state);
    let mut cache_dirty = false;
    let mut repos_data: Vec<RepoData> = Vec::new();
    let mut errors: Vec<RepoError> = Vec::new();

    for ((repo, _), label) in dedup_repos.into_iter().zip(labels) {
        // Repo config validation fails closed and names the invalid field.
        let exclude_rules = load_repo_exclude_rules(&repo)?;

        let head_opt = match resolve_default_branch_head(&repo) {
            Ok(h) => h,
            Err(err) => {
                errors.push(RepoError {
                    label,
                    path: repo,
                    error: err,
                });
                continue;
            }
        };
        let Some(head_sha) = head_opt else {
            // Empty git repo with 0 commits.
            repos_data.push(RepoData {
                label,
                path: repo,
                head: String::new(),
                commits: Vec::new(),
            });
            continue;
        };

        let cache_key = repo.display().to_string();
        let cached_hit = cache.get(&cache_key).filter(|ent| {
            ent.head == head_sha
                && ent.exclude_rules == exclude_rules
                && ent.covered_since <= earliest_needed
        });
        let commits = if let Some(ent) = cached_hit {
            ent.commits.clone()
        } else {
            match fetch_repo_commits_git(&repo, &head_sha, earliest_needed, &exclude_rules) {
                Ok(list) => {
                    cache.insert(
                        cache_key,
                        RepoCacheEntry {
                            head: head_sha.clone(),
                            exclude_rules,
                            covered_since: earliest_needed,
                            commits: list.clone(),
                        },
                    );
                    cache_dirty = true;
                    list
                }
                Err(err) => {
                    errors.push(RepoError {
                        label,
                        path: repo,
                        error: err,
                    });
                    continue;
                }
            }
        };

        repos_data.push(RepoData {
            label,
            path: repo,
            head: head_sha,
            commits,
        });
    }

    if cache_dirty {
        save_cache(&state, &cache);
    }

    // Collect daily 7d totals across all successfully read repos.
    let mut daily_counts = vec![LineCounts::default(); daily_days.len()];
    for rd in &repos_data {
        for c in &rd.commits {
            let ymd = format_epoch(c.ct, true, "%Y-%m-%d");
            if let Some(&idx) = day_index.get(&ymd) {
                daily_counts[idx].add(c.total());
            }
        }
    }
    let daily_7d: Vec<DailyPoint> = daily_days
        .into_iter()
        .zip(daily_counts)
        .map(|(date, counts)| DailyPoint { date, counts })
        .collect();

    // Determine whether any window has delegated runs so we only load token/chain indexes when needed.
    let any_delegated = repos_data.iter().any(|rd| {
        rd.commits
            .iter()
            .any(|c| c.ct >= earliest_needed && c.is_delegated())
    });
    let (chain_index, tokens_by_run) = if any_delegated {
        (
            Some(ChainIndex::load(&state, &runs_dirs)),
            Some(usage::all_run_tokens_map()),
        )
    } else {
        (None, None)
    };

    // Aggregate per window.
    let mut results: Vec<WindowResult> = Vec::with_capacity(specs.len());
    for spec in specs {
        let mut total_bd = Breakdown::default();
        let mut delegated_runs: BTreeSet<String> = BTreeSet::new();
        let mut by_repo_vec: Vec<RepoWindowResult> = Vec::with_capacity(repos_data.len());

        for rd in &repos_data {
            let mut rbd = Breakdown::default();
            for c in &rd.commits {
                if c.ct < spec.since_epoch {
                    continue;
                }
                if c.is_delegated() {
                    rbd.code.delegated.add(c.code);
                    rbd.test.delegated.add(c.test);
                    rbd.docs.delegated.add(c.docs);
                    for run_id in &c.runs {
                        delegated_runs.insert(run_id.clone());
                    }
                } else {
                    rbd.code.other.add(c.code);
                    rbd.test.other.add(c.test);
                    rbd.docs.other.add(c.docs);
                }
            }
            total_bd.add(rbd);
            by_repo_vec.push(RepoWindowResult {
                label: rd.label.clone(),
                path: rd.path.clone(),
                head: rd.head.clone(),
                breakdown: rbd,
            });
        }

        let total_churn = total_bd.total().churn();
        let delegated_churn = total_bd.delegated().churn();
        let delegated_share_pct = if total_churn > 0 {
            Some((delegated_churn as f64) * 100.0 / (total_churn as f64))
        } else {
            None
        };

        let chain_runs = if !delegated_runs.is_empty() {
            chain_index
                .as_ref()
                .map(|ci| ci.expand_chains(&delegated_runs))
                .unwrap_or_else(|| delegated_runs.clone())
        } else {
            BTreeSet::new()
        };

        let mut tokens = TokenCounts::default();
        if let (Some(tmap), Some(ci)) = (tokens_by_run.as_ref(), chain_index.as_ref()) {
            for rid in &chain_runs {
                if let Some(t) = tmap.get(rid) {
                    tokens.add(*t);
                } else if let Some(ht) = ci.history_tokens.get(rid) {
                    tokens.add(*ht);
                }
            }
        }
        let total_tokens = tokens.total();
        let lines_per_million_tokens = if total_tokens > 0 {
            Some((delegated_churn as f64) * 1_000_000.0 / (total_tokens as f64))
        } else {
            None
        };

        results.push(WindowResult {
            spec,
            breakdown: total_bd,
            delegated_share_pct,
            delegated_runs,
            chain_runs,
            tokens,
            lines_per_million_tokens,
            by_repo: by_repo_vec,
        });
    }

    // Sort repos descending by the primary window's churn (7d when 2 columns, or the single --since window).
    let sort_win_idx = results.len().saturating_sub(1);
    let mut repo_sort_keys: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for rd in &repos_data {
        let primary = results
            .get(sort_win_idx)
            .and_then(|w| w.by_repo.iter().find(|r| r.label == rd.label))
            .map(|r| r.breakdown.total().churn())
            .unwrap_or(0);
        let secondary = results
            .first()
            .and_then(|w| w.by_repo.iter().find(|r| r.label == rd.label))
            .map(|r| r.breakdown.total().churn())
            .unwrap_or(0);
        repo_sort_keys.insert(rd.label.clone(), (primary, secondary));
    }
    let mut repo_order: Vec<String> = repos_data.iter().map(|r| r.label.clone()).collect();
    repo_order.sort_by(|a, b| {
        let ka = repo_sort_keys.get(a).copied().unwrap_or((0, 0));
        let kb = repo_sort_keys.get(b).copied().unwrap_or((0, 0));
        kb.0.cmp(&ka.0).then_with(|| kb.1.cmp(&ka.1)).then_with(|| a.cmp(b))
    });
    for w in &mut results {
        w.by_repo.sort_by(|a, b| {
            let ka = repo_sort_keys.get(&a.label).copied().unwrap_or((0, 0));
            let kb = repo_sort_keys.get(&b.label).copied().unwrap_or((0, 0));
            kb.0.cmp(&ka.0)
                .then_with(|| kb.1.cmp(&ka.1))
                .then_with(|| a.label.cmp(&b.label))
        });
    }

    if json_mode {
        println!("{}", render_json(now_ep, &results, &daily_7d, &errors));
    } else {
        print!("{}", render_human(&results, by_repo, &repo_order, &errors));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_excludes_and_negation_reinclude() {
        assert!(is_excluded("Cargo.lock", &[]));
        assert!(is_excluded("crates/foo/Cargo.lock", &[]));
        assert!(is_excluded("pnpm-lock.yaml", &[]));
        assert!(is_excluded("package-lock.json", &[]));
        assert!(is_excluded("yarn.lock", &[]));
        assert!(is_excluded("generated/schema.rs", &[]));
        assert!(is_excluded("src/generated/a/b.ts", &[]));
        assert!(is_excluded("ui/__snapshots__/card.snap", &[]));
        assert!(is_excluded("tests/out.snap", &[]));
        assert!(is_excluded("docs/api.golden.md", &[]));
        assert!(!is_excluded("src/main.rs", &[]));

        // Negation re-includes a default-excluded file; later positive re-excludes it.
        let rules = vec![
            "vendor/**".to_string(),
            "!vendor/keep.rs".to_string(),
            "!**/generated/hand.rs".to_string(),
        ];
        assert!(is_excluded("vendor/lib.rs", &rules));
        assert!(!is_excluded("vendor/keep.rs", &rules));
        assert!(!is_excluded("src/generated/hand.rs", &rules));
        assert!(is_excluded("src/generated/auto.rs", &rules));

        // Later rule overrides earlier negation.
        let rules2 = vec![
            "!Cargo.lock".to_string(),
            "Cargo.lock".to_string(),
        ];
        assert!(is_excluded("Cargo.lock", &rules2));
    }

    #[test]
    fn classify_paths_correctly() {
        assert_eq!(classify_path("tests/unit.rs"), LineCategory::Test);
        assert_eq!(classify_path("tests/README.md"), LineCategory::Test);
        assert_eq!(classify_path("crates/a/tests/foo.rs"), LineCategory::Test);
        assert_eq!(classify_path("crates/a/tests/guide.md"), LineCategory::Test);
        assert_eq!(classify_path("src/foo_test.rs"), LineCategory::Test);
        assert_eq!(classify_path("src/foo_tests.rs"), LineCategory::Test);
        assert_eq!(classify_path("web/app.test.ts"), LineCategory::Test);
        assert_eq!(classify_path("web/btn.test.tsx"), LineCategory::Test);
        assert_eq!(classify_path("web/api.spec.ts"), LineCategory::Test);
        assert_eq!(classify_path("scripts/test_lines.py"), LineCategory::Test);
        assert_eq!(classify_path("test_root.py"), LineCategory::Test);

        assert_eq!(classify_path("README.md"), LineCategory::Docs);
        assert_eq!(classify_path("docs/guide.md"), LineCategory::Docs);

        assert_eq!(classify_path("src/main.rs"), LineCategory::Code);
        assert_eq!(classify_path("scripts/verify.sh"), LineCategory::Code);
    }

    #[test]
    fn validate_line_stats_fails_closed() {
        assert!(validate_line_stats_config(None).is_ok());
        assert!(validate_line_stats_config(Some(&json!(null))).is_ok());
        assert!(validate_line_stats_config(Some(&json!({"exclude": ["vendor/**", "!vendor/a.rs"]}))).is_ok());

        let err = validate_line_stats_config(Some(&json!("bad"))).unwrap_err();
        assert!(err.contains("lineStats"), "{err}");

        let err = validate_line_stats_config(Some(&json!({"unknownKey": []}))).unwrap_err();
        assert!(err.contains("unknownKey"), "{err}");

        let err = validate_line_stats_config(Some(&json!({"exclude": "not-array"}))).unwrap_err();
        assert!(err.contains("lineStats.exclude"), "{err}");

        let err = validate_line_stats_config(Some(&json!({"exclude": [123]}))).unwrap_err();
        assert!(err.contains("lineStats.exclude[0]"), "{err}");

        let err = validate_line_stats_config(Some(&json!({"exclude": ["!"]}))).unwrap_err();
        assert!(err.contains("lineStats.exclude[0]"), "{err}");
    }

    #[test]
    fn extract_trailers_handles_single_and_multiple() {
        let msg = "feat: add lines\n\nBody text.\n\nDelegate-Run: 20261126-100000-a\nDelegate-Run: 20261126-100000-b\n";
        assert_eq!(
            extract_delegate_run_trailers(msg),
            vec!["20261126-100000-a", "20261126-100000-b"]
        );
        assert!(extract_delegate_run_trailers("chore: normal commit\n").is_empty());
    }

    #[test]
    fn parse_numstat_z_handles_renames_binaries_and_excludes() {
        let raw = b"\x1eaaaa\x1f1700000000\x1fcommit msg\n\nDelegate-Run: run-1\n\x1e\x00\n-\t-\timg.png\x001\t2\t\x00old/a.rs\x00new/a.rs\x0010\t0\tCargo.lock\x005\t1\ttests/t.rs\x003\t2\tREADME.md\x00";
        let commits = parse_git_log_numstat_z(raw, &[]);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].ct, 1700000000);
        assert_eq!(commits[0].runs, vec!["run-1"]);
        assert_eq!(commits[0].code, LineCounts { added: 1, deleted: 2 });
        assert_eq!(commits[0].test, LineCounts { added: 5, deleted: 1 });
        assert_eq!(commits[0].docs, LineCounts { added: 3, deleted: 2 });
    }

    #[test]
    fn format_commas_and_human_output() {
        assert_eq!(format_commas(0), "0");
        assert_eq!(format_commas(999), "999");
        assert_eq!(format_commas(1204), "1,204");
        assert_eq!(format_commas(14808), "14,808");
        assert_eq!(format_commas(1000000), "1,000,000");

        let mut b24 = Breakdown::default();
        b24.code.delegated = LineCounts { added: 1204, deleted: 310 };
        b24.test.delegated = LineCounts { added: 640, deleted: 88 };
        b24.docs.other = LineCounts { added: 96, deleted: 40 };
        let w24 = WindowResult {
            spec: WindowSpec {
                key: "24h".into(),
                label: "24 小时".into(),
                since_secs: DAY_SECS,
                since_epoch: 0,
            },
            breakdown: b24,
            delegated_share_pct: Some(82.0),
            delegated_runs: BTreeSet::new(),
            chain_runs: BTreeSet::new(),
            tokens: TokenCounts::default(),
            lines_per_million_tokens: Some(35.0),
            by_repo: vec![],
        };
        let rendered = render_human(&[w24], false, &[], &[]);
        assert!(rendered.starts_with("代码改动（各仓库默认分支，不含合并提交与生成文件）\n"));
        assert!(rendered.contains("+1,204 / \u{2212}310"), "{rendered}");
        assert!(rendered.contains("委派占比"), "{rendered}");
        assert!(rendered.contains("82%"), "{rendered}");
        assert!(rendered.contains("每百万 token"), "{rendered}");
        assert!(rendered.contains("35 行"), "{rendered}");
    }
}
