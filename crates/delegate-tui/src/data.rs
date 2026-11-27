use crate::model::*;
use crate::width::single_line;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

const PRUNE_DIRS: &[&str] = &[".cache", "target", "node_modules", "third_party", ".git"];
const MAX_WALK_DEPTH: usize = 6;

#[derive(Debug, Clone)]
pub struct DelegateClient {
    pub bin: PathBuf,
    lane_json_supported: Arc<OnceLock<bool>>,
    result_no_mark_supported: Arc<OnceLock<bool>>,
}

impl DelegateClient {
    pub fn discover(explicit: Option<PathBuf>) -> Self {
        let bin = resolve_delegate_bin(explicit);
        Self {
            bin,
            lane_json_supported: Arc::new(OnceLock::new()),
            result_no_mark_supported: Arc::new(OnceLock::new()),
        }
    }

    pub fn from_parts(
        bin: PathBuf,
        lane_json_supported: bool,
        result_no_mark_supported: bool,
    ) -> Self {
        let lane = OnceLock::new();
        let _ = lane.set(lane_json_supported);
        let result = OnceLock::new();
        let _ = result.set(result_no_mark_supported);
        Self {
            bin,
            lane_json_supported: Arc::new(lane),
            result_no_mark_supported: Arc::new(result),
        }
    }

    fn lane_json_supported(&self) -> bool {
        *self
            .lane_json_supported
            .get_or_init(|| probe_help_contains(&self.bin, "lane", "--json"))
    }

    fn result_no_mark_supported(&self) -> bool {
        *self
            .result_no_mark_supported
            .get_or_init(|| probe_help_contains(&self.bin, "result", "--no-mark"))
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
        let json_supported = self.lane_json_supported();
        let mut cmd = Command::new(&self.bin);
        cmd.arg("lane");
        if json_supported {
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
        if json_supported {
            parse_lane_json(&stdout).or_else(|_| Ok(parse_lane_text(&stdout)))
        } else {
            Ok(parse_lane_text(&stdout))
        }
    }

    pub fn fetch_result(&self, repo: &Path, run_id: &str) -> Result<String, String> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("result");
        if self.result_no_mark_supported() {
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

    pub fn fetch_merges(&self) -> Vec<MergeRunItem> {
        let Ok(output) = Command::new(&self.bin)
            .args(["merge", "list", "--all", "--json"])
            .output()
        else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_merges_jsonl(&stdout)
    }

    pub fn fetch_candidates(&self) -> Vec<RadarCandidateItem> {
        let Ok(output) = Command::new(&self.bin)
            .args(["radar", "list", "--all", "--json"])
            .output()
        else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_candidates_jsonl(&stdout)
    }

    pub fn fetch_agy_accounts(&self) -> AgyAccountsState {
        let Ok(output) = Command::new(&self.bin)
            .args(["agy", "account", "list", "--json"])
            .output()
        else {
            return AgyAccountsState::default();
        };
        if !output.status.success() {
            return AgyAccountsState::default();
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_agy_accounts_json(&stdout)
    }

    pub fn refresh_agy_quota(&self, force: bool) -> Result<AgyAccountsState, String> {
        let quota_flag = if force { "--quota" } else { "--quota-if-stale" };
        let output = Command::new(&self.bin)
            .args(["agy", "account", "list", quota_flag, "--json"])
            .output()
            .map_err(|e| format!("无法执行 agy account list {quota_flag}: {e}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let msg = single_line(&stderr);
            return Err(if msg.is_empty() {
                format!("额度刷新失败（退出码 {:?}）", output.status.code())
            } else {
                msg
            });
        }
        Ok(parse_agy_accounts_json(&stdout))
    }

    pub fn fetch_line_stats(&self) -> Result<LineStatsSnapshot, String> {
        let output = Command::new(&self.bin)
            .args(["lines", "--json"])
            .output()
            .map_err(|e| format!("无法执行 delegate lines --json: {e}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let msg = single_line(&stderr);
            return Err(if msg.is_empty() {
                format!("代码改动统计失败（退出码 {:?}）", output.status.code())
            } else {
                msg
            });
        }
        parse_line_stats_json(&stdout)
    }

    pub fn switch_agy_account(&self, id: &str) -> Result<String, String> {
        let output = Command::new(&self.bin)
            .args(["agy", "account", "use", id, "--json"])
            .output()
            .map_err(|e| format!("无法执行 agy account use: {e}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        parse_switch_agy_account_output(id, &stdout, &stderr, output.status.success(), output.status.code())
    }
}

pub fn parse_switch_agy_account_output(
    id: &str,
    stdout: &str,
    stderr: &str,
    success: bool,
    exit_code: Option<i32>,
) -> Result<String, String> {
    if let Ok(v) = serde_json::from_str::<Value>(stdout.trim()) {
        if v.get("ok").and_then(Value::as_bool) == Some(true) {
            let running_cnt = v
                .get("runningAgyCount")
                .and_then(Value::as_u64)
                .or_else(|| {
                    v.get("runningAgy")
                        .and_then(Value::as_array)
                        .map(|a| a.len() as u64)
                })
                .unwrap_or(0);
            if running_cnt > 0 {
                return Ok(format!(
                    "已切到 {id}；在跑的 {running_cnt} 个运行用原账号跑完"
                ));
            }
            let email = v.get("email").and_then(Value::as_str).unwrap_or("");
            return if email.is_empty() {
                Ok(format!("已切到 {id}"))
            } else {
                Ok(format!("已切到 {id} ({email})"))
            };
        }
        let msg = v
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("切换失败");
        return Err(msg.to_string());
    }
    if success {
        Ok(format!("已切到 {id}"))
    } else {
        let err = single_line(stderr);
        if err.is_empty() {
            Err(format!("切换失败（退出码 {:?}）", exit_code))
        } else {
            Err(err)
        }
    }
}

impl DelegateClient {

    pub fn fetch_snapshot_fast(
        &self,
        repos: &[PathBuf],
        home: Option<&Path>,
        cwd: Option<&Path>,
    ) -> Option<AppSnapshot> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(["snapshot", "--json"]);
        for repo in repos {
            cmd.arg("--repo").arg(repo);
        }
        let output = cmd.output().ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_snapshot_json(&stdout, home, cwd).ok()
    }

    pub fn collect_snapshot(
        &self,
        repos: &[PathBuf],
        home: Option<&Path>,
        cwd: Option<&Path>,
    ) -> AppSnapshot {
        // Fast single-process path (delegate >= 6.32.0): zero locks, zero per-repo fanout.
        if let Some(snap) = self.fetch_snapshot_fast(repos, home, cwd) {
            return snap;
        }
        // Fallback for older delegate binaries (< 6.32.0).
        let mut groups = Vec::new();
        let mut warnings = Vec::new();
        let mut lane_res: Result<LaneStatus, String> = Ok(LaneStatus::default());
        let mut events_list: Vec<EventItem> = Vec::new();
        let mut dag_queue_list: Vec<DagQueueItem> = Vec::new();
        let mut merges_list: Vec<MergeRunItem> = Vec::new();
        let mut candidates_list: Vec<RadarCandidateItem> = Vec::new();
        let mut agy_accounts_state = AgyAccountsState::default();

        std::thread::scope(|s| {
            let lane_handle = s.spawn(|| self.fetch_lane());
            let events_handle = s.spawn(|| self.fetch_events());
            let queue_handle = s.spawn(|| self.fetch_dag_queue());
            let merges_handle = s.spawn(|| self.fetch_merges());
            let candidates_handle = s.spawn(|| self.fetch_candidates());
            let accounts_handle = s.spawn(|| self.fetch_agy_accounts());
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
            if let Ok(ms) = merges_handle.join() {
                merges_list = ms;
            }
            if let Ok(cs) = candidates_handle.join() {
                candidates_list = cs;
            }
            if let Ok(ac) = accounts_handle.join() {
                agy_accounts_state = ac;
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
            work: Vec::new(),
            lane,
            events: events_list,
            events_has_more: false,
            events_earliest_seq: None,
            dag_queue: dag_queue_list,
            merges: merges_list,
            candidates: candidates_list,
            agy_accounts: agy_accounts_state,
            analytics: AnalyticsSnapshot::default(),
            line_stats: None,
            recent_deliveries: Vec::new(),
            pulse: None,
            alerts: Vec::new(),
            alerts_from_snapshot: false,
            eta: SnapshotEta::default(),
            disk_health: None,
            host_load,
            generated_epoch: None,
            today_ymd: local_ymd(),
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

pub fn state_dir() -> PathBuf {
    if let Some(s) = env::var_os("XDG_STATE_HOME") {
        if !s.is_empty() {
            return PathBuf::from(s).join("delegate");
        }
    }
    home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local/state/delegate")
}

/// Parse the single JSON document emitted by `delegate snapshot --json` into an `AppSnapshot`.
pub fn parse_snapshot_json(
    raw: &str,
    home: Option<&Path>,
    cwd: Option<&Path>,
) -> Result<AppSnapshot, String> {
    let doc: Value = serde_json::from_str(raw.trim()).map_err(|e| format!("snapshot json: {e}"))?;
    if !doc.is_object() {
        return Err("snapshot json: expected object".to_string());
    }

    let generated_epoch = doc["generatedAt"].as_str().and_then(parse_iso_utc_epoch);
    let mut groups = Vec::new();
    if let Some(repos) = doc["repos"].as_array() {
        for r in repos {
            let repo_path = PathBuf::from(r["path"].as_str().unwrap_or(""));
            let runs_dir_str = r["runsDir"].as_str().unwrap_or("");
            let label = repo_display_label(&repo_path, home);
            let runs_slice = r["runs"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            let cleaned_slice = r["cleanedNotes"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let mut group = parse_status_values_at(
                &repo_path,
                &label,
                runs_slice.iter().chain(cleaned_slice.iter()),
                generated_epoch,
            );
            if !runs_dir_str.is_empty() {
                group.runs_dir = PathBuf::from(runs_dir_str);
            }
            if !group.runs.is_empty() {
                groups.push(group);
            }
        }
    }
    sort_repos_for_cwd(&mut groups, cwd);

    let lane = if doc["lane"].is_object() && !doc["lane"].as_object().is_none_or(|m| m.is_empty()) {
        parse_lane_value(&doc["lane"])
    } else {
        LaneStatus::default()
    };

    let work = parse_work_value(&doc["work"]);
    let mut events = parse_events_values(
        doc["events"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]),
    );
    let events_has_more = doc["eventsHasMore"].as_bool().unwrap_or(false);
    let events_earliest_seq = doc["eventsEarliestSeq"]
        .as_u64()
        .or_else(|| events.first().map(|e| e.seq));
    let dag_queue = parse_dag_queue_values(
        doc["dagQueue"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]),
    );
    let merges = parse_merges_values(
        doc["merges"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]),
    );
    let candidates = parse_candidates_values(
        doc["candidates"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]),
    );
    let mut agy_accounts = parse_agy_accounts_value(&doc["agyAccounts"]);
    enrich_run_names_from_groups(&groups, &mut events, &mut agy_accounts);
    let analytics = parse_analytics_value(&doc["analytics"]);
    let recent_deliveries = parse_recent_deliveries_value(&doc["recentDeliveries"]);
    let pulse = parse_pulse_value(&doc["pulse"]);
    let alerts_from_snapshot = doc["alerts"].is_array();
    let alerts = parse_alerts_value(&doc["alerts"]);
    let eta = parse_snapshot_eta_value(&doc["eta"]);
    let disk_health = parse_disk_health_value(&doc["diskHealth"]);
    let host_load = read_host_load();
    let today_ymd = doc["generatedAt"]
        .as_str()
        .and_then(iso_to_local_ymd)
        .unwrap_or_else(local_ymd);

    Ok(AppSnapshot {
        repos: groups,
        work,
        lane,
        events,
        events_has_more,
        events_earliest_seq,
        dag_queue,
        merges,
        candidates,
        agy_accounts,
        analytics,
        line_stats: None,
        recent_deliveries,
        pulse,
        alerts,
        alerts_from_snapshot,
        eta,
        disk_health,
        host_load,
        generated_epoch,
        today_ymd,
        refreshed_at: local_hms(),
        warnings: Vec::new(),
    })
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
    parse_status_jsonl_at(repo_path, repo_label, raw, None)
}

/// Like [`parse_status_jsonl`], computing `finished_ago_seconds` against `now_epoch` (or wall clock if `None`).
pub fn parse_status_jsonl_at(
    repo_path: &Path,
    repo_label: &str,
    raw: &str,
    now_epoch: Option<i64>,
) -> RepoGroup {
    let mut parsed_items: Vec<Value> = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        parsed_items.push(v);
    }
    parse_status_values_at(repo_path, repo_label, parsed_items.iter(), now_epoch)
}

fn parse_status_values_at<'a>(
    repo_path: &Path,
    repo_label: &str,
    items: impl IntoIterator<Item = &'a Value>,
    now_epoch: Option<i64>,
) -> RepoGroup {
    let mut raw_items: Vec<&'a Value> = Vec::new();
    for v in items {
        // Skip non-run metadata objects (worktreeDisk, sourceBuildDisk, statusSummary).
        let run_id = v.get("run").and_then(Value::as_str).unwrap_or("");
        let state = v.get("state").and_then(Value::as_str).unwrap_or("");
        if run_id.is_empty() || state.is_empty() {
            continue;
        }
        raw_items.push(v);
    }

    // Build parent map to collapse reply chains to their latest run.
    let mut by_id: HashMap<String, &'a Value> = HashMap::new();
    let mut parents: HashSet<String> = HashSet::new();
    for &item in &raw_items {
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
    for &item in &raw_items {
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
            &by_id,
            now_epoch,
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
                GroupKind::Pending
                | GroupKind::Collected
                | GroupKind::MergedUncleaned
                | GroupKind::Stopped => a
                    .last_activity_seconds()
                    .cmp(&b.last_activity_seconds()),
            })
            .then_with(|| b.run.cmp(&a.run))
    });

    RepoGroup {
        path: repo_path.to_path_buf(),
        runs_dir: repo_path.join(".local/run/delegate"),
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

fn sanitize_remote_host(raw: &str) -> String {
    let trimmed = raw.trim();
    match trimmed.rsplit_once('@') {
        Some((_, alias)) if !alias.trim().is_empty() => alias.trim().to_string(),
        _ => trimmed.to_string(),
    }
}

fn parse_remote_exec(v: &Value, now_epoch: Option<i64>) -> Option<RemoteExecInfo> {
    if let Some(re) = v.get("remoteExec").filter(|x| x.is_object()) {
        let host = re
            .get("host")
            .and_then(Value::as_str)
            .map(sanitize_remote_host)
            .unwrap_or_default();
        if !host.is_empty() {
            let active = re.get("active").and_then(Value::as_bool).unwrap_or(false);
            let running = re.get("running").and_then(Value::as_bool).unwrap_or(false);
            let queue = re
                .get("queue")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let cmd = re
                .get("cmd")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(single_line);
            let elapsed_seconds = re
                .get("elapsedSeconds")
                .and_then(Value::as_u64)
                .or_else(|| {
                    re.get("since")
                        .and_then(Value::as_str)
                        .and_then(parse_iso_utc_epoch)
                        .map(|ep| {
                            let now = now_epoch.unwrap_or_else(current_utc_epoch);
                            (now - ep).max(0) as u64
                        })
                });
            let output_tail = re
                .get("outputTail")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(|s| s.trim_end_matches('\r').to_string())
                        .filter(|s| !s.trim().is_empty())
                        .collect()
                })
                .unwrap_or_default();
            return Some(RemoteExecInfo {
                host,
                active,
                running,
                queue,
                cmd,
                elapsed_seconds,
                output_tail,
            });
        }
    }
    // Backwards-compatible fallback for snapshots from older `delegate` binaries that emit `remote` + `lane`.
    let host = v
        .get("remote")
        .and_then(|r| r.get("host"))
        .and_then(Value::as_str)
        .map(sanitize_remote_host)
        .unwrap_or_default();
    let lane = v.get("lane").filter(|l| {
        l.get("queue")
            .and_then(Value::as_str)
            .is_some_and(|q| q.starts_with("remote-"))
    });
    if host.is_empty() && lane.is_none() {
        return None;
    }
    let active = lane.is_some();
    let running = lane
        .and_then(|l| l.get("running"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let queue = lane
        .and_then(|l| l.get("queue"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let cmd = lane
        .and_then(|l| {
            l.get("cmd")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| l.get("label").and_then(Value::as_str))
        })
        .map(|raw| {
            let trimmed = match raw.split_once(": ") {
                Some((prefix, rest))
                    if !rest.trim().is_empty()
                        && (prefix.starts_with('/') || prefix.contains("worktrees")) =>
                {
                    rest.trim()
                }
                _ => raw.trim(),
            };
            single_line(trimmed)
        })
        .filter(|s| !s.is_empty());
    let elapsed_seconds = lane.and_then(|l| {
        l.get("elapsedSeconds")
            .and_then(Value::as_u64)
            .or_else(|| {
                l.get("since")
                    .and_then(Value::as_str)
                    .and_then(parse_iso_utc_epoch)
                    .map(|ep| {
                        let now = now_epoch.unwrap_or_else(current_utc_epoch);
                        (now - ep).max(0) as u64
                    })
            })
    });
    Some(RemoteExecInfo {
        host,
        active,
        running,
        queue,
        cmd,
        elapsed_seconds,
        output_tail: Vec::new(),
    })
}

fn build_chain_rounds(
    v: &Value,
    chain_ids: &[String],
    chain_round: usize,
    by_id: &HashMap<String, &Value>,
) -> Vec<ChainRoundInfo> {
    if let Some(arr) = v.get("authorRounds").and_then(Value::as_array) {
        if !arr.is_empty() {
            let mut out: Vec<ChainRoundInfo> = arr
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let rid = item
                        .get("run")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let rv = by_id.get(&rid).copied();
                    ChainRoundInfo {
                        round: item
                            .get("round")
                            .and_then(Value::as_u64)
                            .map(|n| n as usize)
                            .unwrap_or(i + 1),
                        run: rid,
                        kind: item
                            .get("kind")
                            .and_then(Value::as_str)
                            .unwrap_or(if i == 0 { "initial" } else { "reply" })
                            .to_string(),
                        state: rv
                            .and_then(|x| x.get("state"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        question: rv
                            .and_then(|x| x.get("question"))
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        elapsed_seconds: rv
                            .and_then(|x| x.get("elapsedSeconds"))
                            .and_then(Value::as_u64),
                        path: item
                            .get("path")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string),
                        total_lines: item
                            .get("totalLines")
                            .and_then(Value::as_u64)
                            .map(|n| n as usize),
                        total_chars: item
                            .get("totalChars")
                            .and_then(Value::as_u64)
                            .map(|n| n as usize),
                        current: item
                            .get("current")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    }
                })
                .collect();
            let cur_id = v.get("run").and_then(Value::as_str).unwrap_or("");
            let cur_state = v.get("state").and_then(Value::as_str).unwrap_or("");
            let cur_active = matches!(cur_state, "running" | "waiting" | "starting" | "scouting");
            if cur_active && !cur_id.is_empty() && !out.iter().any(|r| r.run == cur_id) {
                for r in &mut out {
                    r.current = false;
                }
                out.push(ChainRoundInfo {
                    round: out.len() + 1,
                    run: cur_id.to_string(),
                    kind: "reply".to_string(),
                    state: cur_state.to_string(),
                    question: false,
                    elapsed_seconds: v.get("elapsedSeconds").and_then(Value::as_u64),
                    path: None,
                    total_lines: None,
                    total_chars: None,
                    current: true,
                });
            }
            if out.len() > 1 {
                return out;
            }
        }
    }
    if chain_ids.len() <= 1 {
        return Vec::new();
    }
    let start_round = chain_round.saturating_sub(chain_ids.len()) + 1;
    let last_idx = chain_ids.len().saturating_sub(1);
    chain_ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let kind = if start_round + i == 1 {
                "initial"
            } else {
                "reply"
            };
            if let Some(rv) = by_id.get(id) {
                ChainRoundInfo {
                    round: start_round + i,
                    run: id.clone(),
                    kind: kind.to_string(),
                    state: rv
                        .get("state")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    question: rv
                        .get("question")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    elapsed_seconds: rv.get("elapsedSeconds").and_then(Value::as_u64),
                    path: rv
                        .get("result")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    total_lines: None,
                    total_chars: rv
                        .get("resultChars")
                        .and_then(Value::as_u64)
                        .map(|n| n as usize),
                    current: i == last_idx,
                }
            } else {
                ChainRoundInfo {
                    round: start_round + i,
                    run: id.clone(),
                    kind: kind.to_string(),
                    state: "archived".to_string(),
                    question: false,
                    elapsed_seconds: None,
                    path: None,
                    total_lines: None,
                    total_chars: None,
                    current: i == last_idx,
                }
            }
        })
        .collect()
}

fn strip_reply_prefixes(s: &str) -> String {
    let mut cur = s.trim();
    while let Some(rest) = cur.strip_prefix("reply to ") {
        cur = rest.trim();
    }
    single_line(cur)
}

#[allow(clippy::too_many_arguments)]
fn parse_run_item(
    v: &Value,
    repo_path: &Path,
    repo_label: &str,
    chain_round: usize,
    chain_ids: Vec<String>,
    root_name: Option<&str>,
    by_id: &HashMap<String, &Value>,
    now_epoch: Option<i64>,
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
    let mode = v
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let applied = v.get("applied").and_then(Value::as_bool).unwrap_or(false);
    let cleaned = v.get("cleaned").and_then(Value::as_bool).unwrap_or(false) || state == "cleaned";
    let replied = v
        .get("repliedAt")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    let question = v.get("question").and_then(Value::as_bool).unwrap_or(false) && !replied;
    let result_read_at = v
        .get("resultReadAt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let zero_change = is_zero_change_delivery(v, by_id);
    let queue_obj = v.get("queue").filter(|q| q.is_object());
    let queue_reason = queue_obj
        .and_then(|q| q.get("reason"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let queue_ahead = queue_obj.and_then(|q| q.get("ahead")).and_then(Value::as_u64);
    let measure_waiting_ahead = v.get("measureWaitingAhead").and_then(Value::as_u64);

    let group = classify_group(
        &state,
        &mode,
        applied,
        cleaned,
        question,
        result_read_at.is_some(),
        false,
        zero_change,
        queue_obj.is_some() || measure_waiting_ahead.is_some(),
    );

    let (model, thinking) = format_model_and_thinking(v);
    let tier = v
        .get("tier")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

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
    let review = parse_review(v.get("review"), v.get("sign"));
    let premerge = parse_premerge(v.get("premerge"));

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
    let finished_at = v
        .get("finishedAt")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let finished_ago_seconds = if matches!(group, GroupKind::Running | GroupKind::Queued) {
        None
    } else if let Some(fin_ep) = finished_at.as_deref().and_then(parse_iso_utc_epoch) {
        let now = now_epoch.unwrap_or_else(current_utc_epoch);
        Some((now - fin_ep).max(0) as u64)
    } else {
        age_seconds.map(|age| {
            age.saturating_sub(elapsed_seconds.unwrap_or(0))
                .saturating_sub(queued_seconds.unwrap_or(0))
        })
    };

    let remote_exec = parse_remote_exec(v, now_epoch);
    let chain_rounds = build_chain_rounds(v, &chain_ids, chain_round, by_id);

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
        thinking,
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
        review,
        premerge,
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
        appended_total: v
            .get("appends")
            .and_then(|a| a.get("total"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        appended_pending: v
            .get("appends")
            .and_then(|a| a.get("pending"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        queue_reason,
        queue_ahead,
        measure_waiting_ahead,
        pending_notes,
        notes,
        result_chars,
        has_result,
        has_worktree,
        result_read_at,
        finished_at,
        tokens_input,
        tokens_output,
        tokens_cache_read,
        work_id: v
            .get("workId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        eta: parse_run_eta_value(v.get("eta")),
        card_l0: parse_card_l0_value(v.get("card")),
        remote_exec,
        chain_rounds,
        agy_account_switch: parse_agy_account_switch(v.get("agyAccountSwitch")),
        review_agy_account_switch: parse_agy_account_switch(v.get("reviewAgyAccountSwitch")),
    }
}

fn parse_agy_account_switch(val: Option<&Value>) -> Option<AgyAccountSwitchInfo> {
    let obj = val?.as_object()?;
    let from = obj.get("from").and_then(Value::as_str)?.trim();
    let to = obj.get("to").and_then(Value::as_str)?.trim();
    if from.is_empty() || to.is_empty() {
        return None;
    }
    let kind = obj
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let reason = obj
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    Some(AgyAccountSwitchInfo {
        kind,
        from: from.to_string(),
        to: to.to_string(),
        reason,
    })
}

fn parse_card_l0_value(v: Option<&Value>) -> Option<CardL0Info> {
    let obj = v.filter(|x| x.is_object())?;
    let risk_count = obj
        .get("riskTotal")
        .or_else(|| obj.get("risks"))
        .or_else(|| obj.get("riskCount"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let max_severity = opt_nonempty_str(obj.get("maxSeverity"));
    let fast_forward = obj.get("fastForward").and_then(Value::as_bool);
    let tests_added = obj.get("testsAdded").and_then(Value::as_u64).unwrap_or(0);
    let explicit_summary = single_line(obj.get("summary").and_then(Value::as_str).unwrap_or(""));
    let summary = if !explicit_summary.is_empty() {
        explicit_summary
    } else if let Some(arr) = obj.get("riskByCategory").and_then(Value::as_array) {
        let mut parts: Vec<String> = arr
            .iter()
            .take(3)
            .filter_map(|item| {
                let cat = single_line(item.get("category").and_then(Value::as_str).unwrap_or(""));
                let cnt = item.get("count").and_then(Value::as_u64).unwrap_or(0);
                if cat.is_empty() || cnt == 0 {
                    None
                } else {
                    Some(format!("{cat} {cnt}"))
                }
            })
            .collect();
        if arr.len() > 3 && !parts.is_empty() {
            if let Some(last) = parts.last_mut() {
                last.push_str(" 等");
            }
        }
        parts.join("、")
    } else {
        String::new()
    };
    Some(CardL0Info {
        risk_count,
        max_severity,
        fast_forward,
        tests_added,
        summary,
    })
}

/// Strictly read-only, zero-subprocess load of a single run's L1 merge review card
/// from `<runs_dir>/<run_id>/card.json`. Called on demand for the currently selected run.
pub fn read_run_card_json(runs_dir: &Path, run_id: &str) -> Option<RunCardL1> {
    if run_id.is_empty() || runs_dir.as_os_str().is_empty() {
        return None;
    }
    let path = runs_dir.join(run_id).join("card.json");
    let raw = fs::read_to_string(&path).ok()?;
    let doc: Value = serde_json::from_str(&raw).ok()?;
    if !doc.is_object() {
        return None;
    }

    let mut risks = Vec::new();
    if let Some(arr) = doc["risks"].as_array() {
        for r in arr {
            if !r.is_object() {
                continue;
            }
            let severity = single_line(r.get("severity").and_then(Value::as_str).unwrap_or(""));
            let category = single_line(
                r.get("category")
                    .or_else(|| r.get("label"))
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            let mut path_s = single_line(
                r.get("file")
                    .or_else(|| r.get("path"))
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            let mut line = r
                .get("line")
                .or_else(|| r.get("startLine"))
                .and_then(Value::as_u64);
            if path_s.is_empty() {
                if let Some(loc) = r.get("location").and_then(Value::as_str) {
                    let loc = single_line(loc);
                    if let Some((f, l)) = loc.rsplit_once(':') {
                        if let Ok(n) = l.parse::<u64>() {
                            path_s = f.to_string();
                            if line.is_none() {
                                line = Some(n);
                            }
                        } else {
                            path_s = loc;
                        }
                    } else {
                        path_s = loc;
                    }
                }
            }
            let added = r.get("added").and_then(Value::as_u64).unwrap_or(0);
            let deleted = r.get("deleted").and_then(Value::as_u64).unwrap_or(0);
            let reason = single_line(
                r.get("reason")
                    .or_else(|| r.get("summary"))
                    .or_else(|| r.get("detail"))
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            let source = single_line(r.get("source").and_then(Value::as_str).unwrap_or(""));
            risks.push(CardRiskEntry {
                severity,
                category,
                path: path_s,
                line,
                added,
                deleted,
                reason,
                source,
            });
        }
    }

    let rule_hits = doc["riskStats"]
        .get("ruleHits")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| risks.iter().filter(|r| r.source.contains("规则")).count() as u64)
        as usize;
    let reviewer_hints = doc["riskStats"]
        .get("reviewerHints")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| risks.iter().filter(|r| r.source.contains("审查员")).count() as u64)
        as usize;

    let tests_added = doc["tests"].get("added").and_then(Value::as_u64).unwrap_or(0);
    let tests_modified = doc["tests"]
        .get("modified")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tests_deleted = doc["tests"]
        .get("deleted")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let review_verdict = single_line(doc["review"].get("verdict").and_then(Value::as_str).unwrap_or(""));
    let review_summary = single_line(
        doc["review"]
            .get("conclusion")
            .or_else(|| doc["review"].get("summary"))
            .or_else(|| doc["review"].get("headline"))
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let review_round = doc["review"].get("round").and_then(Value::as_u64).unwrap_or(0);
    let fast_forward = doc.get("fastForward").and_then(Value::as_bool);

    let accept_ok = doc["accept"].get("ok").and_then(Value::as_bool);
    let accept_label = single_line(
        doc["accept"]
            .get("label")
            .or_else(|| doc["accept"].get("command"))
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let (accept_layers_passed, accept_layers_total) = doc["accept"]["layers"]
        .as_array()
        .map(|arr| {
            let passed = arr
                .iter()
                .filter(|l| l.get("status").and_then(Value::as_str) == Some("passed"))
                .count();
            (passed, arr.len())
        })
        .unwrap_or((0, 0));

    Some(RunCardL1 {
        risks,
        rule_hits,
        reviewer_hints,
        tests_added,
        tests_modified,
        tests_deleted,
        review_verdict,
        review_summary,
        review_round,
        fast_forward,
        accept_ok,
        accept_label,
        accept_layers_passed,
        accept_layers_total,
    })
}

fn parse_premerge(v: Option<&Value>) -> Option<PremergeInfo> {
    let obj = v.filter(|x| x.is_object())?;
    let status = obj
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if status.is_empty() || matches!(status.as_str(), "unknown" | "crashed") {
        return None;
    }
    let base_short = obj
        .get("baseShort")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let bh = obj.get("baseHead").and_then(Value::as_str).unwrap_or("");
            bh[..bh.len().min(7)].to_string()
        });
    let conflicts = string_list(obj.get("conflicts"));
    let conflict_count = obj
        .get("conflictCount")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(conflicts.len());
    let failed_layer = obj
        .get("failedLayer")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let log = obj
        .get("log")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(PremergeInfo {
        status,
        base_short,
        conflicts,
        conflict_count,
        failed_layer,
        log,
    })
}

fn is_zero_change_delivery(v: &Value, by_id: &HashMap<String, &Value>) -> bool {
    if !matches!(
        v.get("state").and_then(Value::as_str),
        Some("delivered" | "answered")
    ) {
        return false;
    }
    if v.get("mode").and_then(Value::as_str) != Some("write") {
        return false;
    }
    if let Some(files) = v
        .get("pendingChanges")
        .and_then(|p| p.get("files"))
        .and_then(Value::as_u64)
    {
        return files == 0;
    }
    let cur_changes = v
        .get("changes")
        .and_then(|c| c.get("files"))
        .and_then(Value::as_u64);
    if cur_changes.is_some_and(|n| n > 0) {
        return false;
    }
    let wt = v.get("worktree").and_then(Value::as_str).unwrap_or("");
    let mut parent = v
        .get("parent")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut visited = HashSet::new();
    while !parent.is_empty() && visited.insert(parent.clone()) {
        let Some(anc) = by_id.get(&parent) else {
            break;
        };
        if anc.get("applied").and_then(Value::as_bool).unwrap_or(false) {
            break;
        }
        if !wt.is_empty() && anc.get("worktree").and_then(Value::as_str).unwrap_or("") != wt {
            break;
        }
        if anc
            .get("pendingChanges")
            .and_then(|p| p.get("files"))
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 0)
        {
            return false;
        }
        if anc
            .get("changes")
            .and_then(|c| c.get("files"))
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 0)
        {
            return false;
        }
        parent = anc
            .get("parent")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
    }
    if let Some(files) = cur_changes {
        return files == 0;
    }
    if let Some(files) = v.get("files").and_then(Value::as_array) {
        return files.is_empty();
    }
    false
}

/// Shared actionable-pending rule (mirrored in `delegate` hook kernel):
/// - Counted: unapplied write delivery (non-zero-change, not superseded); questioning run;
///   failed/rejected/terminal-error run whose result has not been read (`resultReadAt` empty);
///   read-only `answered` (or zero-change delivery) whose result has not been read.
/// - Excluded: read-only `answered` with `resultReadAt` (goes to `Collected`/已收取);
///   `cleaned`; superseded rounds in a continuation chain; applied; stopped; active runs.
#[allow(clippy::too_many_arguments)]
pub fn is_run_pending_actionable(
    state: &str,
    mode: &str,
    applied: bool,
    cleaned: bool,
    question: bool,
    result_read: bool,
    superseded: bool,
    zero_change: bool,
) -> bool {
    if superseded || cleaned || state == "cleaned" || applied {
        return false;
    }
    if matches!(
        state,
        "running" | "starting" | "waiting" | "queued" | "scouting" | "stopped" | "blocked_mainline"
    ) {
        return false;
    }
    if question {
        return true;
    }
    if matches!(state, "delivered" | "answered") && mode == "write" {
        return if zero_change { !result_read } else { true };
    }
    !result_read
}

#[allow(clippy::too_many_arguments)]
fn classify_group(
    state: &str,
    mode: &str,
    applied: bool,
    cleaned: bool,
    question: bool,
    result_read: bool,
    superseded: bool,
    zero_change: bool,
    has_queue: bool,
) -> GroupKind {
    match state {
        "waiting" | "queued" | "blocked_mainline" => GroupKind::Queued,
        "running" | "starting" | "scouting" => {
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
        "cleaned" => GroupKind::Collected,
        _ => {
            if cleaned {
                GroupKind::Collected
            } else if applied {
                GroupKind::MergedUncleaned
            } else if is_run_pending_actionable(
                state,
                mode,
                applied,
                cleaned,
                question,
                result_read,
                superseded,
                zero_change,
            ) {
                GroupKind::Pending
            } else {
                GroupKind::Collected
            }
        }
    }
}

fn format_model_and_thinking(v: &Value) -> (String, Option<String>) {
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
    let short = raw.rsplit('/').next().unwrap_or(raw).replace("gemini-4-", "");

    let explicit_thinking = v
        .get("thinking")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let lower = short.to_ascii_lowercase();
    for (suffix, level) in [
        ("-xhigh", "xhigh"),
        ("-high", "high"),
        ("-medium", "medium"),
        ("-med", "medium"),
        ("-low", "low"),
        ("-minimal", "minimal"),
        ("-min", "minimal"),
        ("-off", "off"),
    ] {
        if lower.ends_with(suffix) && short.len() > suffix.len() {
            let base = short[..short.len() - suffix.len()].to_string();
            return (base, explicit_thinking.or_else(|| Some(level.to_string())));
        }
    }

    (short, explicit_thinking)
}

fn parse_review(rev_val: Option<&Value>, sign_val: Option<&Value>) -> Option<ReviewInfo> {
    let obj = rev_val.filter(|v| v.is_object())?;
    let status = obj
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("done")
        .to_string();
    let verdict = obj
        .get("verdict")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if status.is_empty() && verdict.is_none() {
        return None;
    }
    let cr = obj.get("controllerReview").filter(|v| v.is_object());
    let sign = sign_val.filter(|v| v.is_object());
    let controller_paths = sign
        .and_then(|s| s.get("paths"))
        .map(|p| string_list(Some(p)))
        .filter(|l| !l.is_empty())
        .or_else(|| cr.map(|c| string_list(c.get("paths"))))
        .unwrap_or_default();
    let sign_hash = sign
        .and_then(|s| s.get("hash"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            cr.and_then(|c| c.get("hash"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string);
    let sign_required = sign
        .and_then(|s| s.get("required"))
        .and_then(Value::as_bool)
        .unwrap_or(!controller_paths.is_empty());
    let signed = sign
        .and_then(|s| s.get("signed"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let sign_valid = sign
        .and_then(|s| s.get("valid"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let sign_invalidated = sign
        .and_then(|s| s.get("invalidated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(ReviewInfo {
        status,
        verdict,
        round: obj.get("round").and_then(Value::as_u64).unwrap_or(0),
        rework_rounds: obj.get("reworkRounds").and_then(Value::as_u64).unwrap_or(0),
        needs_attention: obj
            .get("needsAttention")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        upgraded_by: obj
            .get("upgradedBy")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        verdict_file: obj
            .get("verdictFile")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        controller_paths,
        sign_hash,
        sign_required,
        signed,
        sign_valid,
        sign_invalidated,
    })
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
    Ok(parse_lane_value(&v))
}

fn parse_lane_value(v: &Value) -> LaneStatus {
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
                rank: e.get("rank").and_then(Value::as_u64).map(|n| n as usize),
                used: e.get("used").and_then(Value::as_u64).map(|n| n as usize),
                share: e.get("share").and_then(Value::as_u64).map(|n| n as usize),
            })
            .collect()
    };
    let pool = v.get("pool").and_then(Value::as_object);
    let leases = pool
        .and_then(|p| p.get("leases"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|l| {
            Some(LaneLease {
                rank: l.get("rank").and_then(Value::as_u64)? as usize,
                used: l.get("used").and_then(Value::as_u64)? as usize,
                share: l.get("share").and_then(Value::as_u64)? as usize,
            })
        })
        .collect();
    LaneStatus {
        max_heavy: v.get("maxHeavy").and_then(Value::as_u64).map(|n| n as usize),
        heavy_jobs: v
            .get("heavyJobs")
            .and_then(Value::as_u64)
            .map(|n| n as usize),
        threads: v.get("threads").and_then(Value::as_u64).map(|n| n as usize),
        budget: v.get("budget").and_then(Value::as_u64).map(|n| n as usize),
        heavy_jobs_explicit: v
            .get("heavyJobsExplicit")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        pool_total: pool
            .and_then(|p| p.get("total"))
            .and_then(Value::as_u64)
            .map(|n| n as usize),
        pool_target: pool
            .and_then(|p| p.get("target"))
            .and_then(Value::as_u64)
            .map(|n| n as usize),
        pool_free: pool
            .and_then(|p| p.get("free"))
            .and_then(Value::as_u64)
            .map(|n| n as usize),
        leases,
        concurrency_summary: v
            .get("concurrencySummary")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(single_line),
        remote_max_heavy: v
            .get("remoteMaxHeavy")
            .and_then(Value::as_u64)
            .map(|n| n as usize),
        local: parse_entries("local", false),
        remote: parse_entries("remote", true),
    }
}

/// Parse plain-text `delegate lane` output (works on all delegate versions).
pub fn parse_lane_text(raw: &str) -> LaneStatus {
    let mut local = Vec::new();
    let mut remote = Vec::new();
    let mut leases = Vec::new();
    let mut concurrency_summary = None;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("线程 ") && concurrency_summary.is_none() {
            concurrency_summary = Some(single_line(trimmed));
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
        let rest_label = parts.next().unwrap_or("").trim_start();
        let (rank, used, share, label) = if !is_remote && running && rest_label.starts_with('#') {
            let mut tokens = rest_label.splitn(3, char::is_whitespace);
            let r_tok = tokens.next().unwrap_or("");
            let u_tok = tokens.next().unwrap_or("");
            let lbl = tokens.next().unwrap_or("").trim_start();
            let parsed_rank = r_tok.strip_prefix('#').and_then(|s| s.parse::<usize>().ok());
            let parsed_pair = u_tok.split_once('/').and_then(|(u, sh)| {
                Some((u.parse::<usize>().ok()?, sh.parse::<usize>().ok()?))
            });
            if let (Some(r), Some((u, sh))) = (parsed_rank, parsed_pair) {
                leases.push(LaneLease {
                    rank: r,
                    used: u,
                    share: sh,
                });
                (Some(r), Some(u), Some(sh), single_line(lbl))
            } else {
                (None, None, None, single_line(rest_label))
            }
        } else {
            (None, None, None, single_line(rest_label))
        };
        let entry = LaneEntry {
            label,
            since,
            running,
            remote: is_remote,
            rank,
            used,
            share,
        };
        if is_remote {
            remote.push(entry);
        } else {
            local.push(entry);
        }
    }
    LaneStatus {
        max_heavy: None,
        heavy_jobs: None,
        threads: None,
        budget: None,
        heavy_jobs_explicit: false,
        pool_total: None,
        pool_target: None,
        pool_free: None,
        leases,
        concurrency_summary,
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

pub fn local_ymd() -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return String::new();
        }
        format!(
            "{:04}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        )
    }
}

pub fn parse_work_value(val: &Value) -> Vec<WorkItem> {
    let Some(arr) = val.as_array() else {
        return Vec::new();
    };
    arr.iter().filter_map(parse_single_work_item).collect()
}

pub fn parse_work_jsonl(raw: &str) -> Vec<WorkItem> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            if let Some(item) = parse_single_work_item(&v) {
                out.push(item);
            }
        }
    }
    out
}

fn parse_single_work_item(v: &Value) -> Option<WorkItem> {
    let id = v.get("id").and_then(Value::as_str)?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let repo = v.get("repo").and_then(Value::as_str).unwrap_or("").to_string();
    let repo_label = v
        .get("repoLabel")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if repo.is_empty() {
                "-".to_string()
            } else {
                Path::new(&repo)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&repo)
                    .to_string()
            }
        });
    let owner = v
        .get("owner")
        .filter(|o| o.is_object())
        .and_then(|o| {
            let kind = OwnerKind::parse(o.get("kind").and_then(Value::as_str)?)?;
            Some(WorkOwnerInfo {
                kind,
                ask: single_line(o.get("ask").and_then(Value::as_str).unwrap_or("")),
                recommendation: single_line(
                    o.get("recommendation").and_then(Value::as_str).unwrap_or(""),
                ),
                if_undecided: single_line(
                    o.get("ifUndecided").and_then(Value::as_str).unwrap_or(""),
                ),
                cost: single_line(o.get("cost").and_then(Value::as_str).unwrap_or("")),
            })
        });
    let wait_on_status = v
        .get("waitOnStatus")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|w| {
            let condition = w.get("condition").and_then(Value::as_str)?.to_string();
            Some(WorkWaitStatus {
                condition,
                satisfied: w.get("satisfied").and_then(Value::as_bool).unwrap_or(false),
                detail: single_line(w.get("detail").and_then(Value::as_str).unwrap_or("")),
                pointer: w.get("pointer").and_then(Value::as_str).unwrap_or("").to_string(),
            })
        })
        .collect();
    let followups = v
        .get("followups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, f)| {
            let text = f.get("text").and_then(Value::as_str)?.trim();
            if text.is_empty() {
                return None;
            }
            Some(WorkFollowup {
                id: f.get("id").and_then(Value::as_u64).map(|x| x as usize).unwrap_or(i + 1),
                kind: f
                    .get("kind")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("note")
                    .to_string(),
                text: single_line(text),
                anchor: f.get("anchor").and_then(Value::as_str).unwrap_or("").to_string(),
                source_run: f
                    .get("sourceRun")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                done: f.get("done").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect();
    let artifacts = v
        .get("artifacts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| {
            if let Some(p) = a.as_str() {
                return (!p.trim().is_empty()).then(|| WorkArtifact {
                    path: p.trim().to_string(),
                    source_run: String::new(),
                    note: String::new(),
                });
            }
            let path = a.get("path").and_then(Value::as_str)?.trim();
            if path.is_empty() {
                return None;
            }
            Some(WorkArtifact {
                path: path.to_string(),
                source_run: a
                    .get("sourceRun")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                note: single_line(a.get("note").and_then(Value::as_str).unwrap_or("")),
            })
        })
        .collect();
    let ad = v.get("autoDispatch").filter(|x| x.is_object());
    Some(WorkItem {
        id,
        title: single_line(v.get("title").and_then(Value::as_str).unwrap_or("")),
        repo,
        repo_label,
        state: v
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("ready")
            .to_string(),
        closed_kind: v
            .get("closedKind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        done_when: v
            .get("doneWhen")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        wait_on: string_list(v.get("waitOn")),
        verify_on: string_list(v.get("verifyOn")),
        wait_on_status,
        runs: string_list(v.get("runs")),
        artifacts,
        brief: v.get("brief").and_then(Value::as_str).unwrap_or("").to_string(),
        followups,
        owner,
        owner_ref: v
            .get("ownerRef")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        auto_dispatch_prompt: ad
            .and_then(|x| x.get("promptFile"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        auto_dispatch_run: ad
            .and_then(|x| x.get("dispatchedRun"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        defer_reason: single_line(
            v.get("deferReason").and_then(Value::as_str).unwrap_or(""),
        ),
        defer_until: v
            .get("deferUntil")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        defer_streak: v.get("deferStreak").and_then(Value::as_u64).unwrap_or(0) as u32,
        created_at: v
            .get("createdAt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        updated_at: v
            .get("updatedAt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        state_since: v
            .get("stateSince")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        close_reason: single_line(
            v.get("closeReason").and_then(Value::as_str).unwrap_or(""),
        ),
        close_evidence: single_line(
            v.get("closeEvidence").and_then(Value::as_str).unwrap_or(""),
        ),
        focus: single_line(v.get("focus").and_then(Value::as_str).unwrap_or("")),
        next: single_line(v.get("next").and_then(Value::as_str).unwrap_or("")),
        applied: v.get("applied").and_then(Value::as_bool).unwrap_or(false),
        ephemeral: v.get("ephemeral").and_then(Value::as_bool).unwrap_or(false),
        eta: parse_work_eta_value(v.get("eta")),
    })
}

fn opt_nonempty_str(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn parse_run_eta_value(val: Option<&Value>) -> Option<RunEtaInfo> {
    let v = val.filter(|x| x.is_object())?;
    let del = v.get("toDelivered").filter(|x| x.is_object());
    let app = v.get("toApplied").filter(|x| x.is_object());
    let range = del.or(app);
    let blk = v.get("blocker").filter(|x| x.is_object());
    Some(RunEtaInfo {
        stage: v.get("stage").and_then(Value::as_str).unwrap_or("").to_string(),
        stage_label: v
            .get("stageLabel")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        stopped_clock: v
            .get("stoppedClock")
            .and_then(Value::as_bool)
            .unwrap_or(blk.is_some()),
        short_text: single_line(v.get("shortText").and_then(Value::as_str).unwrap_or("")),
        compact_badge: single_line(v.get("compactBadge").and_then(Value::as_str).unwrap_or("")),
        detail_text: single_line(v.get("detailText").and_then(Value::as_str).unwrap_or("")),
        blocker_kind: opt_nonempty_str(blk.and_then(|b| b.get("kind"))),
        blocker_who: opt_nonempty_str(blk.and_then(|b| b.get("who"))),
        blocker_label: opt_nonempty_str(blk.and_then(|b| b.get("label"))),
        p50_clock: opt_nonempty_str(range.and_then(|r| r.get("p50Clock"))),
        p75_clock: opt_nonempty_str(range.and_then(|r| r.get("p75Clock"))),
        anomaly: range
            .and_then(|r| r.get("anomaly"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        is_prior: v.get("isPrior").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn parse_work_eta_value(val: Option<&Value>) -> Option<WorkEtaInfo> {
    let v = val.filter(|x| x.is_object())?;
    let rem = v.get("remaining").filter(|x| x.is_object());
    let blk = v.get("blocker").filter(|x| x.is_object());
    let focus_badge = single_line(v.get("focusBadge").and_then(Value::as_str).unwrap_or(""));
    let compact_raw = single_line(v.get("compactBadge").and_then(Value::as_str).unwrap_or(""));
    let compact_badge = if !compact_raw.is_empty() {
        compact_raw
    } else if let Some(c) = opt_nonempty_str(blk.and_then(|b| b.get("compact"))) {
        c
    } else {
        focus_badge.clone()
    };
    Some(WorkEtaInfo {
        stopped_clock: v
            .get("stoppedClock")
            .and_then(Value::as_bool)
            .unwrap_or(blk.is_some()),
        short_text: single_line(v.get("shortText").and_then(Value::as_str).unwrap_or("")),
        focus_badge,
        compact_badge,
        blocker_kind: opt_nonempty_str(blk.and_then(|b| b.get("kind"))),
        blocker_label: opt_nonempty_str(blk.and_then(|b| b.get("label"))),
        p50_clock: opt_nonempty_str(rem.and_then(|r| r.get("p50Clock"))),
        p75_clock: opt_nonempty_str(rem.and_then(|r| r.get("p75Clock"))),
        is_prior: v.get("isPrior").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn parse_dag_eta_summary(val: Option<&Value>) -> WorkDagEtaSummary {
    let Some(v) = val.filter(|x| x.is_object()) else {
        return WorkDagEtaSummary::default();
    };
    WorkDagEtaSummary {
        write_chains: v.get("writeChains").and_then(Value::as_u64).unwrap_or(0) as usize,
        depth: v.get("depth").and_then(Value::as_u64).unwrap_or(0) as usize,
        active_count: v.get("activeCount").and_then(Value::as_u64).unwrap_or(0) as usize,
        dev_wall_range: single_line(v.get("devWallRange").and_then(Value::as_str).unwrap_or("")),
        earliest_finish_p50_clock: opt_nonempty_str(v.get("earliestFinishP50Clock")),
        earliest_finish_p75_clock: opt_nonempty_str(v.get("earliestFinishP75Clock")),
        external_wait_count: v
            .get("externalWaitCount")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
        deferred_count: v
            .get("deferredCount")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
        earliest_deferred_clock: opt_nonempty_str(v.get("earliestDeferredClock")),
        date_wait_count: v
            .get("dateWaitCount")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
        earliest_date_wait_clock: opt_nonempty_str(v.get("earliestDateWaitClock")),
        owner_blocked_count: v
            .get("ownerBlocked")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        blocker_count: v
            .get("blockers")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        headline: single_line(v.get("headline").and_then(Value::as_str).unwrap_or("")),
        is_prior: v.get("isPrior").and_then(Value::as_bool).unwrap_or(false),
    }
}

fn parse_disk_volume(val: &Value) -> Option<DiskVolumeSnapshot> {
    let obj = val.as_object()?;
    let pct_f = obj.get("usedPct").and_then(Value::as_f64).unwrap_or(0.0);
    let used_pct_tenths = (pct_f.clamp(0.0, 100.0) * 10.0).round() as u32;
    Some(DiskVolumeSnapshot {
        root: obj
            .get("root")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        total_bytes: obj.get("totalBytes").and_then(Value::as_u64).unwrap_or(0),
        used_bytes: obj.get("usedBytes").and_then(Value::as_u64).unwrap_or(0),
        avail_bytes: obj.get("availBytes").and_then(Value::as_u64).unwrap_or(0),
        used_pct_tenths,
        level: obj
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("ok")
            .to_string(),
    })
}

fn parse_disk_health_value(val: &Value) -> Option<DiskHealthSnapshot> {
    let obj = val.as_object()?;
    let overflow_slots = obj
        .get("overflowSlots")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let stale_runs = obj
        .get("staleRuns")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let o = item.as_object()?;
                    Some(StaleSlotRunItem {
                        run: o.get("run").and_then(Value::as_str).unwrap_or("").to_string(),
                        name: single_line(o.get("name").and_then(Value::as_str).unwrap_or("")),
                        slot: o.get("slot").and_then(Value::as_str).unwrap_or("").to_string(),
                        idle_hours: o.get("idleHours").and_then(Value::as_u64).unwrap_or(0),
                        bytes: o.get("bytes").and_then(Value::as_u64).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(DiskHealthSnapshot {
        configured: obj.get("configured").and_then(Value::as_bool).unwrap_or(false),
        level: obj
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("ok")
            .to_string(),
        primary: obj.get("primary").and_then(parse_disk_volume),
        overflow: obj.get("overflow").and_then(parse_disk_volume),
        total_slots: obj
            .get("totalSlots")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
        overflow_slots,
        stale_runs,
    })
}

fn parse_snapshot_eta_value(val: &Value) -> SnapshotEta {
    if !val.is_object() {
        return SnapshotEta::default();
    }
    let wave = val
        .get("wave")
        .filter(|x| x.is_object())
        .map(|w| {
            let blockers = w
                .get("blockers")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|b| {
                            if !b.is_object() {
                                return None;
                            }
                            Some(WaveBlockerBrief {
                                run: b.get("run").and_then(Value::as_str).unwrap_or("").to_string(),
                                name: single_line(
                                    b.get("name").and_then(Value::as_str).unwrap_or(""),
                                ),
                                kind: b.get("kind").and_then(Value::as_str).unwrap_or("").to_string(),
                                compact: single_line(
                                    b.get("compact").and_then(Value::as_str).unwrap_or(""),
                                ),
                                label: single_line(
                                    b.get("label").and_then(Value::as_str).unwrap_or(""),
                                ),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            WaveEtaSummary {
                active_count: w.get("activeCount").and_then(Value::as_u64).unwrap_or(0) as usize,
                running_count: w.get("runningCount").and_then(Value::as_u64).unwrap_or(0) as usize,
                queued_count: w.get("queuedCount").and_then(Value::as_u64).unwrap_or(0) as usize,
                blocked_count: w.get("blockedCount").and_then(Value::as_u64).unwrap_or(0) as usize,
                earliest_p50_clock: opt_nonempty_str(w.get("earliestP50Clock")),
                wave_p50_clock: opt_nonempty_str(w.get("waveP50Clock")),
                wave_p75_clock: opt_nonempty_str(w.get("waveP75Clock")),
                is_prior: w.get("isPrior").and_then(Value::as_bool).unwrap_or(false),
                blockers,
            }
        })
        .unwrap_or_default();
    SnapshotEta {
        wave,
        work: parse_dag_eta_summary(val.get("work")),
        queue: parse_dag_eta_summary(val.get("queue")),
    }
}

/// Pure read-only load of a work item's brief/card markdown file (zero subprocesses, zero writes).
pub fn read_work_brief_markdown(item: &WorkItem) -> Option<Result<String, String>> {
    let brief = item.brief.trim();
    if brief.is_empty() {
        return None;
    }
    let p = PathBuf::from(brief);
    let resolved = if p.is_absolute() {
        p
    } else if !item.repo.is_empty() {
        Path::new(&item.repo).join(&p)
    } else {
        p
    };
    let meta = match fs::metadata(&resolved) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return Some(Err(format!("路径不是普通文件：{}", resolved.display()))),
        Err(_) => return Some(Err(format!("卡片文件不存在：{}", resolved.display()))),
    };
    const MAX_BYTES: u64 = 256 * 1024;
    if meta.len() > MAX_BYTES {
        return Some(Err(format!(
            "卡片文件过大（{} KB > 256 KB）：{}",
            meta.len() / 1024,
            resolved.display()
        )));
    }
    Some(
        fs::read_to_string(&resolved)
            .map_err(|e| format!("无法读取卡片 {}: {e}", resolved.display())),
    )
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

/// Replace raw run IDs in event summaries and running agy processes with human-readable
/// run/chain display names (resolving reply runs like `20261125-115816-7ddc` to their chain
/// name or `reply to <name>`).
fn enrich_run_names_from_groups(
    groups: &[RepoGroup],
    events: &mut [EventItem],
    agy_accounts: &mut AgyAccountsState,
) {
    let mut id_to_display: HashMap<&str, String> = HashMap::new();
    for g in groups {
        for r in &g.runs {
            let label = if !r.display_name.is_empty() {
                r.display_name.clone()
            } else if !r.name.is_empty() {
                r.name.clone()
            } else {
                strip_run_id_timestamp(&r.run).to_string()
            };
            id_to_display.insert(r.run.as_str(), label.clone());
            for cid in &r.chain_ids {
                id_to_display.entry(cid.as_str()).or_insert_with(|| label.clone());
            }
        }
    }
    for ev in events.iter_mut() {
        if ev.run.is_empty() {
            continue;
        }
        let resolved = id_to_display
            .get(ev.run.as_str())
            .cloned()
            .unwrap_or_else(|| {
                let stripped = strip_run_id_timestamp(&ev.run);
                if stripped != ev.run && !is_short_hex_hash(stripped) {
                    stripped.to_string()
                } else {
                    ev.run.clone()
                }
            });
        if resolved != ev.run {
            if ev.summary == ev.run {
                ev.summary = resolved;
            } else if let Some(rest) = ev.summary.strip_prefix(&format!("{} · ", ev.run)) {
                ev.summary = format!("{resolved} · {rest}");
            }
        }
    }
    for proc in &mut agy_accounts.running_agy {
        if let Some(ref rid) = proc.run {
            if let Some(disp) = id_to_display.get(rid.as_str()) {
                proc.run_name = Some(disp.clone());
            } else if proc.run_name.is_none() {
                let stripped = strip_run_id_timestamp(rid);
                if stripped != rid && !is_short_hex_hash(stripped) {
                    proc.run_name = Some(stripped.to_string());
                }
            }
        }
    }
}

fn is_short_hex_hash(s: &str) -> bool {
    (4..=8).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn parse_events_jsonl(raw: &str) -> Vec<EventItem> {
    let vals: Vec<Value> = raw
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if !trimmed.starts_with('{') {
                return None;
            }
            serde_json::from_str::<Value>(trimmed).ok()
        })
        .collect();
    parse_events_values(&vals)
}

fn parse_events_values(items: &[Value]) -> Vec<EventItem> {
    let mut out = Vec::with_capacity(items.len());
    for v in items {
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
        "agy_account_auto" => {
            let from = ps("from");
            let to = ps("to");
            let reason = ps("reason");
            if reason.is_empty() {
                format!("{from} → {to}")
            } else {
                format!("{from} → {to}（{reason}）")
            }
        }
        "agy_account_handoff" => {
            let from = ps("from");
            let to = ps("to");
            let reason = ps("reason");
            if reason.is_empty() {
                format!("{from} → {to}")
            } else {
                format!("{from} → {to}（{reason}）")
            }
        }
        "queue_dispatched" => format!("-> {}", ps("nextRun")),
        "queue_paused" => ps("reason").to_string(),
        "review_started" => {
            let round = p.get("round").and_then(Value::as_u64).unwrap_or(1);
            let inc = if p.get("incremental").and_then(Value::as_bool) == Some(true) {
                " 增量"
            } else {
                ""
            };
            format!("审查中 第{round}轮{inc}")
        }
        "review_verdict" => {
            let v = ps("verdict");
            let round = p.get("round").and_then(Value::as_u64).unwrap_or(0);
            let up = ps("upgradedBy");
            let mut s = if round > 0 {
                format!("{v}（第{round}轮）")
            } else {
                v.to_string()
            };
            if !up.is_empty() {
                s.push_str(&format!(" [{up}]"));
            }
            s
        }
        "rework_sent" => {
            let round = p
                .get("reworkRounds")
                .or_else(|| p.get("round"))
                .and_then(Value::as_u64)
                .unwrap_or(1);
            format!("退回返工 第{round}轮")
        }
        "needs_attention" => {
            let det = ps("detail");
            if det.is_empty() {
                ps("reason").to_string()
            } else {
                det.to_string()
            }
        }
        "applied" => {
            let head = ps("head");
            if head.is_empty() {
                "已合入".into()
            } else {
                format!("合入 {}", &head[..head.len().min(7)])
            }
        }
        "merge_started" => {
            let cnt = p.get("count").and_then(Value::as_u64).unwrap_or(0);
            format!("串行合入 {cnt} 个 run")
        }
        "merge_item" => {
            let src = ps("sourceRun");
            if ps("status") == "MERGED" {
                let tag = if p.get("autoMerge").and_then(Value::as_bool) == Some(true) {
                    "可自主合入"
                } else {
                    "待主控点头"
                };
                format!("已合入 {src}（{tag}）")
            } else {
                format!("跳过 {src}：{}", ps("reason"))
            }
        }
        "merge_done" => {
            let m = p.get("mergedCount").and_then(Value::as_u64).unwrap_or(0);
            let s = p.get("skippedCount").and_then(Value::as_u64).unwrap_or(0);
            let a = p.get("autoMergeCount").and_then(Value::as_u64).unwrap_or(0);
            format!("合入 {m} · 跳过 {s} · 自主 {a}")
        }
        "premerge_ready" => {
            let bs = ps("baseShort");
            let short = if !bs.is_empty() {
                bs
            } else {
                let bh = ps("baseHead");
                &bh[..bh.len().min(7)]
            };
            format!("快进 ✓（基于 {short}）")
        }
        "premerge_conflict" => {
            let files = string_list(p.get("conflicts"));
            if files.is_empty() {
                "预合并冲突".into()
            } else {
                format!("预合并冲突：{}", files.join("、"))
            }
        }
        "premerge_failed" => {
            let layer = ps("failedLayer");
            if !layer.is_empty() {
                format!("预合并失败（{layer}）")
            } else {
                format!("预合并失败：{}", ps("error"))
            }
        }
        "radar_unchanged" => "指纹未变化（0 token）".into(),
        "radar_done" => {
            let added = p.get("added").and_then(Value::as_u64).unwrap_or(0);
            let total = p.get("total").and_then(Value::as_u64).unwrap_or(0);
            format!("新增 {added} · 共 {total} 条")
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
    let vals: Vec<Value> = raw
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if !trimmed.starts_with('{') {
                return None;
            }
            serde_json::from_str::<Value>(trimmed).ok()
        })
        .collect();
    parse_dag_queue_values(&vals)
}

fn parse_dag_queue_values(items: &[Value]) -> Vec<DagQueueItem> {
    let mut out = Vec::with_capacity(items.len());
    for v in items {
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

pub fn parse_merges_jsonl(raw: &str) -> Vec<MergeRunItem> {
    let vals: Vec<Value> = raw
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if !trimmed.starts_with('{') {
                return None;
            }
            serde_json::from_str::<Value>(trimmed).ok()
        })
        .collect();
    parse_merges_values(&vals)
}

fn parse_merges_values(items: &[Value]) -> Vec<MergeRunItem> {
    let mut out = Vec::with_capacity(items.len());
    for v in items {
        let Some(merge_run) = v.get("mergeRun").and_then(Value::as_str) else {
            continue;
        };
        let repo_str = v.get("repo").and_then(Value::as_str).unwrap_or("");
        let repo_path = PathBuf::from(repo_str);
        let repo_label = repo_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(repo_str)
            .to_string();
        let ma = &v["mergeAccept"];
        let items = v
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|it| MergeItemEntry {
                run: it.get("run").and_then(Value::as_str).unwrap_or("").to_string(),
                name: single_line(it.get("name").and_then(Value::as_str).unwrap_or("")),
                status: it.get("status").and_then(Value::as_str).unwrap_or("").to_string(),
                commit: it.get("commit").and_then(Value::as_str).unwrap_or("").to_string(),
                title: single_line(it.get("title").and_then(Value::as_str).unwrap_or("")),
                accept_ok: it.get("acceptOk").and_then(Value::as_bool),
                auto_merge: it.get("autoMerge").and_then(Value::as_bool).unwrap_or(false),
                reason: single_line(it.get("reason").and_then(Value::as_str).unwrap_or("")),
            })
            .collect();
        out.push(MergeRunItem {
            merge_run: merge_run.to_string(),
            name: single_line(v.get("name").and_then(Value::as_str).unwrap_or(merge_run)),
            repo_path,
            repo_label,
            base_head: v.get("baseHead").and_then(Value::as_str).unwrap_or("").to_string(),
            head: v.get("head").and_then(Value::as_str).unwrap_or("").to_string(),
            state: v.get("state").and_then(Value::as_str).unwrap_or("").to_string(),
            started_at: v.get("startedAt").and_then(Value::as_str).unwrap_or("").to_string(),
            finished_at: v.get("finishedAt").and_then(Value::as_str).unwrap_or("").to_string(),
            merged_count: v.get("mergedCount").and_then(Value::as_u64).unwrap_or(0),
            skipped_count: v.get("skippedCount").and_then(Value::as_u64).unwrap_or(0),
            auto_merge_count: v.get("autoMergeCount").and_then(Value::as_u64).unwrap_or(0),
            all_auto_merge: v.get("allAutoMerge").and_then(Value::as_bool).unwrap_or(false),
            merge_accept_ok: ma.get("ok").and_then(Value::as_bool).unwrap_or(false),
            merge_accept_skipped: ma.get("skipped").and_then(Value::as_bool).unwrap_or(false),
            merge_accept_cmd: single_line(ma.get("command").and_then(Value::as_str).unwrap_or("")),
            applied: v.get("applied").and_then(Value::as_bool).unwrap_or(false),
            report_path: v.get("reportPath").and_then(Value::as_str).unwrap_or("").to_string(),
            items,
        });
    }
    out
}

pub fn parse_candidates_jsonl(raw: &str) -> Vec<RadarCandidateItem> {
    let vals: Vec<Value> = raw
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if !trimmed.starts_with('{') {
                return None;
            }
            serde_json::from_str::<Value>(trimmed).ok()
        })
        .collect();
    parse_candidates_values(&vals)
}

fn parse_candidates_values(items: &[Value]) -> Vec<RadarCandidateItem> {
    let mut out = Vec::with_capacity(items.len());
    for v in items {
        let Some(id) = v.get("id").and_then(Value::as_str) else {
            continue;
        };
        let repo_str = v.get("repo").and_then(Value::as_str).unwrap_or("");
        let repo_path = PathBuf::from(repo_str);
        let repo_label = v
            .get("repoName")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                repo_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(repo_str)
                    .to_string()
            });
        out.push(RadarCandidateItem {
            id: id.to_string(),
            repo_path,
            repo_label,
            title: single_line(v.get("title").and_then(Value::as_str).unwrap_or("")),
            evidence: string_list(v.get("evidence")),
            suggestion: v.get("suggestion").and_then(Value::as_str).unwrap_or("").to_string(),
            cost: single_line(v.get("cost").and_then(Value::as_str).unwrap_or("")),
            brief_ready: v.get("briefReady").and_then(Value::as_bool).unwrap_or(false),
            brief_note: single_line(v.get("briefNote").and_then(Value::as_str).unwrap_or("")),
            fingerprint: v.get("fingerprint").and_then(Value::as_str).unwrap_or("").to_string(),
            first_seen_at: v.get("firstSeenAt").and_then(Value::as_str).unwrap_or("").to_string(),
            last_seen_at: v.get("lastSeenAt").and_then(Value::as_str).unwrap_or("").to_string(),
            picked_at: v.get("pickedAt").and_then(Value::as_str).unwrap_or("").to_string(),
            board_path: v.get("boardPath").and_then(Value::as_str).unwrap_or("").to_string(),
        });
    }
    out
}

pub fn parse_agy_accounts_json(raw: &str) -> AgyAccountsState {
    let Ok(v) = serde_json::from_str::<Value>(raw.trim()) else {
        return AgyAccountsState::default();
    };
    parse_agy_accounts_value(&v)
}

fn parse_usage_window(v: &Value) -> AccountUsageWindow {
    if !v.is_object() {
        return AccountUsageWindow::default();
    }
    let input = v.get("input").and_then(Value::as_u64).unwrap_or(0);
    let output = v.get("output").and_then(Value::as_u64).unwrap_or(0);
    let cache_read = v.get("cacheRead").and_then(Value::as_u64).unwrap_or(0);
    let total = v
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| input.saturating_add(output).saturating_add(cache_read));
    let author_total = v
        .get("author")
        .and_then(|a| a.get("total"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reviewer_total = v
        .get("reviewer")
        .and_then(|r| r.get("total"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    AccountUsageWindow {
        input,
        output,
        cache_read,
        total,
        author_total,
        reviewer_total,
        rounds: v.get("rounds").and_then(Value::as_u64).unwrap_or(0) as usize,
        runs: v.get("runs").and_then(Value::as_u64).unwrap_or(0) as usize,
        live_counted: v.get("liveCounted").and_then(Value::as_u64).unwrap_or(0) as usize,
        uncounted_running: v
            .get("uncountedRunning")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
    }
}

fn parse_u64_vec(v: Option<&Value>) -> Vec<u64> {
    v.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|n| n.as_u64().unwrap_or(0))
        .collect()
}

fn parse_usage_stats(v: &Value) -> AccountUsageStats {
    if !v.is_object() {
        return AccountUsageStats::default();
    }
    AccountUsageStats {
        one_hour: parse_usage_window(&v["oneHour"]),
        five_hour: parse_usage_window(&v["fiveHour"]),
        seven_day: parse_usage_window(&v["sevenDay"]),
        hourly_24h: parse_u64_vec(v.get("hourly24h")),
    }
}

fn parse_real_quota(v: &Value) -> Option<AccountRealQuota> {
    if !v.is_object() {
        return None;
    }
    let fetched_at = v
        .get("fetchedAt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let fetched_epoch = v.get("fetchedEpoch").and_then(Value::as_i64).unwrap_or(0);
    if fetched_at.is_empty() && fetched_epoch == 0 {
        return None;
    }
    let age_seconds = v.get("ageSeconds").and_then(Value::as_u64).unwrap_or(0);
    let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let identity_mismatch = v
        .get("identityMismatch")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let error = v
        .get("error")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(single_line);
    let entries: Vec<QuotaPoolEntry> = v
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let pool = single_line(e.get("pool").and_then(Value::as_str).unwrap_or(""));
            if pool.is_empty() {
                return None;
            }
            let remaining_pct_x100 = pct_to_x100(e.get("remainingPct"))?;
            Some(QuotaPoolEntry {
                pool,
                window: e
                    .get("window")
                    .and_then(Value::as_str)
                    .unwrap_or("other")
                    .to_string(),
                window_label: single_line(
                    e.get("windowLabel").and_then(Value::as_str).unwrap_or(""),
                ),
                remaining_pct_x100,
                reset_at: e
                    .get("resetAt")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                reset_in_seconds: e.get("resetInSeconds").and_then(Value::as_u64),
            })
        })
        .collect();
    let unknown_lines = string_list(v.get("unknownLines"));
    Some(AccountRealQuota {
        fetched_at,
        fetched_epoch,
        age_seconds,
        ok,
        identity_mismatch,
        error,
        entries,
        unknown_lines,
    })
}

fn pct_to_x100(v: Option<&Value>) -> Option<u32> {
    let f = v?.as_f64()?;
    if !f.is_finite() || !(0.0..=100.0).contains(&f) {
        return None;
    }
    Some((f * 100.0).round() as u32)
}

fn parse_quota_note(v: &Value) -> Option<QuotaNoteSnapshot> {
    if !v.is_object() {
        return None;
    }
    let weekly_pct_x100 = pct_to_x100(v.get("weekly"));
    let five_hour_pct_x100 = pct_to_x100(v.get("fiveHour"));
    if weekly_pct_x100.is_none() && five_hour_pct_x100.is_none() {
        return None;
    }
    Some(QuotaNoteSnapshot {
        weekly_pct_x100,
        weekly_at: v
            .get("weeklyAt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        five_hour_pct_x100,
        five_hour_at: v
            .get("fiveHourAt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

fn parse_quota_window_estimate(v: &Value) -> Option<QuotaWindowEstimate> {
    if !v.is_object() {
        return None;
    }
    let status = match v.get("status").and_then(Value::as_str)? {
        "depletes" => QuotaEtaStatus::Depletes,
        "safe" => QuotaEtaStatus::Safe,
        "idle" => QuotaEtaStatus::Idle,
        "insufficient" => QuotaEtaStatus::Insufficient,
        _ => return None,
    };
    let remaining_pct_x100 = pct_to_x100(v.get("remainingPct"));
    let effective_pct_x100 = pct_to_x100(v.get("effectivePct"));
    let alert = v.get("alert").and_then(Value::as_bool).unwrap_or(false);
    let mid_minutes = v.get("midMinutes").and_then(Value::as_u64);
    let fast_minutes = v.get("fastMinutes").and_then(Value::as_u64).or(mid_minutes);
    let slow_minutes = v.get("slowMinutes").and_then(Value::as_u64);
    let reset_in_seconds = v.get("resetInSeconds").and_then(Value::as_u64);
    let running_count = v.get("runningCount").and_then(Value::as_u64).unwrap_or(0) as usize;
    let running_writers = v.get("runningWriters").and_then(Value::as_u64).unwrap_or(0) as usize;
    let basis = v
        .get("basis")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let reason = v
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let elapsed_minutes = v.get("elapsedMinutes").and_then(Value::as_u64);
    let source = v
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let tier = v
        .get("tier")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let pool = v
        .get("pool")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(single_line);
    Some(QuotaWindowEstimate {
        status,
        remaining_pct_x100,
        effective_pct_x100,
        alert,
        fast_minutes,
        mid_minutes,
        slow_minutes,
        reset_in_seconds,
        running_count,
        running_writers,
        basis,
        reason,
        elapsed_minutes,
        source,
        tier,
        pool,
    })
}

fn parse_quota_estimate(v: &Value) -> Option<QuotaEstimate> {
    if !v.is_object() {
        return None;
    }
    let weekly = parse_quota_window_estimate(&v["weekly"]);
    let five_hour = parse_quota_window_estimate(&v["fiveHour"]);
    let pools: Vec<QuotaPoolEstimate> = v
        .get("pools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let pool = single_line(p.get("pool").and_then(Value::as_str).unwrap_or(""));
            if pool.is_empty() {
                return None;
            }
            let pw = parse_quota_window_estimate(&p["weekly"]);
            let pfh = parse_quota_window_estimate(&p["fiveHour"]);
            if pw.is_none() && pfh.is_none() {
                return None;
            }
            Some(QuotaPoolEstimate {
                pool,
                weekly: pw,
                five_hour: pfh,
            })
        })
        .collect();
    if weekly.is_none() && five_hour.is_none() && pools.is_empty() {
        return None;
    }
    Some(QuotaEstimate {
        weekly,
        five_hour,
        pools,
    })
}

pub fn parse_analytics_value(v: &Value) -> AnalyticsSnapshot {
    if !v.is_object() {
        return AnalyticsSnapshot::default();
    }
    let hourly_24h_total = parse_u64_vec(v["hourly24h"].get("total"));
    let mut hourly_24h_by_account: Vec<(String, Vec<u64>)> = v["hourly24h"]["byAccount"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, arr)| (k.clone(), parse_u64_vec(Some(arr))))
        .collect();
    hourly_24h_by_account.sort_by(|a, b| a.0.cmp(&b.0));

    let bp = &v["byPurpose"];
    let by_purpose = PurposeBreakdown {
        write: bp.get("write").and_then(Value::as_u64).unwrap_or(0),
        review: bp.get("review").and_then(Value::as_u64).unwrap_or(0),
        rework: bp.get("rework").and_then(Value::as_u64).unwrap_or(0),
        read_only: bp.get("readOnly").and_then(Value::as_u64).unwrap_or(0),
    };

    let by_model: Vec<ModelTokens> = v
        .get("byModel")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let model = single_line(m.get("model").and_then(Value::as_str).unwrap_or(""));
            if model.is_empty() {
                return None;
            }
            let tokens = m.get("tokens").and_then(Value::as_u64).unwrap_or(0);
            Some(ModelTokens { model, tokens })
        })
        .collect();

    let daily_7d: Vec<DailyTrendPoint> = v
        .get("daily7d")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|d| DailyTrendPoint {
            ymd: d
                .get("ymd")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            date: d
                .get("date")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            merged_count: d.get("mergedCount").and_then(Value::as_u64).unwrap_or(0),
            median_merge_secs: d.get("medianMergeSecs").and_then(Value::as_u64),
            avg_merge_secs: d.get("avgMergeSecs").and_then(Value::as_u64),
            total_chains: d.get("totalChains").and_then(Value::as_u64).unwrap_or(0),
            first_round_pass: d.get("firstRoundPass").and_then(Value::as_u64).unwrap_or(0),
            first_round_pass_pct: d.get("firstRoundPassPct").and_then(Value::as_u64),
        })
        .collect();

    let tb = &v["tokenBreakdown"];
    let token_input_7d = tb.get("input").and_then(Value::as_u64).unwrap_or(0);
    let token_output_7d = tb.get("output").and_then(Value::as_u64).unwrap_or(0);
    let token_cache_read_7d = tb.get("cacheRead").and_then(Value::as_u64).unwrap_or(0);

    AnalyticsSnapshot {
        hourly_24h_total,
        hourly_24h_by_account,
        by_purpose,
        by_model,
        daily_7d,
        token_input_7d,
        token_output_7d,
        token_cache_read_7d,
    }
}

pub fn parse_recent_deliveries_value(v: &Value) -> Vec<RecentDeliveryItem> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|d| {
            if !d.is_object() {
                return None;
            }
            let run = d.get("run").and_then(Value::as_str).unwrap_or("").to_string();
            if run.is_empty() {
                return None;
            }
            let name = single_line(d.get("name").and_then(Value::as_str).unwrap_or(&run));
            let repo = single_line(d.get("repo").and_then(Value::as_str).unwrap_or(""));
            let source_root = d
                .get("sourceRoot")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let applied_at = d
                .get("appliedAt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let applied_epoch = d
                .get("appliedEpoch")
                .and_then(Value::as_i64)
                .or_else(|| parse_iso_utc_epoch(&applied_at))
                .unwrap_or(0);
            let applied_head = single_line(d.get("appliedHead").and_then(Value::as_str).unwrap_or(""));
            let changes_files = d.get("changesFiles").and_then(Value::as_u64).unwrap_or(0) as usize;
            let added = d.get("added").and_then(Value::as_i64).unwrap_or(0);
            let deleted = d.get("deleted").and_then(Value::as_i64).unwrap_or(0);
            let elapsed_seconds = d.get("elapsedSeconds").and_then(Value::as_i64).unwrap_or(0);
            let rounds = d.get("rounds").and_then(Value::as_u64).unwrap_or(1) as usize;
            let conclusion = d
                .get("conclusion")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let work_id = opt_nonempty_str(d.get("workId"));
            let cleaned = d.get("cleaned").and_then(Value::as_bool).unwrap_or(false);
            Some(RecentDeliveryItem {
                run,
                name,
                repo,
                source_root,
                applied_at,
                applied_epoch,
                applied_head,
                changes_files,
                added,
                deleted,
                elapsed_seconds,
                rounds,
                conclusion,
                work_id,
                cleaned,
            })
        })
        .collect()
}

pub fn parse_pulse_value(v: &Value) -> Option<SnapshotPulse> {
    if !v.is_object() {
        return None;
    }
    Some(SnapshotPulse {
        owner_open: v.get("ownerOpen").and_then(Value::as_u64).unwrap_or(0) as usize,
        controller_pending: v
            .get("controllerPending")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
        stale_pending: v.get("stalePending").and_then(Value::as_u64).unwrap_or(0) as usize,
        running_runs: v.get("runningRuns").and_then(Value::as_u64).unwrap_or(0) as usize,
        queued_runs: v.get("queuedRuns").and_then(Value::as_u64).unwrap_or(0) as usize,
        work_active_pool: v
            .get("workActivePool")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
        work_deferred: v.get("workDeferred").and_then(Value::as_u64).unwrap_or(0) as usize,
        work_timed: v.get("workTimed").and_then(Value::as_u64).unwrap_or(0) as usize,
        merged_today: v.get("mergedToday").and_then(Value::as_u64).unwrap_or(0) as usize,
        pending_has_urgent: v
            .get("pendingHasUrgent")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        quota_alert: v.get("quotaAlert").and_then(Value::as_bool).unwrap_or(false),
        p0_alert: v.get("p0Alert").and_then(Value::as_bool).unwrap_or(false),
    })
}

pub fn parse_alerts_value(v: &Value) -> Vec<ActiveAlertItem> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|a| {
            if !a.is_object() {
                return None;
            }
            let level = match a.get("level").and_then(Value::as_str).unwrap_or("P1") {
                "P0" => AlertLevel::P0,
                _ => AlertLevel::P1,
            };
            let kind = single_line(a.get("kind").and_then(Value::as_str).unwrap_or(""));
            let label = single_line(a.get("label").and_then(Value::as_str).unwrap_or(""));
            let message = single_line(a.get("message").and_then(Value::as_str).unwrap_or(""));
            if message.is_empty() {
                return None;
            }
            let target_tab = a.get("targetTab").and_then(Value::as_u64).unwrap_or(1) as u8;
            let target_id = opt_nonempty_str(a.get("targetId"));
            Some(ActiveAlertItem {
                level,
                kind,
                label,
                message,
                target_tab,
                target_id,
            })
        })
        .collect()
}

pub fn parse_agy_accounts_value(v: &Value) -> AgyAccountsState {
    if !v.is_object() {
        return AgyAccountsState::default();
    }
    let root = v
        .get("root")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let active = v
        .get("active")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let live_email = v
        .get("liveEmail")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let live_drifted = v
        .get("liveDrifted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let running_agy: Vec<RunningAgyProc> = v
        .get("runningAgy")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let pid = p.get("pid").and_then(Value::as_u64)? as u32;
            let run = p
                .get("run")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let run_name = p
                .get("runName")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let role = p
                .get("role")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let round = p
                .get("round")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let account = p
                .get("account")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let cmdline = single_line(p.get("cmdline").and_then(Value::as_str).unwrap_or(""));
            Some(RunningAgyProc {
                pid,
                run,
                run_name,
                role,
                round,
                account,
                cmdline,
            })
        })
        .collect();
    let accounts: Vec<AgyAccountItem> = v
        .get("accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let id = a
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?
                .to_string();
            let files = string_list(a.get("files"));
            let file_count = a
                .get("fileCount")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(files.len());
            Some(AgyAccountItem {
                id,
                email: single_line(a.get("email").and_then(Value::as_str).unwrap_or("")),
                label: single_line(a.get("label").and_then(Value::as_str).unwrap_or("")),
                active: a.get("active").and_then(Value::as_bool).unwrap_or(false),
                added_at: a.get("addedAt").and_then(Value::as_str).unwrap_or("").to_string(),
                last_used_at: a.get("lastUsedAt").and_then(Value::as_str).unwrap_or("").to_string(),
                last_synced_at: a.get("lastSyncedAt").and_then(Value::as_str).unwrap_or("").to_string(),
                token_expires_at: a
                    .get("tokenExpiresAt")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                file_count,
                files,
                source: a.get("source").and_then(Value::as_str).unwrap_or("").to_string(),
                usage: parse_usage_stats(&a["usage"]),
                quota: parse_real_quota(&a["quota"]),
                quota_note: parse_quota_note(&a["quotaNote"]),
                quota_estimate: parse_quota_estimate(&a["quotaEstimate"]),
            })
        })
        .collect();
    let unassigned_usage = parse_usage_stats(&v["unassignedUsage"]);
    let quota_updated_at = v
        .get("quotaUpdatedAt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let quota_age_seconds = v.get("quotaAgeSeconds").and_then(Value::as_u64);
    let auto_switch = parse_agy_auto_switch(v.get("autoSwitch"));
    AgyAccountsState {
        root,
        active,
        live_email,
        live_drifted,
        running_agy,
        accounts,
        unassigned_usage,
        quota_updated_at,
        quota_age_seconds,
        auto_switch,
    }
}

fn parse_agy_auto_switch(val: Option<&Value>) -> AgyAutoSwitchInfo {
    let mut info = AgyAutoSwitchInfo::default();
    let Some(obj) = val.and_then(Value::as_object) else {
        return info;
    };
    if let Some(en) = obj.get("enabled").and_then(Value::as_bool) {
        info.enabled = en;
    } else if obj.get("mode").and_then(Value::as_str) == Some("auto") {
        info.enabled = true;
    }
    if let Some(v) = obj.get("fiveHourMinPct").and_then(Value::as_f64) {
        info.five_hour_min_pct = v.round().clamp(0.0, 100.0) as u64;
    }
    if let Some(v) = obj.get("weeklyMinPct").and_then(Value::as_f64) {
        info.weekly_min_pct = v.round().clamp(0.0, 100.0) as u64;
    }
    if let Some(v) = obj.get("refreshMinutes").and_then(Value::as_u64) {
        if v > 0 {
            info.refresh_minutes = v;
        }
    }
    if let Some(v) = obj.get("perRunReservePct").and_then(Value::as_f64) {
        info.per_run_reserve_pct = v.round().clamp(0.0, 100.0) as u64;
    }
    if let Some(pc) = obj.get("poolCurrent").and_then(Value::as_object) {
        let mut pairs: Vec<(String, String)> = pc
            .iter()
            .filter_map(|(k, v)| {
                let id = v.as_str()?.trim();
                (!id.is_empty()).then(|| (k.clone(), id.to_string()))
            })
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        info.pool_current = pairs;
    }
    if let Some(ps) = obj.get("poolSummary").and_then(Value::as_object) {
        let mut list: Vec<AgyPoolSummary> = Vec::new();
        for key in ["gemini", "claude_gpt"] {
            if let Some(item) = ps.get(key).and_then(Value::as_object) {
                let str_of = |k: &str| {
                    item.get(k)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string()
                };
                list.push(AgyPoolSummary {
                    pool: key.to_string(),
                    label: {
                        let l = str_of("label");
                        if l.is_empty() {
                            key.to_string()
                        } else {
                            l
                        }
                    },
                    current: str_of("current"),
                    selected: str_of("selected"),
                    tier: str_of("tier"),
                    tier_label: str_of("tierLabel"),
                    would_switch: item
                        .get("wouldSwitch")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    reason: single_line(&str_of("reason")),
                });
            }
        }
        info.pool_summary = list;
    }
    info
}

fn parse_line_count_pair(v: &Value) -> LineCountPair {
    let added = v.get("added").and_then(Value::as_u64).unwrap_or(0);
    let deleted = v.get("deleted").and_then(Value::as_u64).unwrap_or(0);
    let churn = v
        .get("churn")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| added.saturating_add(deleted));
    LineCountPair {
        added,
        deleted,
        churn,
    }
}

fn parse_line_stats_window(v: &Value) -> LineStatsWindow {
    let key = v
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let label = v
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let by_cat = v.get("byCategory");
    let cat_total = |name: &str| -> LineCountPair {
        by_cat
            .and_then(|c| c.get(name))
            .and_then(|c| c.get("total"))
            .map(parse_line_count_pair)
            .unwrap_or_default()
    };
    let code = cat_total("code");
    let test = cat_total("test");
    let docs = cat_total("docs");
    let total = v
        .get("total")
        .map(parse_line_count_pair)
        .unwrap_or_default();
    let delegated_share_pct = v
        .get("delegatedSharePct")
        .and_then(Value::as_f64)
        .filter(|x| x.is_finite() && *x >= 0.0)
        .map(|x| x.round() as u64);
    let lines_per_million_tokens = v
        .get("linesPerMillionTokens")
        .and_then(Value::as_f64)
        .filter(|x| x.is_finite() && *x >= 0.0)
        .map(|x| x.round() as u64);
    let tokens_total = v
        .get("tokens")
        .and_then(|t| t.get("total"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    LineStatsWindow {
        key,
        label,
        code,
        test,
        docs,
        total,
        delegated_share_pct,
        lines_per_million_tokens,
        tokens_total,
    }
}

pub fn parse_line_stats_json(raw: &str) -> Result<LineStatsSnapshot, String> {
    let v: Value = serde_json::from_str(raw.trim())
        .map_err(|e| format!("解析 delegate lines JSON 失败: {e}"))?;
    let windows = v
        .get("windows")
        .and_then(Value::as_array)
        .ok_or_else(|| "delegate lines JSON 缺少 windows 字段".to_string())?;
    let mut win_24h = LineStatsWindow {
        key: "24h".into(),
        label: "24 小时".into(),
        ..LineStatsWindow::default()
    };
    let mut win_7d = LineStatsWindow {
        key: "7d".into(),
        label: "7 天".into(),
        ..LineStatsWindow::default()
    };
    for (idx, w) in windows.iter().enumerate() {
        let parsed = parse_line_stats_window(w);
        if parsed.key == "24h" || (idx == 0 && parsed.key != "7d") {
            win_24h = parsed;
        } else if parsed.key == "7d" || idx == 1 {
            win_7d = parsed;
        }
    }
    let daily_7d = v
        .get("daily7d")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|d| {
                    let added = d.get("added").and_then(Value::as_u64).unwrap_or(0);
                    let deleted = d.get("deleted").and_then(Value::as_u64).unwrap_or(0);
                    let churn = d
                        .get("churn")
                        .and_then(Value::as_u64)
                        .unwrap_or_else(|| added.saturating_add(deleted));
                    LineStatsDailyPoint {
                        date: d
                            .get("date")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        added,
                        deleted,
                        churn,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let total_lines = v.get("totalLines").and_then(Value::as_object).map(|tl| {
        let code = tl.get("code").and_then(Value::as_u64).unwrap_or(0);
        let test = tl.get("test").and_then(Value::as_u64).unwrap_or(0);
        let docs = tl.get("docs").and_then(Value::as_u64).unwrap_or(0);
        let total = tl
            .get("total")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| code.saturating_add(test).saturating_add(docs));
        let by_repo = tl
            .get("byRepo")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        let repo = r.get("repo")?.as_str()?.to_string();
                        let path = r
                            .get("path")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let head = r
                            .get("head")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let rc = r.get("code").and_then(Value::as_u64).unwrap_or(0);
                        let rt = r.get("test").and_then(Value::as_u64).unwrap_or(0);
                        let rd = r.get("docs").and_then(Value::as_u64).unwrap_or(0);
                        let rtot = r
                            .get("total")
                            .and_then(Value::as_u64)
                            .unwrap_or_else(|| rc.saturating_add(rt).saturating_add(rd));
                        Some(RepoTotalLines {
                            repo,
                            path,
                            head,
                            counts: TotalLineCounts {
                                total: rtot,
                                code: rc,
                                test: rt,
                                docs: rd,
                            },
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let daily_totals = tl
            .get("dailyTotals")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|d| {
                        let date = d.get("date")?.as_str()?.to_string();
                        let dt = d.get("total").and_then(Value::as_u64)?;
                        Some(DailyTotalLinesPoint { date, total: dt })
                    })
                    .collect()
            })
            .unwrap_or_default();
        TotalLinesSnapshot {
            counts: TotalLineCounts {
                total,
                code,
                test,
                docs,
            },
            by_repo,
            daily_totals,
        }
    });
    let errors = v
        .get("errors")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let repo = e.get("repo")?.as_str()?.to_string();
                    let err = e.get("error")?.as_str()?.to_string();
                    Some((repo, err))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(LineStatsSnapshot {
        win_24h,
        win_7d,
        daily_7d,
        total_lines,
        errors,
    })
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn parse_snapshot_json_hydrates_repos_events_and_lane() {
        let raw = r#"{
            "schemaVersion": 1,
            "seq": 42,
            "repos": [
                {
                    "path": "/tmp/demo-repo",
                    "runsDir": "/tmp/demo-repo/.local/run/delegate",
                    "runs": [
                        {
                            "run": "20261123-120000-demo",
                            "name": "demo",
                            "state": "delivered",
                            "agent": "pi",
                            "mode": "write",
                            "elapsedSeconds": 12
                        }
                    ],
                    "cleanedNotes": []
                }
            ],
            "events": [
                {
                    "seq": 42,
                    "ts": "2026-11-23T12:00:00Z",
                    "run": "20261123-120000-demo",
                    "repo": "/tmp/demo-repo",
                    "event": "delivered",
                    "detail": {"name": "demo"}
                }
            ],
            "lane": {
                "maxHeavy": 2,
                "heavyJobs": 4,
                "active": [],
                "queued": []
            },
            "dagQueue": [],
            "merges": [],
            "candidates": []
        }"#;
        let snap = parse_snapshot_json(raw, None, None).expect("snapshot parses");
        assert_eq!(snap.repos.len(), 1);
        assert_eq!(snap.repos[0].runs.len(), 1);
        assert_eq!(snap.repos[0].runs[0].run, "20261123-120000-demo");
        assert_eq!(snap.repos[0].runs[0].state, "delivered");
        assert_eq!(snap.events.len(), 1);
        assert_eq!(snap.events[0].seq, 42);
        assert_eq!(snap.lane.max_heavy, Some(2));
    }

    #[test]
    fn shared_pending_classification_fixture_matches_tui_groups() {
        let fixture_raw = include_str!("../../../tests/fixtures/pending_classification.json");
        let doc: Value = serde_json::from_str(fixture_raw).expect("valid shared fixture json");
        let expected_count = doc["expectedPendingCount"].as_u64().unwrap() as usize;
        let mut expected_pending: Vec<String> = doc["expectedPendingRuns"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        expected_pending.sort();
        let mut expected_collected: Vec<String> = doc["expectedCollectedRuns"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        expected_collected.sort();

        let mut jsonl = String::new();
        for item in doc["runs"].as_array().unwrap() {
            jsonl.push_str(&item.to_string());
            jsonl.push('\n');
        }
        let repo = parse_status_jsonl_at(
            Path::new("/tmp/fixture-repo"),
            "fixture-repo",
            &jsonl,
            Some(1_795_430_000),
        );

        let mut actual_pending: Vec<String> = repo
            .runs
            .iter()
            .filter(|r| r.group == GroupKind::Pending)
            .map(|r| r.run.clone())
            .collect();
        actual_pending.sort();
        let mut actual_collected: Vec<String> = repo
            .runs
            .iter()
            .filter(|r| r.group == GroupKind::Collected)
            .map(|r| r.run.clone())
            .collect();
        actual_collected.sort();

        assert_eq!(repo.count_group(GroupKind::Pending), expected_count);
        assert_eq!(actual_pending, expected_pending);
        assert_eq!(actual_collected, expected_collected);
        assert_eq!(GroupKind::Collected.label(), "已收取");
    }

    #[test]
    fn parse_switch_agy_account_output_reports_running_processes_notice() {
        let with_running = r#"{"ok":true,"active":"beta","email":"beta@example.test","runningAgyCount":2,"runningAgy":[{"pid":101,"run":"r1","cmdline":"agy"},{"pid":102,"run":"r2","cmdline":"agy"}]}"#;
        assert_eq!(
            parse_switch_agy_account_output("beta", with_running, "", true, Some(0)).unwrap(),
            "已切到 beta；在跑的 2 个运行用原账号跑完"
        );

        let idle = r#"{"ok":true,"active":"beta","email":"beta@example.test","runningAgyCount":0,"runningAgy":[]}"#;
        assert_eq!(
            parse_switch_agy_account_output("beta", idle, "", true, Some(0)).unwrap(),
            "已切到 beta (beta@example.test)"
        );

        let err_json = r#"{"ok":false,"error":"untracked_live_credentials","message":"live account is untracked"}"#;
        assert_eq!(
            parse_switch_agy_account_output("beta", err_json, "", false, Some(1)).unwrap_err(),
            "live account is untracked"
        );
    }

    #[test]
    fn parse_snapshot_json_hydrates_disk_health_and_overview_line() {
        let raw = r#"{
            "schemaVersion": 1,
            "seq": 1,
            "repos": [],
            "events": [],
            "lane": {"maxHeavy": 2, "heavyJobs": 4, "active": [], "queued": []},
            "dagQueue": [],
            "merges": [],
            "candidates": [],
            "diskHealth": {
                "configured": true,
                "level": "high",
                "primary": {
                    "root": "/path/to/primary",
                    "totalBytes": 322122547200,
                    "usedBytes": 279172874240,
                    "availBytes": 42949672960,
                    "usedPct": 86.7,
                    "level": "high"
                },
                "overflow": {
                    "root": "/path/to/overflow",
                    "totalBytes": 1099511627776,
                    "usedBytes": 439804651110,
                    "availBytes": 659706976666,
                    "usedPct": 40.0,
                    "level": "ok"
                },
                "totalSlots": 4,
                "overflowSlots": ["repo-3", "repo-4"],
                "overflowSlotCount": 2,
                "staleRuns": [
                    {
                        "run": "20261125-010000-old",
                        "name": "old-task",
                        "slot": "repo-1",
                        "idleHours": 27,
                        "bytes": 12884901888
                    }
                ]
            }
        }"#;
        let snap = parse_snapshot_json(raw, None, None).expect("snapshot parses");
        let dh = snap.disk_health.as_ref().expect("diskHealth parsed");
        assert_eq!(dh.level, "high");
        assert_eq!(dh.overflow_slots.len(), 2);
        assert_eq!(dh.stale_runs.len(), 1);
        assert_eq!(dh.stale_runs[0].name, "old-task");
        let (line, _) = crate::ui_overview::format_overview_disk_line(dh).expect("overview disk line");
        assert!(line.contains("主盘 87%"), "line={line}");
        assert!(line.contains("溢出盘 40%"), "line={line}");
        assert!(line.contains("溢出槽 2"), "line={line}");
        assert!(line.contains("old-task"), "line={line}");
        let alerts = snap.effective_alerts();
        assert!(alerts.iter().any(|a| a.kind == "disk_high"));
        assert!(alerts.iter().any(|a| a.kind == "disk_stale_slot"));
    }

    #[test]
    fn replied_question_with_result_read_is_collected_not_pending() {
        let raw = r#"{
            "schemaVersion": 1,
            "seq": 1,
            "repos": [{
                "path": "/tmp/demo",
                "runs": [
                    {
                        "run": "20261127-050000-scout-replied",
                        "name": "scout-replied",
                        "state": "answered",
                        "mode": "read-only",
                        "question": true,
                        "resultReadAt": "2026-11-27T05:00:00Z",
                        "repliedAt": "2026-11-27T05:00:01Z"
                    },
                    {
                        "run": "20261127-050001-scout-unreplied",
                        "name": "scout-unreplied",
                        "state": "answered",
                        "mode": "read-only",
                        "question": true,
                        "resultReadAt": "2026-11-27T05:00:00Z"
                    }
                ]
            }],
            "events": [],
            "lane": {"maxHeavy": 2, "heavyJobs": 4, "active": [], "queued": []},
            "dagQueue": [],
            "merges": [],
            "candidates": []
        }"#;
        let snap = parse_snapshot_json(raw, None, None).expect("snapshot parses");
        let runs = &snap.repos[0].runs;
        let replied = runs
            .iter()
            .find(|r| r.run == "20261127-050000-scout-replied")
            .expect("replied scout run");
        assert_eq!(replied.group, GroupKind::Collected);
        assert!(!replied.question);

        let unreplied = runs
            .iter()
            .find(|r| r.run == "20261127-050001-scout-unreplied")
            .expect("unreplied scout run");
        assert_eq!(unreplied.group, GroupKind::Pending);
        assert!(unreplied.question);
    }
}
