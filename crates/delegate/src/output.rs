use crate::changes;
use crate::common::*;
use crate::runs;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

static JSON: AtomicBool = AtomicBool::new(false);
pub fn enable_json() {
    JSON.store(true, Ordering::Relaxed);
}
pub fn json_enabled() -> bool {
    JSON.load(Ordering::Relaxed)
}
fn glob_match(pattern: &[u8], path: &[u8]) -> bool {
    if pattern.is_empty() { return path.is_empty(); }
    if pattern.starts_with(b"**/") {
        return glob_match(&pattern[3..], path) || path.iter().enumerate()
            .filter(|(_, byte)| **byte == b'/')
            .any(|(i, _)| glob_match(&pattern[3..], &path[i + 1..]));
    }
    if pattern[0] == b'*' {
        return glob_match(&pattern[1..], path) || (!path.is_empty() && path[0] != b'/' && glob_match(pattern, &path[1..]));
    }
    if pattern[0] == b'?' {
        return !path.is_empty() && path[0] != b'/' && glob_match(&pattern[1..], &path[1..]);
    }
    !path.is_empty() && pattern[0] == path[0] && glob_match(&pattern[1..], &path[1..])
}
pub fn blind_files(files: &Value, patterns: &Value) -> Vec<String> {
    let rules = strings(patterns);
    files.as_array().into_iter().flatten().filter_map(Value::as_str)
        .filter(|path| rules.iter().any(|rule| glob_match(rule.as_bytes(), path.as_bytes())))
        .map(str::to_string).collect()
}
pub fn blind_note(files: &Value) -> String {
    let Some(items) = files.as_array() else { return String::new() };
    if items.is_empty() { return String::new(); }
    let shown = items.iter().take(5).filter_map(Value::as_str).collect::<Vec<_>>().join("、");
    format!("验收未覆盖：{shown}{}", if items.len() > 5 { format!(" 等另 {} 个", items.len()-5) } else { String::new() })
}

pub fn worktree_disk() {
    let threshold = setting("WORKTREE_WARN_GIB", "20")
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .unwrap_or(20.0);
    if threshold == 0.0 {
        return;
    }
    let threshold_bytes = (threshold * 1024.0_f64.powi(3)) as u64;
    let cwd = std::env::current_dir().unwrap_or_default();
    let repo = crate::launch::repo_key(&cwd);
    let all = runs::all_runs();
    let active_paths: std::collections::BTreeSet<_> = all
        .iter()
        .filter(|run| runs::active(&runs::state(run)))
        .map(|run| s(&json(run.join("meta.json"))["worktree"], "path").to_string())
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut items = Vec::new();
    for run in all.iter().rev() {
        let meta = json(run.join("meta.json"));
        let path = s(&meta["worktree"], "path");
        if s(&meta, "mode") != "write"
            || runs::active(&runs::state(run))
            || crate::launch::repo_key_for_meta(&meta) != repo
            || path.is_empty()
            || !Path::new(path).is_dir()
            || active_paths.contains(path)
            || !seen.insert(path.to_string())
        {
            continue;
        }
        let Some(bytes) = meta["worktreeBytes"].as_u64() else {
            continue;
        };
        items.push(json!({"run":meta["run"], "name":meta["name"], "bytes":bytes}));
    }
    let total = items
        .iter()
        .map(|item| item["bytes"].as_u64().unwrap_or(0))
        .fold(0u64, u64::saturating_add);
    if total < threshold_bytes || items.is_empty() {
        return;
    }
    items.sort_by_key(|item| std::cmp::Reverse(item["bytes"].as_u64().unwrap_or(0)));
    items.truncate(3);
    let next = "已确认合入后：delegate clean <name> --force";
    if json_enabled() {
        println!(
            "{}",
            json!({"worktreeDisk":{"totalBytes":total,"thresholdBytes":threshold_bytes,"largest":items,"next":next}})
        );
    } else {
        let largest = items
            .iter()
            .map(|item| {
                format!(
                    "{} {:.1} GiB",
                    s(item, "name").replace(['\n', '\r'], " "),
                    item["bytes"].as_u64().unwrap_or(0) as f64 / 1024.0_f64.powi(3)
                )
            })
            .collect::<Vec<_>>()
            .join("、");
        println!(
            "提示：已结束未清理 worktree 共 {:.1} GiB；最大：{largest}；{next}",
            total as f64 / 1024.0_f64.powi(3)
        );
    }
}

pub fn source_build_disk() {
    let threshold = setting("SOURCE_BUILD_WARN_GIB", "60")
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .unwrap_or(60.0);
    if threshold == 0.0 {
        return;
    }
    let threshold_bytes = (threshold * 1024.0_f64.powi(3)) as u64;
    let cwd = std::env::current_dir().unwrap_or_default();
    let repo = crate::launch::repo_key(&cwd);
    let latest = runs::all_runs()
        .iter()
        .map(|run| json(run.join("meta.json")))
        .filter(|meta| {
            s(meta, "mode") == "write"
                && crate::launch::repo_key_for_meta(meta) == repo
                && meta["sourceBuildBytes"].as_u64().is_some()
                && meta["sourceBuildPaths"].as_array().is_some_and(|paths| {
                    paths.iter().all(|item| {
                        item["path"].as_str().is_some_and(|path| !path.is_empty())
                            && item["bytes"].as_u64().is_some()
                    })
                })
                && meta["sourceBuildMeasuredAt"]
                    .as_str()
                    .is_some_and(|time| !time.is_empty())
        })
        .filter_map(|meta| {
            let time = s(&meta, "sourceBuildMeasuredNs").parse::<u128>().ok()?;
            Some((time, meta))
        })
        .max_by_key(|(time, _)| *time);
    let Some((_, meta)) = latest else {
        return;
    };
    let total = meta["sourceBuildBytes"].as_u64().unwrap_or(0);
    if total == 0 || total < threshold_bytes {
        return;
    }
    let next = "先量再删：见 references/cleanup.md 的“源仓库的构建缓存”";
    if json_enabled() {
        println!(
            "{}",
            json!({"sourceBuildDisk":{
                "totalBytes":total,"thresholdBytes":threshold_bytes,
                "paths":meta["sourceBuildPaths"],"measuredAt":meta["sourceBuildMeasuredAt"],"next":next
            }})
        );
    } else {
        let paths = meta["sourceBuildPaths"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| {
                format!(
                    "{} {:.1} GiB",
                    s(item, "path").replace(['\n', '\r'], " "),
                    item["bytes"].as_u64().unwrap_or(0) as f64 / 1024.0_f64.powi(3)
                )
            })
            .collect::<Vec<_>>()
            .join("、");
        println!(
            "提示：源仓库构建目录共 {:.1} GiB；各路径：{paths}；{next}",
            total as f64 / 1024.0_f64.powi(3)
        );
    }
}

pub fn human(run: &Path, status: &Value) -> String {
    let meta = json(run.join("meta.json"));
    let state = s(status, "state");
    let label = match state {
        "running" => "运行中",
        "waiting" => "等待中",
        "starting" => "启动中",
        "delivered" => "已交付",
        "answered" => "已答复",
        "rejected" => "未通过",
        "timeout" => "超时",
        "stopped" => "已停止",
        "skipped" => "已跳过",
        "malformed" => "答复畸形",
        "failed" => "失败",
        "crashed" => "进程异常",
        "killed" => "已终止",
        _ => state,
    };
    let seconds = n(status, "elapsedSeconds");
    let elapsed = if seconds < 60 {
        format!("{seconds} 秒")
    } else {
        format!("{} 分钟", seconds / 60)
    };
    let mut parts = vec![
        s(status, "name").replace(['\n', '\r'], " "),
        format!("{label} {elapsed}"),
        format!("{}/{}", s(status, "agent"), s(status, "tier")),
    ];
    let mut totals = status["changes"].clone();
    let mut paths = status["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    // A worktree is still being checked out until the agent starts; counting it then shows mass deletions.
    if runs::active(state) && !meta["base"].is_null() && run.join("agent.pid").exists() {
        let top = Path::new(s(&meta, "top"));
        if let Some(after) = changes::snapshot(top, run, &changes::managed_paths(&meta)) {
            if let Ok(diff) =
                changes::tree_changes(top, &meta["base"], &after, meta["worktree"].is_object())
            {
                paths = diff.iter().map(|c| s(c, "path").to_string()).collect();
                totals = json!({"files":diff.len(),"added":diff.iter().map(|c|n(c,"added")).sum::<i64>(),"deleted":diff.iter().map(|c|n(c,"deleted")).sum::<i64>()});
            }
        }
    }
    let mut dirs = BTreeMap::<String, usize>::new();
    for path in &paths {
        let relative = repository_relative(path, s(&meta, "top"));
        let relative = Path::new(&relative);
        let parent = relative.parent().unwrap_or(Path::new("."));
        let dir = parent
            .components()
            .take(2)
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        *dirs
            .entry(if dir.is_empty() { ".".into() } else { dir })
            .or_default() += 1;
    }
    let mut dirs = dirs.into_iter().collect::<Vec<_>>();
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let dirs = dirs
        .iter()
        .take(3)
        .map(|(dir, count)| format!("{dir} {count}"))
        .collect::<Vec<_>>()
        .join(", ");
    let count = if totals.is_object() {
        n(&totals, "files") as usize
    } else {
        paths.len()
    };
    if count > 0 {
        parts.push(format!(
            "改 {count} 个文件{}{}",
            if totals.is_object() {
                format!(" +{}/-{}", n(&totals, "added"), n(&totals, "deleted"))
            } else {
                String::new()
            },
            if dirs.is_empty() {
                String::new()
            } else {
                format!("（{dirs}）")
            }
        ));
    }
    if s(status, "warning").contains("could not snapshot") {
        parts.push("改动未知".into());
    }
    if n(&status["pendingChanges"], "files") > count as i64 {
        parts.push(format!(
            "累计待合入 {} 个文件 +{}/-{}",
            n(&status["pendingChanges"], "files"),
            n(&status["pendingChanges"], "added"),
            n(&status["pendingChanges"], "deleted")
        ));
    }
    if state == "running" {
        if let Some(command) = runs::events(run)
            .iter()
            .rev()
            .find(|e| !s(e, "cmd").is_empty())
        {
            let mut cmd = s(command, "cmd").replace(['\n', '\r'], " ");
            for root in [
                s(&meta, "top"),
                s(&meta, "workdir"),
                s(&meta["worktree"], "source"),
            ] {
                if !root.is_empty() {
                    cmd = cmd.replace(&format!("{root}/"), "").replace(root, ".");
                }
            }
            parts.push(format!("最近：{}", clip(&cmd, 97)));
        }
    }
    if status["accept"].is_object() {
        parts.push(
            if b(&status["accept"], "ok") {
                "验收通过"
            } else {
                "验收失败"
            }
            .into(),
        );
        // Excluded roots are managed links and unfetched reference submodules by
        // design; `--json` keeps them.
    } else if state == "answered" {
        parts.push("未设验收".into());
    }
    if status["evidence"].is_object() {
        parts.push(format!(
            "证据 {}",
            if b(&status["evidence"], "timedOut") {
                "超时"
            } else if status["evidence"]["exit"].as_i64() == Some(0) {
                "通过"
            } else {
                "失败"
            }
        ));
    }
    if b(status, "applied") {
        parts.push(
            if s(status, "appliedBy") == "detected" {
                "已合入（主干已含改动）"
            } else {
                "已合入"
            }
            .into(),
        );
    }
    if !status["protectViolation"].is_null() {
        parts.push(
            if s(status, "error").contains("generated output") {
                "生成物被手改"
            } else {
                "触及保护路径"
            }
            .into(),
        );
    }
    if !status["readOnlyViolation"].is_null() || !status["workspaceChanged"].is_null() {
        parts.push("只读工作区有改动".into());
    }
    if n(status, "denied") > 0 {
        parts.push(format!("拦下 {} 次全量检查", n(status, "denied")));
    }
    let occupied = crate::resources::occupied_for_run(s(status, "run"));
    if !occupied.is_empty() { parts.push(format!("占用 {}", occupied.join("、"))); }
    let blind = blind_note(&status["acceptBlind"]);
    if !blind.is_empty() { parts.push(blind); }
    let id = s(status, "run");
    let next = if runs::active(state) {
        format!("等待交付：delegate wait {id}")
    } else if b(status, "applied") && s(status, "appliedBy") == "detected" {
        format!("清理：delegate clean {id}")
    } else if !s(status, "next").is_empty() {
        if state == "timeout" && s(status, "mode") == "write" {
            format!("改动保留，续做不计返工次数：{}", s(status, "next"))
        } else if matches!(state, "delivered" | "answered") {
            if meta["worktree"].is_object() {
                if status["sourceDrift"]["overlap"]
                    .as_array()
                    .is_some_and(|paths| !paths.is_empty())
                {
                    format!("源改动重叠，审查后合入或同步返工：delegate diff {id} --total，再 delegate apply {id} 或 delegate reply {id} --sync '<修正>'")
                } else {
                    format!("审查后合入：delegate diff {id} --total，再 delegate apply {id}")
                }
            } else {
                format!("审查改动：delegate diff {id}")
            }
        } else {
            format!("查看结论后接手或返工：delegate status {id} --json")
        }
    } else {
        String::new()
    };
    if !next.is_empty() {
        parts.push(format!("下一步：{next}"));
    }
    parts.join(" · ")
}
