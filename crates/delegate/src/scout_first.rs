//! Two-step scout-first orchestration helpers for write tasks:
//! verbatim prose scout prompt & writer appendix templates, loose path/line scanning
//! against the scout's own snapshot tree (with git blob SHA hashing), factual footnotes
//! for missing or baseline-changed paths, natural-language scout question detection,
//! lightweight brief detection, and `preEditSteps` counting.
use crate::brief_check::{self, CheckReport, PathMention};
use crate::common::{git_command, s};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

pub const SCOUT_PROMPT_TEMPLATE: &str = "\
# 只读探路：为接手改代码的同事写一份交接笔记

另一位同事将在新会话中完成下面的任务，看不到你读过的过程，只看你的回复。请只读查清现状，用自然语言把下面几件事交代清楚（可用几个简短小标题或列表，不必套固定格式，控制在 1500 字内）：
- 要改哪些文件、在哪个函数或大致行段、各处为什么要动；
- 相关的上下游调用、可照搬的现成写法、最快验证改动的测试命令、容易踩的坑（例如需同步改的旧测试）；
- 如果发现任务说明的前提与代码实际矛盾、写手没法替主控拿主意，在开头直接向主控提问说明矛盾与你的倾向，并附上已查清的事实。

## 任务说明
{{brief}}";

pub const WRITER_APPENDIX_HEADER: &str = "\n\n## 探路交接笔记\n以下是只读探路同事对现状的查证笔记（与上文任务说明冲突时以上文为准）：\n\n";

pub const LIGHTWEIGHT_MAX_CHARS: usize = 2000;

pub fn build_scout_prompt(raw_brief: &str) -> String {
    SCOUT_PROMPT_TEMPLATE.replace("{{brief}}", raw_brief)
}

/// Build the writer prompt combining the original brief, the prose scout handoff note
/// (which may already end with a factual missing-paths footnote), and an optional controller reply.
/// Returns `(composed_prompt, map_end_byte_offset)` where `map_end_byte_offset` points to the exact
/// end of the scout handoff note (before any controller reply or contract separator).
pub fn build_writer_brief_with_end(
    raw_brief: &str,
    note: &str,
    controller_reply: Option<&str>,
) -> (String, usize) {
    let mut out = format!(
        "{}{}{}",
        raw_brief.trim_end(),
        WRITER_APPENDIX_HEADER,
        note.trim()
    );
    let map_end = out.len();
    if let Some(reply) = controller_reply.map(str::trim).filter(|r| !r.is_empty()) {
        out.push_str(&format!("\n\n## 主控对探路发现的答复\n\n{reply}"));
    }
    (out, map_end)
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

/// Compute git blob SHA-1 (`sha1("blob <len>\0<bytes>")`) deterministically without spawning git.
pub fn git_blob_sha1(bytes: &[u8]) -> String {
    let mut hasher = sha1_smol::Sha1::new();
    let header = format!("blob {}\0", bytes.len());
    hasher.update(header.as_bytes());
    hasher.update(bytes);
    hasher.digest().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeEntry {
    File(String),
    Dir,
}

/// Snapshot tree to inspect mentioned paths and compute/compare git blob SHAs against.
pub enum TreeSource<'a> {
    /// Read directly from a worktree/directory on disk (fast, zero subprocesses).
    Dir(&'a Path),
    /// Read from a git tree object (`meta.base.tree`) in `repo`.
    GitTree { repo: &'a Path, tree: &'a str },
}

impl<'a> TreeSource<'a> {
    /// Inspect the given repo-relative paths and return existing entries with file blob SHAs.
    pub fn inspect_paths(&self, paths: &[String]) -> BTreeMap<String, TreeEntry> {
        let mut found = BTreeMap::new();
        if paths.is_empty() {
            return found;
        }
        match self {
            TreeSource::Dir(root) => {
                for p in paths {
                    if p.starts_with('/') || p.split('/').any(|c| c == "..") {
                        continue;
                    }
                    let full = root.join(p);
                    if full.is_file() {
                        if let Ok(bytes) = fs::read(&full) {
                            found.insert(p.clone(), TreeEntry::File(git_blob_sha1(&bytes)));
                        }
                    } else if full.is_dir() || full.exists() {
                        found.insert(p.clone(), TreeEntry::Dir);
                    }
                }
            }
            TreeSource::GitTree { repo, tree } => {
                // Normalize trailing slashes so `git ls-tree` inspects directory entries themselves.
                let mut norm_to_orig: BTreeMap<String, Vec<String>> = BTreeMap::new();
                let mut query: Vec<String> = Vec::new();
                for p in paths {
                    if p.starts_with('/') || p.split('/').any(|c| c == "..") {
                        continue;
                    }
                    let norm = p.trim_end_matches('/').to_string();
                    if norm.is_empty() {
                        continue;
                    }
                    let entry = norm_to_orig.entry(norm.clone()).or_default();
                    if entry.is_empty() {
                        query.push(norm);
                    }
                    if !entry.contains(p) {
                        entry.push(p.clone());
                    }
                }
                for chunk in query.chunks(200) {
                    let mut cmd = git_command();
                    cmd.arg("-C").arg(repo).args(["ls-tree", "-z", tree, "--"]);
                    for item in chunk {
                        cmd.arg(item);
                    }
                    let Ok(out) = cmd.output() else {
                        continue;
                    };
                    if !out.status.success() {
                        continue;
                    }
                    for rec in out.stdout.split(|&b| b == 0) {
                        if rec.is_empty() {
                            continue;
                        }
                        let rec_str = String::from_utf8_lossy(rec);
                        let Some((meta_part, path_part)) = rec_str.split_once('\t') else {
                            continue;
                        };
                        let mut cols = meta_part.split_whitespace();
                        let _mode = cols.next();
                        let Some(kind) = cols.next() else {
                            continue;
                        };
                        let Some(sha) = cols.next() else {
                            continue;
                        };
                        let tree_entry = if kind == "blob" {
                            TreeEntry::File(sha.to_string())
                        } else {
                            TreeEntry::Dir
                        };
                        if let Some(origs) = norm_to_orig.get(path_part) {
                            for orig in origs {
                                found.insert(orig.clone(), tree_entry.clone());
                            }
                        }
                    }
                }
            }
        }
        found
    }

    /// Compare scout-recorded blob SHAs against this tree and return sorted paths whose content
    /// changed (or disappeared) after scouting.
    pub fn diff_blobs(&self, scout_blobs: &BTreeMap<String, String>) -> Vec<String> {
        if scout_blobs.is_empty() {
            return Vec::new();
        }
        let keys: Vec<String> = scout_blobs.keys().cloned().collect();
        let current = self.inspect_paths(&keys);
        let mut changed = Vec::new();
        for (path, scout_sha) in scout_blobs {
            match current.get(path) {
                Some(TreeEntry::File(cur_sha)) if cur_sha == scout_sha => {}
                _ => changed.push(path.clone()),
            }
        }
        changed.sort();
        changed
    }
}

pub fn format_missing_paths_note(missing: &[String]) -> Option<String> {
    if missing.is_empty() {
        return None;
    }
    const MAX_LIST: usize = 20;
    let body = if missing.len() <= MAX_LIST {
        missing.join("、")
    } else {
        format!(
            "{} 等共 {} 个",
            missing[..MAX_LIST].join("、"),
            missing.len()
        )
    };
    Some(format!(
        "注：探路提及的以下路径在基线中尚不存在（可能是待新建文件或路径笔误）：{body}"
    ))
}

pub fn format_changed_paths_note(changed: &[String]) -> Option<String> {
    if changed.is_empty() {
        return None;
    }
    const MAX_LIST: usize = 20;
    let body = if changed.len() <= MAX_LIST {
        changed.join("、")
    } else {
        format!(
            "{} 等共 {} 个",
            changed[..MAX_LIST].join("、"),
            changed.len()
        )
    };
    Some(format!(
        "注：探路后以下文件在当前基线中已变动，行号可能偏移：{body}"
    ))
}

/// Insert a factual note line at `map_end` (the exact end of the scout handoff note in `prompt.md`).
/// Returns `(updated_prompt, inserted_bytes)`. Idempotent if `note` is already present.
pub fn insert_note_at(prompt_md: &str, map_end: Option<usize>, note: &str) -> (String, usize) {
    let trimmed_note = note.trim();
    if trimmed_note.is_empty() || prompt_md.contains(trimmed_note) {
        return (prompt_md.to_string(), 0);
    }
    let pos = map_end
        .filter(|&idx| idx <= prompt_md.len() && prompt_md.is_char_boundary(idx))
        .or_else(|| prompt_md.find("\n\n## 主控对探路发现的答复"))
        .unwrap_or_else(|| prompt_md.trim_end_matches('\n').len());
    let insertion = format!("\n\n{trimmed_note}");
    let mut out = String::with_capacity(prompt_md.len() + insertion.len());
    out.push_str(&prompt_md[..pos]);
    out.push_str(&insertion);
    out.push_str(&prompt_md[pos..]);
    (out, insertion.len())
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

fn contains_positive_phrase(text: &str, phrase: &str) -> bool {
    let mut search_from = 0;
    while let Some(rel) = text[search_from..].find(phrase) {
        let idx = search_from + rel;
        let prev_char = text[..idx].chars().next_back();
        if !matches!(prev_char, Some('无' | '不' | '未' | '非' | '没')) {
            return true;
        }
        search_from = idx + phrase.len();
    }
    false
}

fn has_controller_question_phrase(line: &str) -> bool {
    let stripped = line
        .trim()
        .trim_matches(['#', '*', '>', '-', '`', '[', ']', '【', '】', ' ']);
    if stripped.is_empty() {
        return false;
    }
    const ZH_PHRASES: &[&str] = &[
        "需要主控",
        "需主控",
        "请主控",
        "待主控",
        "向主控提问",
        "请拍板",
        "待拍板",
        "请确认",
        "需要确认",
        "需确认",
        "请决定",
        "待决定",
        "开工前确认",
        "开工前请",
    ];
    if ZH_PHRASES
        .iter()
        .any(|p| contains_positive_phrase(stripped, p))
    {
        return true;
    }
    let lower = stripped.to_ascii_lowercase();
    const EN_PHRASES: &[&str] = &[
        "needs a decision",
        "need a decision",
        "needs controller",
        "need controller",
        "please confirm",
        "please decide",
        "controller decision",
    ];
    EN_PHRASES.iter().any(|p| {
        if let Some(idx) = lower.find(p) {
            let prefix = lower[..idx].trim_end();
            !(prefix.ends_with("no") || prefix.ends_with("not") || prefix.ends_with("don't"))
        } else {
            false
        }
    })
}

fn is_non_heading_question_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return false;
    }
    let body = trimmed
        .trim_end_matches(['”', '"', '\'', '」', '』', ')', '）', '*', '`'])
        .trim_end();
    if !(body.ends_with('？') || body.ends_with('?')) {
        return false;
    }
    // Exclude trailing URL query strings like `https://example.com/?`
    let last_token = body.split_whitespace().next_back().unwrap_or(body);
    !(last_token.contains("://") && body.ends_with('?'))
}

/// Natural-language detection of whether a read-only scout is asking the controller to decide a
/// brief-vs-code contradiction. Because scout is strictly read-only (zero file edits), we can use
/// relaxed natural-language recognition on its opening and closing paragraphs.
pub fn is_scout_question(raw_map: &str) -> bool {
    let mut fence: Option<(char, usize)> = None;
    let mut paragraphs: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();

    for line in raw_map.lines() {
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
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "---" || trimmed == "***" {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
        } else {
            current.push(trimmed);
        }
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    if paragraphs.is_empty() {
        return false;
    }

    // Opening window: first paragraph (and if the first paragraph is just a heading, also the second),
    // capped to 6 lines total so we only inspect the opening block.
    let mut opening: Vec<&str> = Vec::new();
    for (p_idx, para) in paragraphs.iter().enumerate() {
        if p_idx > 0 && opening.iter().any(|l| !l.starts_with('#')) {
            break;
        }
        for &line in para {
            if p_idx > 0 && line.starts_with('#') && !opening.is_empty() {
                break;
            }
            opening.push(line);
            if opening.len() >= 6 {
                break;
            }
        }
        if opening.len() >= 6 {
            break;
        }
    }

    if opening
        .iter()
        .any(|l| has_controller_question_phrase(l) || is_non_heading_question_line(l))
    {
        return true;
    }

    // Closing paragraph: check last paragraph if distinct from opening.
    if paragraphs.len() > 1 {
        if let Some(last) = paragraphs.last() {
            if last
                .iter()
                .any(|l| has_controller_question_phrase(l) || is_non_heading_question_line(l))
            {
                return true;
            }
        }
    }

    false
}

/// Returns true if the task brief itself names at least one existing path in `tree`.
pub fn brief_has_existing_paths(
    raw_brief: &str,
    tree: &TreeSource<'_>,
    strip_roots: &[&Path],
) -> bool {
    let mentions = brief_check::scan_path_mentions(raw_brief, strip_roots);
    if mentions.is_empty() {
        return false;
    }
    let paths: Vec<String> = mentions.into_iter().map(|m| m.path).collect();
    !tree.inspect_paths(&paths).is_empty()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MapCheck {
    pub mentions: Vec<PathMention>,
    pub existing_paths: Vec<String>,
    pub missing_paths: Vec<String>,
    pub blobs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapOutcome {
    Accepted {
        note: String,
        is_question: bool,
        check: MapCheck,
    },
    Dropped {
        code: &'static str,
        reason: String,
        details: Vec<String>,
        blobs: BTreeMap<String, String>,
    },
}

fn is_suffix_of_any(candidate: &str, existing: &[String]) -> bool {
    let clean = candidate.trim_end_matches('/');
    existing.iter().any(|ex| {
        let ex_clean = ex.trim_end_matches('/');
        ex_clean == clean
            || ex_clean
                .strip_suffix(clean)
                .is_some_and(|prefix| prefix.ends_with('/'))
    })
}

/// Loosely evaluate a prose scout handoff note against the scout's own snapshot tree.
///
/// Never drops on formatting or missing files alone; only drops when:
/// 1. Output is empty or a content-policy refusal (no usable text);
/// 2. Extreme wrong-repo guard: >= 5 repo-style paths mentioned, 0 exist in the snapshot tree,
///    AND the task brief itself named at least one existing path.
pub fn evaluate_map(
    raw_map: &str,
    tree: &TreeSource<'_>,
    strip_roots: &[&Path],
    brief_has_existing: bool,
) -> MapOutcome {
    if raw_map.trim().is_empty() {
        return MapOutcome::Dropped {
            code: "empty_map",
            reason: "探路输出为空".to_string(),
            details: Vec::new(),
            blobs: BTreeMap::new(),
        };
    }
    if crate::agy::is_policy_refusal(raw_map) {
        return MapOutcome::Dropped {
            code: "policy_refusal",
            reason: "探路被内容策略拦截".to_string(),
            details: Vec::new(),
            blobs: BTreeMap::new(),
        };
    }

    let mentions = brief_check::scan_path_mentions(raw_map, strip_roots);
    let paths: Vec<String> = mentions.iter().map(|m| m.path.clone()).collect();
    let inspected = tree.inspect_paths(&paths);

    let mut existing_paths = Vec::new();
    let mut blobs = BTreeMap::new();
    for p in &paths {
        if let Some(entry) = inspected.get(p) {
            existing_paths.push(p.clone());
            if let TreeEntry::File(sha) = entry {
                blobs.insert(p.clone(), sha.clone());
            }
        }
    }

    let mut missing_paths = Vec::new();
    for p in &paths {
        if inspected.contains_key(p) {
            continue;
        }
        // Only treat repo-style file paths (with `/` and a recognized extension, not a shorthand
        // suffix of an already-found existing path) as missing baseline files.
        if !p.contains('/') || !brief_check::has_known_file_extension(p) {
            continue;
        }
        if is_suffix_of_any(p, &existing_paths) {
            continue;
        }
        missing_paths.push(p.clone());
    }

    if existing_paths.is_empty() && missing_paths.len() >= 5 && brief_has_existing {
        let preview = missing_paths
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join("、");
        return MapOutcome::Dropped {
            code: "wrong_repo_paths",
            reason: format!(
                "探路提及 {} 个仓内样式路径但基线中现存 0 个（任务说明含现存文件）：{preview}",
                missing_paths.len()
            ),
            details: missing_paths,
            blobs,
        };
    }

    let mut note = raw_map.trim().to_string();
    if let Some(footnote) = format_missing_paths_note(&missing_paths) {
        note.push_str("\n\n");
        note.push_str(&footnote);
    }
    let is_question = is_scout_question(raw_map);

    MapOutcome::Accepted {
        note,
        is_question,
        check: MapCheck {
            mentions,
            existing_paths,
            missing_paths,
            blobs,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "delegate-scout-prose-{}-{}-{}",
            tag,
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_files(root: &Path, paths: &[&str]) {
        for rel in paths {
            let full = root.join(rel);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&full, format!("// fixture {rel}\n")).unwrap();
        }
    }

    #[test]
    fn scout_prompt_matches_verbatim_prose_handoff_spec() {
        let prompt = build_scout_prompt("修复某处竞态");
        assert!(prompt.starts_with("# 只读探路：为接手改代码的同事写一份交接笔记\n"));
        assert!(prompt.contains("控制在 1500 字内"));
        assert!(prompt.contains("## 任务说明\n修复某处竞态"));
        assert!(!prompt.contains("## L0"));
        assert!(!prompt.contains("## L1"));
        assert!(!prompt.contains("::"));
    }

    #[test]
    fn real_discard_fixtures_are_all_accepted_with_paths_extracted() {
        let dm_gc = include_str!("../../../tests/fixtures/scout/dm-gc.md");
        let agy_auto = include_str!("../../../tests/fixtures/scout/agy-auto-switch.md");
        let dev_heal = include_str!("../../../tests/fixtures/scout/dev-self-heal.md");
        let ops_login = include_str!("../../../tests/fixtures/scout/ops-auth-provider.md");

        // 1. dm-gc (against aia-skills style repo)
        let dir_dm = temp_dir("dm-gc");
        write_files(
            &dir_dm,
            &[
                "crates/delegate/src/main.rs",
                "crates/delegate/src/gc.rs",
                "crates/delegate/src/runs.rs",
                "crates/delegate/src/work.rs",
                "crates/delegate/src/disk.rs",
                "skills/delegate/SKILL.md",
                "skills/delegate/references/commands.md",
                "skills/delegate/references/model-routing.md",
                "tests/test_delegate.py",
                "CHANGELOG.md",
            ],
        );
        match evaluate_map(dm_gc, &TreeSource::Dir(&dir_dm), &[dir_dm.as_path()], true) {
            MapOutcome::Accepted {
                note,
                is_question,
                check,
            } => {
                assert!(!is_question, "dm-gc should not be flagged as question");
                for expected in [
                    "crates/delegate/src/main.rs",
                    "crates/delegate/src/gc.rs",
                    "crates/delegate/src/runs.rs",
                    "crates/delegate/src/disk.rs",
                    "skills/delegate/SKILL.md",
                    "CHANGELOG.md",
                ] {
                    assert!(
                        check.blobs.contains_key(expected),
                        "dm-gc missing blob for {expected}; got {:?}",
                        check.blobs.keys().collect::<Vec<_>>()
                    );
                }
                assert!(
                    check
                        .missing_paths
                        .contains(&"tests/test_delegate_auto_gc.py".to_string()),
                    "dm-gc should note pending new test file: {:?}",
                    check.missing_paths
                );
                assert!(note.contains("注：探路提及的以下路径在基线中尚不存在"));
                assert!(note.contains("tests/test_delegate_auto_gc.py"));
                // Shorthand `references/commands.md` is a suffix of existing `skills/delegate/references/commands.md`
                assert!(
                    !check
                        .missing_paths
                        .contains(&"references/commands.md".to_string())
                );
            }
            MapOutcome::Dropped { reason, .. } => panic!("dm-gc unexpectedly dropped: {reason}"),
        }
        let _ = fs::remove_dir_all(&dir_dm);

        // 2. agy-auto-switch (Markdown links + file:// + multi-range)
        let dir_agy = temp_dir("agy-auto");
        write_files(
            &dir_agy,
            &[
                "crates/delegate/src/config.rs",
                "crates/delegate/src/agy_accounts.rs",
                "crates/delegate/src/agents.rs",
                "crates/delegate/src/supervise.rs",
                "crates/delegate/src/review.rs",
                "crates/delegate/src/usage.rs",
                "crates/delegate-tui/src/main.rs",
                "crates/delegate-tui/src/model.rs",
                "tests/test_delegate_agy_sticky.py",
                "skills/delegate/references/agy-accounts.md",
                "CHANGELOG.md",
            ],
        );
        match evaluate_map(
            agy_auto,
            &TreeSource::Dir(&dir_agy),
            &[dir_agy.as_path()],
            true,
        ) {
            MapOutcome::Accepted {
                is_question, check, ..
            } => {
                assert!(!is_question, "agy-auto should not be flagged as question");
                for expected in [
                    "crates/delegate/src/config.rs",
                    "crates/delegate/src/agy_accounts.rs",
                    "crates/delegate/src/supervise.rs",
                    "crates/delegate-tui/src/main.rs",
                    "crates/delegate-tui/src/model.rs",
                ] {
                    assert!(
                        check.blobs.contains_key(expected),
                        "agy-auto missing blob for {expected}"
                    );
                }
            }
            MapOutcome::Dropped { reason, .. } => panic!("agy-auto unexpectedly dropped: {reason}"),
        }
        let _ = fs::remove_dir_all(&dir_agy);

        // 3. dev-self-heal (norvane repo)
        let dir_dev = temp_dir("dev-heal");
        write_files(
            &dir_dev,
            &[
                "manage.sh",
                "scripts/run-delegated.sh",
                "apps/aiaxiom-server/src/server/threads/harness_change.rs",
                "apps/aiaxiom-server/src/server/error_log.rs",
                "apps/web/src/lib/devLogin.ts",
                "apps/web/src/pages/Auth.tsx",
                "apps/web/src/pages/home/AccountMenu.tsx",
                "apps/web/src/i18n/zh/Auth.ts",
                "apps/web/src/i18n/en/Auth.ts",
                "docs/reference/dev-maintenance.md",
            ],
        );
        match evaluate_map(
            dev_heal,
            &TreeSource::Dir(&dir_dev),
            &[dir_dev.as_path()],
            true,
        ) {
            MapOutcome::Accepted {
                is_question, check, ..
            } => {
                assert!(!is_question, "dev-heal should not be flagged as question");
                for expected in [
                    "manage.sh",
                    "scripts/run-delegated.sh",
                    "apps/aiaxiom-server/src/server/threads/harness_change.rs",
                    "apps/web/src/pages/Auth.tsx",
                    "docs/reference/dev-maintenance.md",
                ] {
                    assert!(
                        check.blobs.contains_key(expected),
                        "dev-heal missing blob for {expected}"
                    );
                }
            }
            MapOutcome::Dropped { reason, .. } => panic!("dev-heal unexpectedly dropped: {reason}"),
        }
        let _ = fs::remove_dir_all(&dir_dev);

        // 4. ops-auth-provider (norvane repo, backticks + pending new module)
        let dir_ops = temp_dir("ops-login");
        write_files(
            &dir_ops,
            &[
                "apps/aiaxiom-server/src/server/ops.rs",
                "apps/aiaxiom-server/src/server/oauth/mod.rs",
                "crates/control-plane/src/oauth/mod.rs",
                "scripts/alpha.sh",
                "docs/reference/dev-maintenance.md",
            ],
        );
        match evaluate_map(
            ops_login,
            &TreeSource::Dir(&dir_ops),
            &[dir_ops.as_path()],
            true,
        ) {
            MapOutcome::Accepted {
                note,
                is_question,
                check,
            } => {
                assert!(!is_question, "ops-login should not be flagged as question");
                for expected in [
                    "apps/aiaxiom-server/src/server/ops.rs",
                    "crates/control-plane/src/oauth/mod.rs",
                    "scripts/alpha.sh",
                    "docs/reference/dev-maintenance.md",
                ] {
                    assert!(
                        check.blobs.contains_key(expected),
                        "ops-login missing blob for {expected}"
                    );
                }
                assert!(
                    check
                        .missing_paths
                        .contains(&"apps/aiaxiom-server/src/server/ops/auth_provider.rs".to_string())
                );
                assert!(note.contains("apps/aiaxiom-server/src/server/ops/auth_provider.rs"));
            }
            MapOutcome::Dropped { reason, .. } => {
                panic!("ops-login unexpectedly dropped: {reason}")
            }
        }
        let _ = fs::remove_dir_all(&dir_ops);
    }

    #[test]
    fn wrong_repo_drop_requires_5_missing_zero_existing_and_brief_existing() {
        let dir = temp_dir("wrong-repo");
        write_files(&dir, &["src/real.rs"]);
        let map5 = "要改 `other/a.rs`、`other/b.rs`、`other/c.rs`、`other/d.rs` 和 `other/e.rs`。";

        // 5 missing + 0 existing + brief has existing -> DROPPED with concrete details
        match evaluate_map(map5, &TreeSource::Dir(&dir), &[dir.as_path()], true) {
            MapOutcome::Dropped {
                code,
                reason,
                details,
                ..
            } => {
                assert_eq!(code, "wrong_repo_paths");
                assert!(reason.contains("5 个仓内样式路径"));
                assert_eq!(details.len(), 5);
                assert!(details.contains(&"other/a.rs".to_string()));
            }
            MapOutcome::Accepted { .. } => panic!("expected wrong_repo_paths drop"),
        }

        // Brief has NO existing paths -> ACCEPTED (might be a brand-new scaffold task)
        assert!(matches!(
            evaluate_map(map5, &TreeSource::Dir(&dir), &[dir.as_path()], false),
            MapOutcome::Accepted { .. }
        ));

        // Only 4 missing paths -> ACCEPTED with factual footnote
        let map4 = "要改 `other/a.rs`、`other/b.rs`、`other/c.rs`、`other/d.rs`。";
        assert!(matches!(
            evaluate_map(map4, &TreeSource::Dir(&dir), &[dir.as_path()], true),
            MapOutcome::Accepted { .. }
        ));

        // At least 1 existing path -> ACCEPTED even with 5 missing
        let map5_plus_1 = format!("{map5}\n参考 `src/real.rs`。");
        assert!(matches!(
            evaluate_map(&map5_plus_1, &TreeSource::Dir(&dir), &[dir.as_path()], true),
            MapOutcome::Accepted { .. }
        ));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scout_question_detects_natural_questions_and_avoids_false_positives() {
        // Explicit & natural questions
        assert!(is_scout_question("需要主控决定\n说明说改 old_alpha，但代码只有 alpha_fn。"));
        assert!(is_scout_question("## **Needs a decision**\nShould we drop `--org`?"));
        assert!(is_scout_question(
            "开工前请主控确认：auth_providers 表没有 organization_id 列，是否去掉 --org？\n\n### 已查清的事实\n..."
        ));
        assert!(is_scout_question(
            "发现任务前提与现状矛盾：alpha.sh 没有 ops 子命令，倾向改用 alpha server ops，请问按哪种方案推进？\n\n细节如下。"
        ));
        assert!(is_scout_question(
            "### 查证结果\n代码在 `src/alpha.rs` (L1-10)。\n\n综上，请问主控倾向方案 A 还是方案 B？"
        ));

        // Normal handoff notes must NOT trigger question
        assert!(!is_scout_question(
            "### 改动位置\n1. `src/alpha.rs` 的 `alpha_fn` (L1-3)：更新返回值。\n\n### 测试\n运行 `cargo test`。"
        ));
        assert!(!is_scout_question(
            "### 为什么需要改这里？\n因为旧接口会丢掉错误信息。\n\n### 落点\n改 `src/alpha.rs`。"
        ));
        assert!(!is_scout_question(
            "现状清晰，无需主控确认，写手可直接按说明修改 `src/alpha.rs`。"
        ));
        assert!(!is_scout_question(
            "查证完毕，无不符之处。\n\n```md\n需要主控决定（代码块内示例，忽略）\n```\n\n测试命令：`true`。"
        ));
    }

    #[test]
    fn insert_note_at_respects_map_end_even_with_horizontal_rules_inside_map() {
        let (composed, map_end) = build_writer_brief_with_end(
            "任务说明",
            "第一段\n\n---\n\n第二段",
            Some("主控答复内容"),
        );
        let full = format!("{composed}\n\n---\n合同内容\n");
        let changed_note = format_changed_paths_note(&["src/alpha.rs".into()]).unwrap();
        let (updated, delta) = insert_note_at(&full, Some(map_end), &changed_note);
        assert!(delta > 0);
        let idx_second = updated.find("第二段").unwrap();
        let idx_note = updated.find(&changed_note).unwrap();
        let idx_reply = updated.find("## 主控对探路发现的答复").unwrap();
        assert!(idx_second < idx_note && idx_note < idx_reply);
        // Idempotent on second call
        let (updated2, delta2) = insert_note_at(&updated, Some(map_end + delta), &changed_note);
        assert_eq!(delta2, 0);
        assert_eq!(updated2, updated);
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
