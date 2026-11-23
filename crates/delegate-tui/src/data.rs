use crate::model::*;
use crate::width::single_line;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const PRUNE_DIRS: &[&str] = &[".cache", "target", "node_modules", "third_party", ".git"];
const MAX_WALK_DEPTH: usize = 6;

#[derive(Debug, Clone)]
pub struct DelegateClient {
    pub bin: PathBuf,
    lane_json_supported: bool,
    result_no_mark_supported: bool,
}

impl DelegateClient {
    pub fn discover(explicit: Option<PathBuf>) -> Self {
        let bin = resolve_delegate_bin(explicit);
        let lane_json_supported = probe_help_contains(&bin, "lane", "--json");
        let result_no_mark_supported = probe_help_contains(&bin, "result", "--no-mark");
        Self {
            bin,
            lane_json_supported,
            result_no_mark_supported,
        }
    }

    pub fn from_parts(
        bin: PathBuf,
        lane_json_supported: bool,
        result_no_mark_supported: bool,
    ) -> Self {
        Self {
            bin,
            lane_json_supported,
            result_no_mark_supported,
        }
    }

    pub fn fetch_repo_status(&self, repo: &Path, home: Option<&Path>) -> Result<RepoGroup, String> {
        let output = Command::new(&self.bin)
            .args(["status", "--json", "--all", "--files"])
            .current_dir(repo)
            .output()
            .map_err(|e| format!("无法执行 {}: {e}", self.bin.display()))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() && stdout.trim().is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "{}: status 失败 ({})",
                repo_display_label(repo, home),
                single_line(&stderr)
            ));
        }
        let label = repo_display_label(repo, home);
        Ok(parse_status_jsonl(repo, &label, &stdout))
    }

    pub fn fetch_lane(&self) -> Result<LaneStatus, String> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("lane");
        if self.lane_json_supported {
            cmd.arg("--json");
        }
        let output = cmd
            .output()
            .map_err(|e| format!("无法执行 {} lane: {e}", self.bin.display()))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("lane 失败: {}", single_line(&stderr)));
        }
        if self.lane_json_supported {
            parse_lane_json(&stdout).or_else(|_| Ok(parse_lane_text(&stdout)))
        } else {
            Ok(parse_lane_text(&stdout))
        }
    }

    pub fn fetch_result(&self, repo: &Path, run_id: &str) -> Result<String, String> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("result");
        if self.result_no_mark_supported {
            cmd.arg("--no-mark");
        }
        cmd.arg(run_id).current_dir(repo);
        let output = cmd
            .output()
            .map_err(|e| format!("无法执行 result: {e}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(single_line(&stderr))
        }
    }

    pub fn fetch_diff_stat(&self, repo: &Path, run_id: &str) -> Result<String, String> {
        for args in [
            vec!["diff", "--stat", "--total", run_id],
            vec!["diff", "--stat", run_id],
        ] {
            let output = Command::new(&self.bin)
                .args(&args)
                .current_dir(repo)
                .output()
                .map_err(|e| format!("无法执行 diff: {e}"))?;
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout).into_owned();
                if !text.trim().is_empty() || args.len() == 3 {
                    return Ok(text);
                }
            }
        }
        Ok(String::new())
    }

    pub fn fetch_events(&self) -> Vec<EventItem> {
        let Ok(output) = Command::new(&self.bin)
            .args(["events", "--json"])
            .output()
        else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_events_jsonl(&stdout)
    }

    pub fn fetch_dag_queue(&self) -> Vec<DagQueueItem> {
        let Ok(output) = Command::new(&self.bin)
            .args(["queue", "list", "--json"])
            .output()
        else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_dag_queue_jsonl(&stdout)
    }

    pub fn collect_snapshot(
        &self,
        repos: &[PathBuf],
        home: Option<&Path>,
        cwd: Option<&Path>,
    ) -> AppSnapshot {
        let mut groups = Vec::new();
        let mut warnings = Vec::new();
        let mut lane_res: Result<LaneStatus, String> = Ok(LaneStatus::default());
        let mut events_list: Vec<EventItem> = Vec::new();
        let mut dag_queue_list: Vec<DagQueueItem> = Vec::new();

        std::thread::scope(|s| {
            let lane_handle = s.spawn(|| self.fetch_lane());
            let events_handle = s.spawn(|| self.fetch_events());
            let queue_handle = s.spawn(|| self.fetch_dag_queue());
            let repo_handles: Vec<_> = repos
                .iter()
                .map(|repo| s.spawn(move || self.fetch_repo_status(repo, home)))
                .collect();

            for handle in repo_handles {
                match handle.join() {
                    Ok(Ok(group)) => {
                        if !group.runs.is_empty() {
                            groups.push(group);
                        }
                    }
                    Ok(Err(err)) => warnings.push(err),
                    Err(_) => warnings.push("仓库采集线程异常退出".to_string()),
                }
            }
            if let Ok(res) = lane_handle.join() {
                lane_res = res;
            }
            if let Ok(evs) = events_handle.join() {
                events_list = evs;
            }
            if let Ok(qs) = queue_handle.join() {
                dag_queue_list = qs;
            }
        });

        sort_repos_for_cwd(&mut groups, cwd);

        let lane = match lane_res {
            Ok(lane) => lane,
            Err(err) => {
                warnings.push(err);
                LaneStatus::default()
            }
        };
        let host_load = read_host_load();
        AppSnapshot {
            repos: groups,
            lane,
            events: events_list,
            dag_queue: dag_queue_list,
            host_load,
            refreshed_at: local_hms(),
            warnings,
        }
    }
}

fn absolutize_bin(p: PathBuf) -> PathBuf {
    if p.is_absolute() || p.components().count() <= 1 {
        return p;
    }
    fs::canonicalize(&p).unwrap_or_else(|_| {
        env::current_dir()
            .map(|cwd| cwd.join(&p))
            .unwrap_or(p)
    })
}

pub fn resolve_delegate_bin(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit {
        return absolutize_bin(p);
    }
    if let Ok(env_bin) = env::var("DELEGATE_BIN") {
        let trimmed = env_bin.trim();
        if !trimmed.is_empty() {
            let p = absolutize_bin(PathBuf::from(trimmed));
            if p.is_file() {
                return p;
            }
        }
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("delegate");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    if let Some(home) = home_dir() {
        for rel in [
            ".claude/skills/delegate/bin/delegate",
            ".codex/skills/delegate/bin/delegate",
            ".cursor/skills/delegate/bin/delegate",
            ".local/share/aia-skills/skills/delegate/bin/delegate",
        ] {
            let candidate = home.join(rel);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from("delegate")
}

fn probe_help_contains(bin: &Path, subcommand: &str, needle: &str) -> bool {
    let Ok(output) = Command::new(bin).args([subcommand, "--help"]).output() else {
        return false;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.contains(needle)
}

pub fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

pub fn repo_display_label(repo: &Path, home: Option<&Path>) -> String {
    if let Some(h) = home {
        if let Ok(rel) = repo.strip_prefix(h) {
            if rel.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rel.display());
        }
    }
    repo.display().to_string()
}

/// Discover repositories with `.local/run/delegate` under `root`, matching `delegate-board`.
pub fn discover_repos(root: &Path, extra_cwd: Option<&Path>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk_repos(root, 0, &mut found);
    if let Some(cwd) = extra_cwd {
        if let Some(git_root) = find_git_root(cwd) {
            if git_root.join(".local/run/delegate").is_dir() && !found.contains(&git_root) {
                found.push(git_root);
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

fn walk_repos(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if dir.join(".local/run/delegate").is_dir() {
        found.push(dir.to_path_buf());
    }
    if depth >= MAX_WALK_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if PRUNE_DIRS.contains(&name_str.as_ref()) {
            continue;
        }
        subdirs.push(path);
    }
    subdirs.sort();
    for sub in subdirs {
        walk_repos(&sub, depth + 1, found);
    }
}

/// Resolve the primary repository root for `start`, following a linked worktree `.git` file
/// back to its source repository root when applicable.
pub fn find_primary_repo_root(start: &Path) -> Option<PathBuf> {
    let git_root = find_git_root(start)?;
    let dot_git = git_root.join(".git");
    if dot_git.is_file() {
        if let Ok(content) = fs::read_to_string(&dot_git) {
            if let Some(gitdir) = content.trim().strip_prefix("gitdir:").map(str::trim) {
                let gd = PathBuf::from(gitdir);
                let gd = if gd.is_absolute() {
                    gd
                } else {
                    git_root.join(gd)
                };
                // Linked worktree gitdir is `<source>/.git/worktrees/<name>`
                if let Some(worktrees_dir) = gd.parent() {
                    if worktrees_dir.file_name().is_some_and(|n| n == "worktrees") {
                        if let Some(main_dot_git) = worktrees_dir.parent() {
                            if main_dot_git.file_name().is_some_and(|n| n == ".git") {
                                if let Some(main_root) = main_dot_git.parent() {
                                    return Some(
                                        fs::canonicalize(main_root)
                                            .unwrap_or_else(|_| main_root.to_path_buf()),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Some(fs::canonicalize(&git_root).unwrap_or(git_root))
}

/// Sort repositories so the repo containing `cwd` comes first, and the rest are ordered by most
/// recent activity (smallest `recent_activity_seconds` first).
pub fn sort_repos_for_cwd(repos: &mut [RepoGroup], cwd: Option<&Path>) {
    let current_root = cwd.and_then(find_primary_repo_root);
    let cwd_canon = cwd.map(|c| fs::canonicalize(c).unwrap_or_else(|_| c.to_path_buf()));

    let is_current = |repo: &RepoGroup| -> bool {
        let repo_canon = fs::canonicalize(&repo.path).unwrap_or_else(|_| repo.path.clone());
        if let Some(ref root) = current_root {
            if &repo_canon == root {
                return true;
            }
        }
        if let Some(ref c) = cwd_canon {
            if c == &repo_canon || c.starts_with(&repo_canon) {
                return true;
            }
        }
        false
    };

    repos.sort_by(|a, b| {
        let a_cur = is_current(a);
        let b_cur = is_current(b);
        b_cur
            .cmp(&a_cur)
            .then_with(|| a.recent_activity_seconds().cmp(&b.recent_activity_seconds()))
            .then_with(|| a.label.cmp(&b.label))
    });
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut cur = start.to_path_buf();
    loop {
        if cur.join(".git").exists() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

/// Parse `delegate status --json --all [--files]` JSONL output into a chain-collapsed RepoGroup.
pub fn parse_status_jsonl(repo_path: &Path, repo_label: &str, raw: &str) -> RepoGroup {
    let mut raw_items: Vec<Value> = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        // Skip non-run metadata objects (worktreeDisk, sourceBuildDisk, statusSummary).
        let run_id = v.get("run").and_then(Value::as_str).unwrap_or("");
        let state = v.get("state").and_then(Value::as_str).unwrap_or("");
        if run_id.is_empty() || state.is_empty() {
            continue;
        }
        raw_items.push(v);
    }

    // Build parent map to collapse reply chains to their latest run.
    let mut by_id: HashMap<String, &Value> = HashMap::new();
    let mut parents: HashSet<String> = HashSet::new();
    for item in &raw_items {
        if let Some(id) = item.get("run").and_then(Value::as_str) {
            by_id.insert(id.to_string(), item);
        }
        if let Some(p) = item
            .get("parent")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            parents.insert(p.to_string());
        }
    }

    let mut runs = Vec::new();
    for item in &raw_items {
        let run_id = item
            .get("run")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if parents.contains(&run_id) {
            continue;
        }
        let (chain_ids, computed_round, root_name) = trace_chain(&run_id, &by_id);
        let json_round = item
            .get("chainRound")
            .and_then(Value::as_u64)
            .unwrap_or(1) as usize;
        let chain_round = computed_round.max(json_round).max(1);
        runs.push(parse_run_item(
            item,
            repo_path,
            repo_label,
            chain_round,
            chain_ids,
            root_name.as_deref(),
        ));
    }

    // Newest item at the top within every group.
    runs.sort_by(|a, b| {
        a.group
            .cmp(&b.group)
            .then_with(|| match a.group {
                GroupKind::Running | GroupKind::Queued => a
                    .age_seconds
                    .unwrap_or(u64::MAX)
                    .cmp(&b.age_seconds.unwrap_or(u64::MAX)),
                GroupKind::Pending | GroupKind::MergedUncleaned | GroupKind::Stopped => a
                    .last_activity_seconds()
                    .cmp(&b.last_activity_seconds()),
            })
            .then_with(|| b.run.cmp(&a.run))
    });

    RepoGroup {
        path: repo_path.to_path_buf(),
        label: repo_label.to_string(),
        runs,
    }
}

fn trace_chain(head_id: &str, by_id: &HashMap<String, &Value>) -> (Vec<String>, usize, Option<String>) {
    let mut ids = Vec::new();
    let mut visited = HashSet::new();
    let mut cur = Some(head_id.to_string());
    let mut root_name = None;
    while let Some(id) = cur {
        if !visited.insert(id.clone()) {
            break;
        }
        ids.push(id.clone());
        if let Some(v) = by_id.get(&id) {
            if let Some(n) = v.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                root_name = Some(n.to_string());
            }
            cur = v
                .get("parent")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
        } else {
            // Parent was already cleaned/archived; count it as at least one prior round.
            break;
        }
    }
    ids.reverse();
    let depth = ids.len();
    (ids, depth, root_name)
}

fn strip_reply_prefixes(s: &str) -> String {
    let mut cur = s.trim();
    while let Some(rest) = cur.strip_prefix("reply to ") {
        cur = rest.trim();
    }
    single_line(cur)
}

fn parse_run_item(
    v: &Value,
    repo_path: &Path,
    repo_label: &str,
    chain_round: usize,
    chain_ids: Vec<String>,
    root_name: Option<&str>,
) -> RunItem {
    let run = v
        .get("run")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let raw_name = v
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&run)
        .to_string();
    let display_name = v
        .get("chainName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(strip_reply_prefixes)
        .or_else(|| root_name.map(strip_reply_prefixes))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| strip_reply_prefixes(&raw_name));

    let state = v
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let applied = v.get("applied").and_then(Value::as_bool).unwrap_or(false);
    let question = v.get("question").and_then(Value::as_bool).unwrap_or(false);
    let queue_obj = v.get("queue").filter(|q| q.is_object());
    let queue_reason = queue_obj
        .and_then(|q| q.get("reason"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let queue_ahead = queue_obj.and_then(|q| q.get("ahead")).and_then(Value::as_u64);
    let measure_waiting_ahead = v.get("measureWaitingAhead").and_then(Value::as_u64);

    let group = classify_group(
        &state,
        applied,
        queue_obj.is_some() || measure_waiting_ahead.is_some(),
    );

    let model = format_model_name(v);
    let tier = v
        .get("tier")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mode = v
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let changes = v.get("changes");
    let changes_files = changes
        .and_then(|c| c.get("files"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let changes_added = changes
        .and_then(|c| c.get("added"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let changes_deleted = changes
        .and_then(|c| c.get("deleted"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let pending = v.get("pendingChanges");
    let pending_files = pending
        .and_then(|c| c.get("files"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let pending_added = pending
        .and_then(|c| c.get("added"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let pending_deleted = pending
        .and_then(|c| c.get("deleted"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let files = string_list(v.get("files"));
    let (dir_changes, dirs_more) = parse_dir_changes(v.get("shape"), &files);
    let accept = parse_accept(v.get("accept"));

    let protect_violation = string_list(v.get("protectViolation"));
    let prompt_protected_paths = string_list(v.get("promptProtectedPaths"));
    let source_write_violation = v
        .get("sourceWriteViolation")
        .is_some_and(|sv| !sv.is_null() && sv != &Value::Bool(false));
    let conflict_markers = string_list(v.get("conflictMarkers"));

    let last_command = parse_last_command(v.get("last"));
    let activity = parse_activity(v.get("activity"));
    let stall_warning = parse_stall_warning(
        v.get("stallWarning")
            .or_else(|| v.get("activity").and_then(|a| a.get("stallWarning"))),
    );
    let next_step = v
        .get("next")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(single_line);
    let recovery = v
        .get("recovery")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(single_line);
    let error = v
        .get("error")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(single_line);
    let transient_error = v
        .get("transientError")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(single_line);

    let notes = parse_notes(v.get("notes"));
    let pending_notes = v
        .get("pendingNotes")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or_else(|| notes.iter().filter(|n| !n.done).count());

    let tokens = v.get("tokens");
    let tokens_input = tokens.and_then(|t| t.get("input")).and_then(Value::as_u64);
    let tokens_output = tokens.and_then(|t| t.get("output")).and_then(Value::as_u64);
    let tokens_cache_read = tokens
        .and_then(|t| t.get("cacheRead"))
        .and_then(Value::as_u64);

    let has_result = v
        .get("result")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    let result_chars = v
        .get("resultChars")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let has_worktree = v
        .get("worktree")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());

    let elapsed_seconds = v.get("elapsedSeconds").and_then(Value::as_u64);
    let age_seconds = v.get("ageSeconds").and_then(Value::as_u64);
    let queued_seconds = v.get("queuedSeconds").and_then(Value::as_u64);
    let finished_ago_seconds = if matches!(group, GroupKind::Running | GroupKind::Queued) {
        None
    } else {
        age_seconds.map(|age| {
            age.saturating_sub(elapsed_seconds.unwrap_or(0))
                .saturating_sub(queued_seconds.unwrap_or(0))
        })
    };

    RunItem {
        run,
        name: raw_name,
        display_name,
        chain_round,
        chain_ids,
        parent: v
            .get("parent")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        repo_path: repo_path.to_path_buf(),
        repo_label: repo_label.to_string(),
        state,
        group,
        model,
        tier,
        mode,
        elapsed_seconds,
        age_seconds,
        finished_ago_seconds,
        idle_seconds: v.get("idleSeconds").and_then(Value::as_u64),
        queued_seconds,
        turns: v.get("turns").and_then(Value::as_u64),
        changes_files,
        changes_added,
        changes_deleted,
        pending_files,
        pending_added,
        pending_deleted,
        dir_changes,
        dirs_more,
        files,
        accept,
        protect_violation,
        prompt_protected_paths,
        source_write_violation,
        conflict_markers,
        last_command,
        activity,
        stall_warning,
        next_step,
        recovery,
        error,
        transient_error,
        resumes: v.get("resumes").and_then(Value::as_u64).unwrap_or(0),
        question,
        applied,
        applied_by: v
            .get("appliedBy")
            .and_then(Value::as_str)
            .map(str::to_string),
        pending_reply: v.get("pendingReply").is_some_and(|pr| !pr.is_null()),
        queue_reason,
        queue_ahead,
        measure_waiting_ahead,
        pending_notes,
        notes,
        result_chars,
        has_result,
        has_worktree,
        finished_at: v
            .get("finishedAt")
            .and_then(Value::as_str)
            .map(str::to_string),
        tokens_input,
        tokens_output,
        tokens_cache_read,
    }
}

fn classify_group(state: &str, applied: bool, has_queue: bool) -> GroupKind {
    match state {
        "waiting" | "queued" => GroupKind::Queued,
        "running" | "starting" => {
            if has_queue {
                GroupKind::Queued
            } else {
                GroupKind::Running
            }
        }
        "stopped" => {
            if applied {
                GroupKind::MergedUncleaned
            } else {
                GroupKind::Stopped
            }
        }
        "cleaned" => GroupKind::Pending,
        _ => {
            if applied {
                GroupKind::MergedUncleaned
            } else {
                GroupKind::Pending
            }
        }
    }
}

fn format_model_name(v: &Value) -> String {
    let raw = v
        .get("modelName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            v.get("model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            v.get("agent")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("-");
    let short = raw.rsplit('/').next().unwrap_or(raw);
    short.replace("gemini-4-", "")
}

fn parse_accept(val: Option<&Value>) -> AcceptStatus {
    let Some(obj) = val.filter(|v| v.is_object()) else {
        return AcceptStatus::None;
    };
    let command = obj
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if obj.get("ok").and_then(Value::as_bool) == Some(true) {
        AcceptStatus::Passed { command }
    } else if obj
        .get("infrastructureFailure")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || obj
            .get("notExecuted")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        AcceptStatus::NotExecuted {
            command,
            infra: obj
                .get("infrastructureFailure")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    } else {
        AcceptStatus::Failed {
            command,
            exit_code: obj.get("exitCode").and_then(Value::as_i64),
        }
    }
}

fn parse_last_command(val: Option<&Value>) -> Option<String> {
    let v = val?;
    let raw = match v {
        Value::String(s) if !s.trim().is_empty() => s.clone(),
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string()),
        _ => return None,
    };
    let clean = single_line(&raw);
    if clean.is_empty() {
        return None;
    }
    for prefix in ["bash: ", "Bash: "] {
        if let Some(rest) = clean.strip_prefix(prefix) {
            return Some(format!("$ {rest}"));
        }
    }
    Some(clean)
}

fn parse_notes(val: Option<&Value>) -> Vec<NoteItem> {
    let Some(arr) = val.and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let text = item.get("text").and_then(Value::as_str)?.trim();
            if text.is_empty() {
                return None;
            }
            Some(NoteItem {
                index: item
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .unwrap_or(i + 1),
                text: single_line(text),
                done: item.get("done").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect()
}

fn parse_dir_changes(shape: Option<&Value>, files: &[String]) -> (Vec<DirChange>, usize) {
    if let Some(dirs) = shape
        .and_then(|s| s.get("dirs"))
        .and_then(Value::as_array)
        .filter(|arr| !arr.is_empty())
    {
        let mut out = Vec::new();
        for d in dirs {
            let path = d
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or(".")
                .to_string();
            let added = d.get("added").and_then(Value::as_u64).unwrap_or(0);
            let deleted = d.get("deleted").and_then(Value::as_u64).unwrap_or(0);
            let file_count = files
                .iter()
                .filter(|f| file_matches_dir(f, &path))
                .count();
            out.push(DirChange {
                path,
                added,
                deleted,
                files: file_count,
            });
        }
        let more = shape
            .and_then(|s| s.get("dirsMore"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        return (out, more);
    }

    // Fallback: group `files` by directory (up to 2 path segments).
    if files.is_empty() {
        return (Vec::new(), 0);
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for f in files {
        let dir = summarize_dir_key(f);
        *counts.entry(dir).or_insert(0) += 1;
    }
    let mut list: Vec<DirChange> = counts
        .into_iter()
        .map(|(path, count)| DirChange {
            path,
            added: 0,
            deleted: 0,
            files: count,
        })
        .collect();
    list.sort_by(|a, b| b.files.cmp(&a.files).then_with(|| a.path.cmp(&b.path)));
    let more = list.len().saturating_sub(8);
    list.truncate(8);
    (list, more)
}

fn file_matches_dir(file: &str, dir: &str) -> bool {
    if dir == "." {
        !file.contains('/')
    } else {
        file == dir || file.starts_with(&format!("{dir}/"))
    }
}

fn summarize_dir_key(file: &str) -> String {
    let parts: Vec<&str> = file.split('/').collect();
    match parts.len() {
        0 | 1 => ".".to_string(),
        2 => parts[0].to_string(),
        _ => format!("{}/{}", parts[0], parts[1]),
    }
}

fn string_list(val: Option<&Value>) -> Vec<String> {
    match val {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// Parse `delegate lane --json` output.
pub fn parse_lane_json(raw: &str) -> Result<LaneStatus, String> {
    let v: Value = serde_json::from_str(raw.trim()).map_err(|e| e.to_string())?;
    let parse_entries = |key: &str, remote: bool| -> Vec<LaneEntry> {
        v.get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|e| LaneEntry {
                label: single_line(e.get("label").and_then(Value::as_str).unwrap_or("")),
                since: e
                    .get("since")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                running: e.get("running").and_then(Value::as_bool).unwrap_or(false),
                remote,
            })
            .collect()
    };
    Ok(LaneStatus {
        max_heavy: v.get("maxHeavy").and_then(Value::as_u64).map(|n| n as usize),
        remote_max_heavy: v
            .get("remoteMaxHeavy")
            .and_then(Value::as_u64)
            .map(|n| n as usize),
        local: parse_entries("local", false),
        remote: parse_entries("remote", true),
    })
}

/// Parse plain-text `delegate lane` output (works on all delegate versions).
pub fn parse_lane_text(raw: &str) -> LaneStatus {
    let mut local = Vec::new();
    let mut remote = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (is_remote, rest) = if let Some(r) = trimmed.strip_prefix("remote ") {
            (true, r.trim_start())
        } else {
            (false, trimmed)
        };
        let (running, after_state) = if let Some(r) = rest.strip_prefix("running") {
            (true, r.trim_start())
        } else if let Some(r) = rest.strip_prefix("queued") {
            (false, r.trim_start())
        } else {
            continue;
        };
        let mut parts = after_state.splitn(2, char::is_whitespace);
        let since = parts.next().unwrap_or("").to_string();
        let label = single_line(parts.next().unwrap_or(""));
        let entry = LaneEntry {
            label,
            since,
            running,
            remote: is_remote,
        };
        if is_remote {
            remote.push(entry);
        } else {
            local.push(entry);
        }
    }
    LaneStatus {
        max_heavy: None,
        remote_max_heavy: None,
        local,
        remote,
    }
}

pub fn read_host_load() -> Option<HostLoad> {
    let loadavg = fs::read_to_string("/proc/loadavg").ok()?;
    let (load1_tenths, load5_tenths, load15_tenths) = parse_proc_loadavg(&loadavg)?;
    let mem = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| parse_proc_meminfo(&s));
    Some(HostLoad {
        load1_tenths,
        load5_tenths,
        load15_tenths,
        mem_used_tenths_gib: mem.map(|(u, _)| u),
        mem_total_tenths_gib: mem.map(|(_, t)| t),
    })
}

pub fn parse_proc_loadavg(text: &str) -> Option<(u32, u32, u32)> {
    let mut it = text.split_whitespace();
    let l1: f64 = it.next()?.parse().ok()?;
    let l5: f64 = it.next()?.parse().ok()?;
    let l15: f64 = it.next()?.parse().ok()?;
    Some((
        (l1 * 10.0).round().max(0.0) as u32,
        (l5 * 10.0).round().max(0.0) as u32,
        (l15 * 10.0).round().max(0.0) as u32,
    ))
}

pub fn parse_proc_meminfo(text: &str) -> Option<(u32, u32)> {
    let mut total_kb: Option<u64> = None;
    let mut avail_kb: Option<u64> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = rest.split_whitespace().next().and_then(|s| s.parse().ok());
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail_kb = rest.split_whitespace().next().and_then(|s| s.parse().ok());
        }
        if total_kb.is_some() && avail_kb.is_some() {
            break;
        }
    }
    let total = total_kb?;
    let avail = avail_kb?;
    let used = total.saturating_sub(avail);
    let to_tenths_gib = |kb: u64| -> u32 {
        ((kb as f64 / (1024.0 * 1024.0)) * 10.0).round().max(0.0) as u32
    };
    Some((to_tenths_gib(used), to_tenths_gib(total)))
}

/// Extract the first `max_paragraphs` non-empty paragraphs (capped at `max_lines`) for the detail preview.
pub fn extract_result_excerpt(
    full_text: &str,
    max_paragraphs: usize,
    max_lines: usize,
) -> (String, bool) {
    let mut out_lines: Vec<&str> = Vec::new();
    let mut paragraphs = 0usize;
    let mut in_paragraph = false;
    let mut truncated = false;

    for raw in full_text.lines() {
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() {
            if in_paragraph {
                in_paragraph = false;
                if paragraphs >= max_paragraphs {
                    truncated = true;
                    break;
                }
                if !out_lines.is_empty() && out_lines.len() < max_lines {
                    out_lines.push("");
                }
            }
            continue;
        }
        if !in_paragraph {
            if paragraphs >= max_paragraphs {
                truncated = true;
                break;
            }
            paragraphs += 1;
            in_paragraph = true;
        }
        if out_lines.len() >= max_lines {
            truncated = true;
            break;
        }
        out_lines.push(line);
    }

    while out_lines.last().is_some_and(|l| l.is_empty()) {
        out_lines.pop();
    }
    (out_lines.join("\n"), truncated)
}

pub fn local_hms() -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return "--:--:--".to_string();
        }
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

fn parse_activity(val: Option<&Value>) -> Option<ActivityInfo> {
    let obj = val.filter(|v| v.is_object())?;
    let kind = obj.get("kind").and_then(Value::as_str)?.to_string();
    let label = obj
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if kind.is_empty() && label.is_empty() {
        return None;
    }
    Some(ActivityInfo {
        kind,
        label: single_line(&label),
        cmd: obj
            .get("cmd")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(single_line),
        elapsed_seconds: obj.get("elapsedSeconds").and_then(Value::as_u64),
        detail: obj
            .get("detail")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(single_line),
    })
}

fn parse_stall_warning(val: Option<&Value>) -> Option<StallWarningInfo> {
    let obj = val.filter(|v| v.is_object())?;
    let kind = obj
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if kind.is_empty() {
        return None;
    }
    Some(StallWarningInfo {
        kind,
        cmd: obj
            .get("cmd")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(single_line),
        seconds: obj.get("seconds").and_then(Value::as_u64),
        count: obj.get("count").and_then(Value::as_u64),
        detail: obj
            .get("detail")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(single_line),
        self_check: obj
            .get("selfCheck")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(single_line),
    })
}

pub fn parse_events_jsonl(raw: &str) -> Vec<EventItem> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let Some(seq) = v.get("seq").and_then(Value::as_u64) else {
            continue;
        };
        let at = v
            .get("at")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let run = v
            .get("run")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let repo_full = v.get("repo").and_then(Value::as_str).unwrap_or("");
        let repo = Path::new(repo_full)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(repo_full)
            .to_string();
        let kind = v
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let p = &v["payload"];
        let summary = summarize_event_payload(&kind, &run, p);
        let payload_pretty =
            serde_json::to_string_pretty(p).unwrap_or_else(|_| p.to_string());
        out.push(EventItem {
            seq,
            at,
            run,
            repo,
            kind,
            summary,
            payload_pretty,
        });
    }
    if out.len() > 500 {
        let drop_n = out.len() - 500;
        out.drain(..drop_n);
    }
    out
}

fn summarize_event_payload(kind: &str, run: &str, p: &Value) -> String {
    let ps = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("");
    let name = ps("name");
    let target = if !name.is_empty() {
        single_line(name)
    } else if !ps("queueName").is_empty() {
        single_line(ps("queueName"))
    } else if !run.is_empty() {
        run.to_string()
    } else {
        single_line(ps("label"))
    };
    let extra = match kind {
        "started" => {
            let m = ps("model");
            if m.is_empty() {
                ps("mode").to_string()
            } else {
                format!("{} {m}", ps("mode"))
            }
        }
        "queued" => {
            let scope = ps("scope");
            if scope == "dag" {
                let after = string_list(p.get("after")).join(",");
                format!("after: {after}")
            } else {
                let r = ps("reason");
                if r.is_empty() {
                    ps("label").to_string()
                } else {
                    r.to_string()
                }
            }
        }
        "lane_acquired" | "lane_released" => ps("label").to_string(),
        "accept" => {
            let ok = if p.get("ok").and_then(Value::as_bool) == Some(true) {
                "通过"
            } else {
                "失败"
            };
            format!("{ok} {}", ps("command"))
        }
        "delivered" => {
            if ps("state") == "answered" {
                "已答复".into()
            } else {
                "已交付".into()
            }
        }
        "question" => "等待主控决定".into(),
        "rejected" | "failed" => ps("error").to_string(),
        "stall_warning" => {
            let det = ps("detail");
            if det.is_empty() {
                ps("stallKind").to_string()
            } else {
                det.to_string()
            }
        }
        "loop_review" => ps("reason").to_string(),
        "queue_dispatched" => format!("-> {}", ps("nextRun")),
        "queue_paused" => ps("reason").to_string(),
        "applied" => {
            let head = ps("head");
            if head.is_empty() {
                "已合入".into()
            } else {
                format!("合入 {}", &head[..head.len().min(7)])
            }
        }
        _ => String::new(),
    };
    if extra.is_empty() {
        target
    } else if target.is_empty() {
        single_line(&extra)
    } else {
        format!("{target} · {}", single_line(&extra))
    }
}

pub fn parse_dag_queue_jsonl(raw: &str) -> Vec<DagQueueItem> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let name = v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let repo_full = v.get("repo").and_then(Value::as_str).unwrap_or("");
        let repo = Path::new(repo_full)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(repo_full)
            .to_string();
        out.push(DagQueueItem {
            name,
            status: v
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pending")
                .to_string(),
            after: string_list(v.get("after")),
            repo,
            prompt_file: v
                .get("promptFile")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            run_id: v
                .get("runId")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            pause_reason: v
                .get("pauseReason")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(single_line),
            created_at: v
                .get("createdAt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        });
    }
    out
}
