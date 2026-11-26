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
    if pattern.is_empty() {
        return path.is_empty();
    }
    if pattern.starts_with(b"**/") {
        return glob_match(&pattern[3..], path)
            || path
                .iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'/')
                .any(|(i, _)| glob_match(&pattern[3..], &path[i + 1..]));
    }
    if pattern[0] == b'*' {
        return glob_match(&pattern[1..], path)
            || (!path.is_empty() && path[0] != b'/' && glob_match(pattern, &path[1..]));
    }
    if pattern[0] == b'?' {
        return !path.is_empty() && path[0] != b'/' && glob_match(&pattern[1..], &path[1..]);
    }
    !path.is_empty() && pattern[0] == path[0] && glob_match(&pattern[1..], &path[1..])
}
pub fn blind_files(files: &Value, patterns: &Value) -> Vec<String> {
    let rules = strings(patterns);
    files
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|path| {
            rules
                .iter()
                .any(|rule| glob_match(rule.as_bytes(), path.as_bytes()))
        })
        .map(str::to_string)
        .collect()
}
pub fn blind_note(files: &Value) -> String {
    let Some(items) = files.as_array() else {
        return String::new();
    };
    if items.is_empty() {
        return String::new();
    }
    let shown = items
        .iter()
        .take(5)
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("、");
    format!(
        "验收未覆盖：{shown}{}",
        if items.len() > 5 {
            format!(" 等另 {} 个", items.len() - 5)
        } else {
            String::new()
        }
    )
}

/// Combined single-pass `worktree_disk` + `source_build_disk` warning emitter for `status`/`wait`.
/// Reads each run's `meta.json` at most once (instead of 4x across two `runs::all_runs()` calls).
pub fn disk_warnings() {
    let wt_threshold = setting("WORKTREE_WARN_GIB", "20")
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .unwrap_or(20.0);
    let sb_threshold = setting("SOURCE_BUILD_WARN_GIB", "60")
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .unwrap_or(60.0);
    if wt_threshold == 0.0 && sb_threshold == 0.0 {
        return;
    }

    let mut roots = vec![runs_root()];
    if let Some(old) = legacy_runs_root() {
        roots.push(old);
    }
    let mut entries: Vec<(u64, std::path::PathBuf, Value)> = Vec::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(root) else {
            continue;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            let mp = p.join("meta.json");
            if !mp.is_file() {
                continue;
            }
            let meta = json(&mp);
            let ns = meta["startedNs"].as_u64().unwrap_or(0);
            entries.push((ns, p, meta));
        }
    }
    if entries.is_empty() {
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let repo = crate::launch::repo_key(&cwd);

    if wt_threshold > 0.0 {
        entries.sort_by_key(|(ns, _, _)| *ns);
        let threshold_bytes = (wt_threshold * 1024.0_f64.powi(3)) as u64;
        let mut active_paths = std::collections::BTreeSet::new();
        for (_, run, meta) in &entries {
            if !run.join("exit_code").is_file() && runs::active(&runs::state(run)) {
                active_paths.insert(s(&meta["worktree"], "path").to_string());
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut items = Vec::new();
        for (_, run, meta) in entries.iter().rev() {
            let Some(bytes) = meta["worktreeBytes"].as_u64() else {
                continue;
            };
            let path = s(&meta["worktree"], "path");
            if s(meta, "mode") != "write"
                || path.is_empty()
                || (!run.join("exit_code").is_file() && runs::active(&runs::state(run)))
                || crate::launch::repo_key_for_meta(meta) != repo
                || !Path::new(path).is_dir()
                || active_paths.contains(path)
                || !seen.insert(path.to_string())
            {
                continue;
            }
            items.push(json!({"run":meta["run"], "name":meta["name"], "bytes":bytes}));
        }
        let total = items
            .iter()
            .map(|item| item["bytes"].as_u64().unwrap_or(0))
            .fold(0u64, u64::saturating_add);
        if total >= threshold_bytes && !items.is_empty() {
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
    }

    if sb_threshold > 0.0 {
        let threshold_bytes = (sb_threshold * 1024.0_f64.powi(3)) as u64;
        let latest = entries
            .iter()
            .map(|(_, _, meta)| meta)
            .filter(|meta| {
                s(meta, "mode") == "write"
                    && meta["sourceBuildBytes"].as_u64().is_some()
                    && crate::launch::repo_key_for_meta(meta) == repo
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
                let time = s(meta, "sourceBuildMeasuredNs").parse::<u128>().ok()?;
                Some((time, meta))
            })
            .max_by_key(|(time, _)| *time);
        if let Some((_, meta)) = latest {
            let total = meta["sourceBuildBytes"].as_u64().unwrap_or(0);
            if total > 0 && total >= threshold_bytes {
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
        }
    }
}

/// A delivery that failed (rework limit, launch error) stays in the mailbox with its error;
/// say so instead of looking like it is still waiting.
fn pending_label(pending: &Value) -> Option<String> {
    if !pending.is_object() {
        return None;
    }
    let error = s(pending, "error");
    Some(if error.is_empty() {
        "待续答 / pending reply".into()
    } else {
        format!("续答未发出 / pending reply failed: {}（stop 清除后重发）", clip(error, 160))
    })
}

pub fn human(run: &Path, status: &Value) -> String {
    human_line(run, status, false)
}

pub fn human_line(run: &Path, status: &Value, collapse_chain: bool) -> String {
    human_line_with_repo(run, status, collapse_chain, None)
}

fn format_short_duration_zh(seconds: i64) -> String {
    let s = seconds.max(0);
    if s < 60 {
        format!("{s} 秒")
    } else if s < 3600 {
        format!("{} 分钟", s / 60)
    } else {
        format!("{} 小时 {} 分", s / 3600, (s % 3600) / 60)
    }
}

fn remote_exec_inline(status: &Value, state: &str) -> Option<String> {
    let re = status.get("remoteExec").filter(|v| v.is_object())?;
    let host = s(re, "host");
    if host.is_empty() {
        return None;
    }
    let active = b(re, "active");
    let running = b(re, "running");
    let cmd = s(re, "cmd");
    if active {
        let short_cmd = if cmd.is_empty() {
            String::new()
        } else {
            format!(" {}", clip(&cmd.replace(['\n', '\r'], " "), 40))
        };
        let dur = re["elapsedSeconds"]
            .as_i64()
            .map(|sec| format!(" {}", format_short_duration_zh(sec)))
            .unwrap_or_default();
        if running {
            Some(format!("远端:@{host}{short_cmd}{dur}"))
        } else {
            Some(format!("远端:@{host} 排队{short_cmd}"))
        }
    } else if runs::active(state) {
        Some(format!("远端:@{host}"))
    } else {
        None
    }
}

pub fn human_line_with_repo(
    run: &Path,
    status: &Value,
    collapse_chain: bool,
    repo_tag: Option<&str>,
) -> String {
    let state = s(status, "state");
    let label = match state {
        "running" => "运行中",
        "waiting" => "等待中",
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
    };
    let label = if b(status, "question") {
        "提问"
    } else if b(status, "policyRefused") {
        "结论被模型服务拒绝"
    } else {
        label
    };
    let seconds = n(status, "elapsedSeconds");
    let elapsed = if seconds < 60 {
        format!("{seconds} 秒")
    } else {
        format!("{} 分钟", seconds / 60)
    };
    let base_name = if collapse_chain
        && n(status, "chainRound") > 1
        && !s(status, "chainName").is_empty()
    {
        format!(
            "{}（第 {} 轮）",
            s(status, "chainName").replace(['\n', '\r'], " "),
            n(status, "chainRound")
        )
    } else {
        s(status, "name").replace(['\n', '\r'], " ")
    };
    let display_name = match repo_tag.filter(|t| !t.is_empty()) {
        Some(tag) => format!("[{tag}] {base_name}"),
        None => base_name,
    };
    let mut parts = vec![
        display_name,
        format!("{label} {elapsed}"),
        agent_label(status),
    ];
    if let Some(remote_part) = remote_exec_inline(status, state) {
        parts.push(remote_part);
    }
    if status["appends"].is_object() && n(&status["appends"], "total") > 0 {
        parts.push(format!(
            "已追加 {} 条，待投递 {} 条",
            n(&status["appends"], "total"),
            n(&status["appends"], "pending")
        ));
        let err = {
            let e = s(&status["appends"], "error");
            if !e.is_empty() {
                e
            } else {
                s(&status["pendingReply"], "error")
            }
        };
        if !err.is_empty() {
            parts.push(format!(
                "续答未发出 / pending reply failed: {}（stop 清除后重发）",
                clip(err, 160)
            ));
        }
    } else if let Some(label) = pending_label(&status["pendingReply"]) {
        parts.push(label);
    }
    if !s(status, "busy").is_empty() { parts.push(format!("busy: {}", s(status, "busy"))); }
    if status["queue"]["busy"] == json!(true) { parts.push("busy: capacity; showing cached queue".into()); }
    if status["queue"].is_object() {
        let queue = &status["queue"];
        let minutes =
            ((epoch() as i64 - iso_epoch(s(queue, "since")).unwrap_or(epoch() as i64)) / 60).max(0);
        parts[1] = format!("排队中 {minutes} 分钟");
        parts.push(s(queue, "reason").into());
        parts.push(format!("前面还有 {} 个", n(queue, "ahead")));
    }
    let totals = status["changes"].clone();
    let paths = status["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut dirs = BTreeMap::<String, usize>::new();
    let status_top = s(status, "top");
    for path in &paths {
        let relative = repository_relative(path, status_top);
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
    let has_worktree = !s(status, "worktree").is_empty();
    let use_pending = has_worktree
        && s(status, "mode") == "write"
        && status["pendingChanges"].is_object();
    let display_totals = if use_pending {
        status["pendingChanges"].clone()
    } else {
        totals.clone()
    };
    let count = if display_totals.is_object() {
        n(&display_totals, "files") as usize
    } else {
        paths.len()
    };
    let show_dirs = !dirs.is_empty()
        && (!use_pending
            || !totals.is_object()
            || n(&totals, "files") == n(&display_totals, "files"));
    if count > 0 {
        parts.push(format!(
            "改 {count} 个文件{}{}",
            if display_totals.is_object() {
                format!(
                    " +{}/-{}",
                    n(&display_totals, "added"),
                    n(&display_totals, "deleted")
                )
            } else {
                String::new()
            },
            if show_dirs {
                format!("（{dirs}）")
            } else {
                String::new()
            }
        ));
    }
    let uncommitted = strings(&status["commitsDiffer"]);
    if !uncommitted.is_empty() {
        parts.push(format!("commits differ {} 个文件", uncommitted.len()));
    }
    if !s(status, "archive").is_empty() {
        parts.push(format!("archive: {}", s(status, "archive")));
    }
    if s(status, "warning").contains("could not snapshot") {
        parts.push("改动未知".into());
    }
    if !use_pending && n(&status["pendingChanges"], "files") > count as i64 {
        parts.push(format!(
            "累计待合入 {} 个文件 +{}/-{}",
            n(&status["pendingChanges"], "files"),
            n(&status["pendingChanges"], "added"),
            n(&status["pendingChanges"], "deleted")
        ));
    }
    if runs::active(state) {
        let act_label = s(&status["activity"], "label");
        if !act_label.is_empty() {
            parts.push(act_label.to_string());
        } else if state == "running" {
            // Waiting out an upstream outage is what the run is doing now; its last command is old news.
            match status["last"]
                .as_str()
                .and_then(|last| last.strip_prefix("resume: "))
            {
                Some(waiting) => parts.push(waiting.to_string()),
                None => {
                    if let Some(command) = runs::events(run)
                        .iter()
                        .rev()
                        .find(|e| !s(e, "cmd").is_empty())
                    {
                        let mut cmd = s(command, "cmd").replace(['\n', '\r'], " ");
                        for root in [status_top, s(status, "worktree")] {
                            if !root.is_empty() {
                                cmd = cmd.replace(&format!("{root}/"), "").replace(root, ".");
                            }
                        }
                        parts.push(format!("最近：{}", clip(&cmd, 97)));
                    }
                }
            }
        }
    }
    if status["stallWarning"].is_object() {
        let sw = &status["stallWarning"];
        let det = s(sw, "detail");
        if det.is_empty() {
            parts.push(format!("⚠ 卡住提醒（{}）", s(sw, "kind")));
        } else if !s(&status["activity"], "label").contains("卡住提醒") {
            parts.push(format!("⚠ 卡住提醒：{}", clip(det, 60)));
        }
    }
    if let Some(ahead) = status["measureWaitingAhead"].as_u64() {
        parts.push(format!("等待测量预约，前面 {ahead} 个"));
    }
    if b(status, "remoteOff") {
        parts.push("远程：关".into());
    }
    if status["lane"].is_object() {
        let queue_zh = match s(&status["lane"], "queue") {
            "remote-heavy" => "远端重队列",
            "remote-light" => "远端轻队列",
            "local" => "本机队列",
            other => other,
        };
        if !queue_zh.is_empty() {
            let state_zh = if b(&status["lane"], "running") {
                "运行中"
            } else {
                "排队中"
            };
            parts.push(format!("{queue_zh}（{state_zh}）"));
        }
    }
    if status["accept"].is_object() {
        parts.push(crate::accept::human_accept_label(
            &status["accept"],
            b(status, "infrastructureFailure"),
        ));
        // Excluded roots are managed links and unfetched reference submodules by
        // design; `--json` keeps them.
    } else if state == "answered" {
        parts.push("未设验收".into());
    }
    if status["review"].is_object() {
        let rev = &status["review"];
        let rstatus = s(rev, "status");
        let rverdict = s(rev, "verdict");
        let round = n(rev, "round").max(1);
        let rework_rounds = n(rev, "reworkRounds");
        let review_label = if rstatus == "reviewing" {
            if round > 1 {
                format!("审查中（第 {round} 轮）")
            } else {
                "审查中".to_string()
            }
        } else if rstatus == "reworking" {
            let r = rework_rounds.max(round);
            format!("REWORK 第 {r} 轮（返工中）")
        } else if rstatus == "cancelled" || b(rev, "cancelled") {
            let by = s(rev, "cancelledBy");
            if by.is_empty() {
                "审查已取消".to_string()
            } else {
                format!("审查已取消（{by}）")
            }
        } else if rverdict == "PASS" {
            if rework_rounds > 0 {
                format!("审查 PASS（第 {round} 轮）")
            } else {
                "审查 PASS".to_string()
            }
        } else if rverdict == "REWORK" {
            let r = rework_rounds.max(round);
            format!("REWORK 第 {r} 轮")
        } else if rverdict == "NEEDS_CONTROLLER" {
            let sign = &status["sign"];
            if sign.is_object() && b(sign, "required") {
                if b(sign, "valid") {
                    "审查 NEEDS_CONTROLLER（已签字）".to_string()
                } else if b(sign, "invalidated") {
                    "审查 NEEDS_CONTROLLER（签字已失效）".to_string()
                } else {
                    "审查 NEEDS_CONTROLLER（待签字）".to_string()
                }
            } else {
                "审查 NEEDS_CONTROLLER".to_string()
            }
        } else {
            String::new()
        };
        if !review_label.is_empty() {
            let rev_model = {
                let mn = s(rev, "modelName");
                if !mn.is_empty() { mn } else { s(rev, "model") }
            };
            let mut full_label = if rev_model.is_empty() {
                review_label
            } else {
                format!("{review_label} · {rev_model}")
            };
            if rev["extra"].is_object() && b(&rev["extra"], "disagreed") {
                full_label.push_str(" · 意见不一致");
            }
            parts.push(full_label);
        }
        if b(rev, "needsAttention") && rverdict != "PASS" {
            parts.push("⚠ 需主控关注".to_string());
        }
    }
    if status["card"].is_object() {
        let risk_str = crate::card::format_l0_risk(&status["card"]);
        if !risk_str.is_empty() {
            parts.push(risk_str);
        }
        parts.push(crate::card::format_l0_fast_forward(
            &status["card"],
            status.get("premerge"),
        ));
    } else if status["premerge"].is_object() {
        parts.push(crate::premerge::format_l0(&status["premerge"]));
    } else if s(status, "mode") == "write" && matches!(state, "delivered" | "answered") {
        parts.push("快进 —".into());
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
    if status["conflictMarkers"]
        .as_array()
        .is_some_and(|a| !a.is_empty())
    {
        parts.push("已合入·有冲突待解决".into());
    } else if b(status, "applied") {
        parts.push(
            if s(status, "appliedBy") == "detected" {
                "已合入（主干已含改动）"
            } else {
                "已合入"
            }
            .into(),
        );
    }
    if let Some(paths) = status["protectViolation"]
        .as_array()
        .filter(|p| !p.is_empty())
    {
        let label = if s(status, "error").contains("generated output") {
            "生成物被手改"
        } else {
            "触及保护路径"
        };
        parts.push(format!("{label} {} 个", paths.len()));
    } else if !status["protectViolation"].is_null() {
        parts.push(
            if s(status, "error").contains("generated output") {
                "生成物被手改"
            } else {
                "触及保护路径"
            }
            .into(),
        );
    }
    if let Some(paths) = status["promptProtectedPaths"]
        .as_array()
        .filter(|p| !p.is_empty())
    {
        parts.push(format!("正文提及保护路径 {} 处", paths.len()));
    }
    if !status["readOnlyViolation"].is_null() || !status["workspaceChanged"].is_null() {
        parts.push(
            if b(status, "shell") {
                "只读任务产生了写入"
            } else {
                "只读工作区有改动"
            }
            .into(),
        );
    }
    for (key, label) in [
        ("sourceWriteViolation", "写到了源仓库"),
        ("sourceWorkspaceChanges", "源仓库工作区有新改动"),
    ] {
        if key == "sourceWorkspaceChanges" && !status["sourceWriteViolation"].is_null() {
            continue;
        }
        if let Some(paths) = status[key].as_array().filter(|p| !p.is_empty()) {
            parts.push(format!("{label}：{} 个文件", paths.len()));
        }
    }
    if n(status, "denied") > 0 {
        parts.push(format!("拦下 {} 次全量检查", n(status, "denied")));
    }
    if n(status, "resumes") > 0 {
        parts.push(format!(
            "上游中断后续做 {} 次（{}）",
            n(status, "resumes"),
            clip(&s(status, "transientError").replace(['\n', '\r'], " "), 60)
        ));
    }
    let occupied = crate::resources::occupied_for_run(s(status, "run"));
    if !occupied.is_empty() {
        parts.push(format!("占用 {}", occupied.join("、")));
    }
    if let Some(blind) = status["acceptBlind"]
        .as_array()
        .filter(|items| !items.is_empty())
    {
        parts.push(format!("验收未覆盖 {} 个文件", blind.len()));
    }
    if let Some(missing) = status["missingObjects"]
        .as_array()
        .filter(|items| !items.is_empty())
    {
        let list = missing
            .iter()
            .map(|item| {
                let sha = s(item, "sha");
                format!("{}={}", s(item, "ref"), &sha[..sha.len().min(12)])
            })
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("快照对象已丢失（{list}，可能被 git prune 清理）"));
    }
    let pending_notes = n(status, "pendingNotes");
    if pending_notes > 0 {
        parts.push(format!("待办 {pending_notes}"));
    }
    let id = s(status, "run");
    let next = if b(status, "cleaned") {
        format!("证据归档：{}", s(status, "archive"))
    } else if status["missingObjects"]
        .as_array()
        .is_some_and(|items| !items.is_empty())
    {
        let recovery = s(status, "recovery");
        if recovery.is_empty() {
            s(status, "next").to_string()
        } else {
            recovery.to_string()
        }
    } else if runs::active(state) {
        format!("等待交付：delegate wait {id}")
    } else if b(status, "applied") && s(status, "appliedBy") == "detected" {
        format!("清理：delegate clean {id}")
    } else if !s(status, "next").is_empty() {
        if b(status, "question") {
            format!("回答提问：delegate reply {id} '<决定>'（不计返工）")
        } else if b(status, "policyRefused")
            || state == "needs-redispatch"
            || b(status, "infrastructureFailure")
        {
            s(status, "next").to_string()
        } else if ["timeout", "stalled"].contains(&state) && s(status, "mode") == "write" {
            format!("改动保留，续做不计返工次数：{}", s(status, "next"))
        } else if matches!(state, "delivered" | "answered") {
            let rverdict = s(&status["review"], "verdict");
            if rverdict == "NEEDS_CONTROLLER" {
                let sign = &status["sign"];
                if sign.is_object() && b(sign, "required") && !b(sign, "valid") {
                    format!("先看审查卡并签字：delegate card {id}，delegate sign {id}，再 delegate apply {id}")
                } else {
                    format!("主控审查结论（NEEDS_CONTROLLER）：delegate card {id}，再 delegate reply {id} '<决定>' 或 delegate apply {id}")
                }
            } else if has_worktree {
                if status["sourceDrift"]["overlap"]
                    .as_array()
                    .is_some_and(|paths| !paths.is_empty())
                {
                    format!("源改动重叠，先看审查卡：delegate card {id}（或 delegate diff {id} --total），再 delegate apply {id} 或 delegate reply {id} --sync '<修正>'")
                } else {
                    format!("先看审查卡：delegate card {id}（或 delegate diff {id} --total），再 delegate apply {id}")
                }
            } else {
                format!("先看审查卡：delegate card {id}（或 delegate diff {id}）")
            }
        } else if state == "failed" && s(status, "error").contains("agy 登录失效") {
            "agy 登录失效，`delegate agy account status`".into()
        } else {
            format!("查看结论后接手或返工：delegate status {id} --json")
        }
    } else {
        String::new()
    };
    if !next.is_empty() {
        parts.push(format!("下一步：{next}"));
    }
    if !b(status, "cleaned") && status["eta"].is_object() {
        let eta_obj = &status["eta"];
        let eta_stage = s(eta_obj, "stage");
        let short_text = s(eta_obj, "shortText");
        if eta_stage != "done" && !short_text.is_empty() {
            parts.push(short_text.to_string());
        }
    }
    parts.join(" · ")
}

fn format_path_list(paths: &[String], limit: usize, summary_path: &Path) -> String {
    let shown = paths
        .iter()
        .take(limit)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > limit {
        format!(
            "{shown} … 共 {} 个（完整见 {}）",
            paths.len(),
            summary_path.display()
        )
    } else {
        shown
    }
}

pub fn format_source_drift(drift: &Value) -> Option<String> {
    if !drift.is_object() {
        return None;
    }
    let commits = n(drift, "commits");
    let files = n(drift, "files");
    if commits == 0 && files == 0 {
        return None;
    }
    let overlap = strings(&drift["overlap"]);
    let more = n(drift, "overlapMore");
    let overlap_count = overlap.len() as i64 + more;
    if overlap_count > 0 {
        let listed = overlap.join("、");
        let suffix = if more > 0 {
            format!("{listed} 等")
        } else {
            listed
        };
        Some(format!(
            "!!! 主干已前进 {commits} 个提交 / {files} 个文件，与本链改动重叠 {overlap_count} 个：{suffix}"
        ))
    } else {
        Some(format!(
            "主干已前进 {commits} 个提交 / {files} 个文件，与本链改动重叠 0 个"
        ))
    }
}

/// Concise summary line followed by indented path details for `delegate status <run>`.
pub fn human_detail(run: &Path, status: &Value) -> String {
    let mut lines = vec![human_line(run, status, true)];
    if status["remoteExec"].is_object() {
        let re = &status["remoteExec"];
        let host = s(re, "host");
        if !host.is_empty() {
            let queue_zh = match s(re, "queue") {
                "remote-light" => "轻队列",
                "remote-heavy" => "重队列",
                _ => "远端",
            };
            let cmd = s(re, "cmd");
            let dur = re["elapsedSeconds"]
                .as_i64()
                .map(|sec| format!("（已跑 {}）", format_short_duration_zh(sec)))
                .unwrap_or_default();
            if b(re, "active") {
                let state_zh = if b(re, "running") { "运行中" } else { "排队中" };
                let cmd_part = if cmd.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", cmd.replace(['\n', '\r'], " "))
                };
                lines.push(format!(
                    "  远端执行：@{host} · {queue_zh} · {state_zh}{cmd_part}{dur}"
                ));
            } else {
                lines.push(format!("  远端机器：@{host}"));
            }
            if let Some(tail) = re["outputTail"].as_array().filter(|a| !a.is_empty()) {
                lines.push(format!("  远端输出尾部（最近 {} 行）：", tail.len()));
                for item in tail {
                    if let Some(line) = item.as_str() {
                        lines.push(format!("    {line}"));
                    }
                }
            }
        }
    }
    let author_rounds: Vec<Value> = if let Some(arr) = status["authorRounds"].as_array() {
        arr.clone()
    } else if run.join("meta.json").is_file() {
        runs::collect_result_rounds(run, true)
            .into_iter()
            .map(|r| {
                json!({
                    "round": r.round,
                    "run": r.run_id,
                    "kind": r.kind,
                    "path": r.path.to_string_lossy(),
                    "totalLines": r.total_lines,
                    "totalChars": r.total_chars,
                    "current": r.current,
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    let cur_run_id = s(status, "run");
    let cur_is_active = runs::active(s(status, "state"));
    let cur_missing_from_rounds = cur_is_active
        && !cur_run_id.is_empty()
        && !author_rounds.iter().any(|r| s(r, "run") == cur_run_id);
    let total_rounds = author_rounds.len() + usize::from(cur_missing_from_rounds);
    if total_rounds > 1 {
        let mut unique_runs: std::collections::BTreeSet<&str> = author_rounds
            .iter()
            .map(|r| s(r, "run"))
            .filter(|id| !id.is_empty())
            .collect();
        if cur_missing_from_rounds {
            unique_runs.insert(cur_run_id);
        }
        let multi_run = unique_runs.len() > 1;
        let hint = if !cur_run_id.is_empty() && !author_rounds.is_empty() {
            format!(
                "，查看：delegate result {cur_run_id} --round <1..={}> / --all",
                author_rounds.len()
            )
        } else {
            String::new()
        };
        lines.push(format!("  各轮原文（共 {total_rounds} 轮{hint}）："));
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
            let is_cur = b(r, "current") && !cur_missing_from_rounds;
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
            lines.push(format!(
                "    - 第 {rn} 轮（{tag_str}）：{rpath}（共 {rlines} 行 / {rchars} 字）"
            ));
        }
        if cur_missing_from_rounds {
            let rn = author_rounds.len() + 1;
            let tag_str = if multi_run {
                format!("{cur_run_id} · reply，进行中")
            } else {
                "reply，进行中".to_string()
            };
            lines.push(format!("    - 第 {rn} 轮（{tag_str}）：尚未产出原文"));
        }
    }
    if !b(status, "cleaned") && status["eta"].is_object() {
        let eta_obj = &status["eta"];
        let eta_stage = s(eta_obj, "stage");
        let detail_text = s(eta_obj, "detailText");
        if eta_stage != "done" && !detail_text.is_empty() {
            lines.push(format!("  {detail_text}"));
        }
    }
    let summary_path = run.join("summary.json");
    let meta = json(run.join("meta.json"));
    if status["stallWarning"].is_object() {
        let sw = &status["stallWarning"];
        let det = s(sw, "detail");
        let sc = s(sw, "selfCheck");
        if !det.is_empty() {
            lines.push(format!("  ⚠ 卡住提醒（{}）：{det}", s(sw, "kind")));
        }
        if !sc.is_empty() {
            lines.push(format!("  自检建议：{sc}"));
        }
    }
    if let Some(drift_line) = format_source_drift(&status["sourceDrift"]) {
        lines.push(format!("  {drift_line}"));
    }
    if let Some(line) =
        crate::agy_accounts::format_account_switch_line(&status["agyAccountSwitch"], false)
    {
        lines.push(format!("  {line}"));
    }
    if let Some(line) =
        crate::agy_accounts::format_account_switch_line(&status["reviewAgyAccountSwitch"], true)
    {
        lines.push(format!("  {line}"));
    }
    if b(&meta, "netBaseRebased") && !s(&meta, "netBase").is_empty() {
        let sha = s(&meta, "netBase");
        lines.push(format!("  netBase: {}", &sha[..sha.len().min(7)]));
    }
    let uncommitted = strings(&status["commitsDiffer"]);
    if !uncommitted.is_empty() {
        lines.push(format!(
            "  commits differ from the final tree: {}",
            format_path_list(&uncommitted, 20, &summary_path)
        ));
    }
    let violated = strings(&status["protectViolation"]);
    if !violated.is_empty() {
        let label = if s(status, "error").contains("generated output") {
            "生成物被手改"
        } else {
            "触及保护路径"
        };
        lines.push(format!(
            "  {label}：{}",
            format_path_list(&violated, 20, &summary_path)
        ));
    }
    let prompt_protected = strings(&status["promptProtectedPaths"]);
    if !prompt_protected.is_empty() {
        let list = prompt_protected
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("、");
        let flags = prompt_protected
            .iter()
            .take(10)
            .map(|p| format!("--allow {}", shell_quote(p)))
            .collect::<Vec<_>>()
            .join(" ");
        let more = if prompt_protected.len() > 10 {
            format!(" 等共 {} 处", prompt_protected.len())
        } else {
            String::new()
        };
        lines.push(format!("  正文提及保护路径：{list}{more}（如需修改：{flags}）"));
    }
    for (key, label) in [
        ("sourceWriteViolation", "写到了源仓库"),
        ("sourceWorkspaceChanges", "源仓库工作区有新改动"),
    ] {
        if key == "sourceWorkspaceChanges" && !status["sourceWriteViolation"].is_null() {
            continue;
        }
        let paths = strings(&status[key]);
        if !paths.is_empty() {
            lines.push(format!(
                "  {label}：{} 个文件（{}）",
                paths.len(),
                format_path_list(&paths, 20, &summary_path)
            ));
        }
    }
    let blind = blind_note(&status["acceptBlind"]);
    if !blind.is_empty() {
        lines.push(format!("  {blind}"));
    }
    if status["accept"]["layers"].as_array().is_some_and(|a| !a.is_empty()) {
        let acc = &status["accept"];
        if b(acc, "failClosed") {
            lines.push(format!("  验收全量兜底：{}", s(acc, "failClosedReason")));
        }
        for layer in acc["layers"].as_array().unwrap() {
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
            let reason = s(layer, "reason");
            lines.push(format!("  验收层 [{st_zh}] {lname} ({tier})：{reason}"));
        }
    }
    if status["review"].is_object() {
        let rev = &status["review"];
        let verdict = s(rev, "verdict");
        let orig = s(rev, "originalVerdict");
        let rstatus = s(rev, "status");
        let round = n(rev, "round");
        let rework_rounds = n(rev, "reworkRounds");
        let up = s(rev, "upgradedBy");
        let mut info = Vec::new();
        if rstatus == "reviewing" {
            info.push(format!("状态：审查中（第 {} 轮）", round.max(1)));
        } else if rstatus == "reworking" {
            info.push(format!("状态：REWORK 第 {} 轮（返工中）", rework_rounds.max(round).max(1)));
        } else if !verdict.is_empty() {
            info.push(format!("结论：{verdict}（共 {round} 轮，返工 {rework_rounds} 轮）"));
        }
        if !orig.is_empty() && orig != verdict {
            info.push(format!("原判：{orig}"));
        }
        if !up.is_empty() {
            info.push(format!("升级：{up}"));
        }
        let rev_model_name = s(rev, "modelName");
        let rev_model_id = s(rev, "model");
        if !rev_model_name.is_empty() && !rev_model_id.is_empty() && rev_model_name != rev_model_id
        {
            info.push(format!("模型：{rev_model_name}（{rev_model_id}）"));
        } else if !rev_model_name.is_empty() {
            info.push(format!("模型：{rev_model_name}"));
        } else if !rev_model_id.is_empty() {
            info.push(format!("模型：{rev_model_id}"));
        }
        if b(rev, "formatRetryFailed") || (b(rev, "formatError") && b(rev, "formatRetried")) {
            info.push("补问 1 次仍无结论".into());
        }
        if rev["extra"].is_object() && b(&rev["extra"], "disagreed") {
            info.push("意见不一致".into());
        }
        if !info.is_empty() {
            lines.push(format!("  审查：{}", info.join(" · ")));
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
                    lines.push(format!("  审查改用 {to}"));
                } else {
                    lines.push(format!("  审查改用 {to}（{reason}）"));
                }
            }
        }
        if rev["extra"].is_object() {
            let ex = &rev["extra"];
            let em = {
                let n = s(ex, "modelName");
                if !n.is_empty() { n } else { s(ex, "model") }
            };
            match s(ex, "status") {
                "skipped" => {
                    let sr = s(ex, "skipReason");
                    if !sr.is_empty() {
                        lines.push(format!("  {sr}"));
                    }
                }
                "done" => {
                    let ev = s(ex, "verdict");
                    let disagree = if b(ex, "disagreed") {
                        " · 意见不一致"
                    } else {
                        ""
                    };
                    let mut ex_parts = Vec::new();
                    if !em.is_empty() {
                        ex_parts.push(format!("模型：{em}"));
                    }
                    if !ev.is_empty() {
                        ex_parts.push(format!("结论：{ev}{disagree}"));
                    }
                    if !ex_parts.is_empty() {
                        lines.push(format!("  补充审查：{}", ex_parts.join(" · ")));
                    }
                    if !s(ex, "verdictFile").is_empty() {
                        lines.push(format!("  补充审查文件：{}", s(ex, "verdictFile")));
                    }
                }
                "error" => {
                    let err = s(ex, "error");
                    lines.push(format!(
                        "  补充审查：模型：{em} · 失败：{}",
                        if err.is_empty() { "执行异常" } else { err }
                    ));
                }
                _ => {}
            }
        }
        if !s(rev, "parseError").is_empty() {
            lines.push(format!("  审查异常：{}", s(rev, "parseError")));
        }
        if !s(rev, "roViolationFile").is_empty() {
            lines.push(format!(
                "  只读违规清单（{} 项）：{}",
                n(rev, "roViolationCount"),
                s(rev, "roViolationFile")
            ));
        }
        if !s(rev, "verdictFile").is_empty() {
            lines.push(format!("  审查结论文件：{}", s(rev, "verdictFile")));
        }
        let cr = &rev["controllerReview"];
        if cr.is_object() && !s(cr, "hash").is_empty() {
            let paths = strings(&cr["paths"]);
            lines.push(format!("  controllerReviewPaths：{}", paths.join("、")));
            lines.push(format!("  签字哈希：{}", s(cr, "hash")));
            if !s(cr, "diffFile").is_empty() {
                lines.push(format!("  diff 文件：{}", s(cr, "diffFile")));
            }
        }
        let sign = &status["sign"];
        if sign.is_object() && b(sign, "required") {
            let id = s(status, "run");
            if b(sign, "valid") {
                lines.push(format!(
                    "  签字状态：已签字有效（{}，于 {}）",
                    s(sign, "signedHash"),
                    s(sign, "signedAt")
                ));
            } else if b(sign, "invalidated") {
                lines.push(format!(
                    "  签字状态：已失效（原签字 {} ≠ 当前 {}）；重新审查后执行 delegate sign {id}",
                    s(sign, "signedHash"),
                    s(sign, "hash")
                ));
            } else {
                lines.push(format!("  签字状态：未签字（确认无误后执行 delegate sign {id}）"));
            }
        }
    }
    if let Some(items) = status["notes"].as_array().filter(|items| !items.is_empty()) {
        let pending = n(status, "pendingNotes");
        lines.push(format!("  待办（未完成 {pending} / 共 {}）：", items.len()));
        for item in items {
            let idx = n(item, "index");
            let mark = if b(item, "done") { "x" } else { " " };
            let text = s(item, "text");
            let created = s(item, "createdAt");
            let done_suffix = if !s(item, "doneAt").is_empty() {
                format!("，完成于 {}", s(item, "doneAt"))
            } else {
                String::new()
            };
            lines.push(format!("    {idx}. [{mark}] {text} ({created}{done_suffix})"));
        }
    }
    lines.join("\n")
}

pub fn human_cleaned_note(chain: &crate::notes::ChainNotes, detail: bool) -> String {
    let pending = chain.pending_count();
    let mut parts = vec![chain.name.replace(['\n', '\r'], " ")];
    if chain.applied {
        parts.push("已合入 · 已清理".into());
    } else {
        parts.push("已清理".into());
    }
    if pending > 0 {
        parts.push(format!("待办 {pending}"));
        parts.push(format!(
            "下一步：delegate note {} --done",
            chain.latest_run
        ));
    }
    let mut lines = vec![parts.join(" · ")];
    if detail && !chain.items.is_empty() {
        lines.push(format!(
            "  待办（未完成 {pending} / 共 {}）：",
            chain.items.len()
        ));
        for (i, item) in chain.items.iter().enumerate() {
            let mark = if item.done { "x" } else { " " };
            let done_suffix = item
                .done_at
                .as_deref()
                .map(|t| format!("，完成于 {t}"))
                .unwrap_or_default();
            lines.push(format!(
                "    {}. [{}] {} ({}{})",
                i + 1,
                mark,
                item.text,
                item.created_at,
                done_suffix
            ));
        }
    }
    lines.join("\n")
}

pub fn cleaned_note_json(chain: &crate::notes::ChainNotes) -> Value {
    let pending = chain.pending_count();
    let mut obj = json!({
        "run": chain.latest_run,
        "name": chain.name,
        "state": "cleaned",
        "cleaned": true,
        "applied": chain.applied,
        "notes": chain.items_json(),
        "pendingNotes": pending,
    });
    if pending > 0 {
        obj["next"] = json!(format!("delegate note {} --done", chain.latest_run));
    }
    obj
}

/// `pi/<tier>` for a tier's pick; `pi/<model>` for a pinned model, which no tier chose.
fn agent_label(status: &Value) -> String {
    // Name the model on every line: a caller should see what it is talking to, and the tier
    // (with "推断" when the caller gave none) says why that model was picked.
    let model = s(status, "model");
    // An alias names the model; a full provider/id shows only its id.
    let name = match s(status, "modelName") {
        alias if !alias.is_empty() && !alias.contains('/') => alias,
        _ => model.rsplit_once('/').map_or(model, |(_, name)| name),
    };
    let agent = s(status, "agent");
    let tier = s(status, "tier");
    let pinned = status["agentPinned"].as_bool() == Some(true);
    match (name.is_empty(), pinned || tier.is_empty()) {
        (true, _) => format!("{agent}/{tier}"),
        (false, true) => format!("{agent}/{name}"),
        (false, false) => format!(
            "{agent}/{name} · {tier}{}",
            if b(status, "tierInferred") {
                "（推断）"
            } else {
                ""
            }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{agent_label, human_detail, human_line, pending_label};
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn a_pinned_model_is_named_instead_of_the_tier() {
        let tier = json!({"agent":"pi","tier":"review"});
        assert_eq!(agent_label(&tier), "pi/review");
        let picked = json!({"agent":"pi","tier":"review","tierInferred":true,
            "model":"provider/gpt-sol","modelName":"sol"});
        assert_eq!(agent_label(&picked), "pi/sol · review（推断）");
        let pinned = json!({"agent":"pi","tier":"review","agentPinned":true,
            "model":"provider/deepseek-flash"});
        assert_eq!(agent_label(&pinned), "pi/deepseek-flash");
    }

    #[test]
    fn a_failed_pending_reply_shows_its_error() {
        assert_eq!(pending_label(&json!(null)), None);
        assert_eq!(pending_label(&json!({"message":"m"})).unwrap(), "待续答 / pending reply");
        assert!(pending_label(&json!({"error":"rework limit reached"}))
            .unwrap()
            .contains("pending reply failed: rework limit reached"));
    }

    #[test]
    fn collapsed_chain_and_pending_notes_render_in_summary_and_detail() {
        let status = json!({
            "run": "20261122-100200-r3",
            "name": "reply to reply to 合入员",
            "chainName": "合入员",
            "chainRound": 3,
            "state": "running",
            "elapsedSeconds": 120,
            "agent": "pi",
            "tier": "write",
            "pendingNotes": 2,
            "notes": [
                {"index": 1, "text": "换线上服务", "createdAt": "2026-11-22T02:00:00Z", "done": false},
                {"index": 2, "text": "真机验证", "createdAt": "2026-11-22T02:01:00Z", "done": false}
            ]
        });
        let dummy = Path::new("/nonexistent/delegate-test-run");
        let collapsed = human_line(dummy, &status, true);
        assert!(collapsed.starts_with("合入员（第 3 轮） · 运行中 2 分钟"), "{collapsed}");
        assert!(collapsed.contains("待办 2"), "{collapsed}");

        let raw = human_line(dummy, &status, false);
        assert!(raw.starts_with("reply to reply to 合入员 · 运行中 2 分钟"), "{raw}");

        let detail = human_detail(dummy, &status);
        assert!(detail.contains("待办（未完成 2 / 共 2）"), "{detail}");
        assert!(detail.contains("1. [ ] 换线上服务 (2026-11-22T02:00:00Z)"), "{detail}");
        assert!(detail.contains("2. [ ] 真机验证 (2026-11-22T02:01:00Z)"), "{detail}");
    }

    #[test]
    fn eta_appends_one_tail_segment_on_l0_and_breakdown_line_in_detail() {
        let dummy = Path::new("/nonexistent/delegate-test-eta");
        let active = json!({
            "run": "20261125-030000-feat",
            "name": "feat-eta",
            "state": "running",
            "elapsedSeconds": 240,
            "agent": "pi",
            "tier": "write",
            "eta": {
                "stage": "review_parallel",
                "shortText": "预计 03:40 交付（p75 04:10）",
                "detailText": "预估：03:40 交付（p75 04:10）〔慢层+审查 4m，剩 ~6m；返工率 28%〕"
            }
        });
        let l0 = human_line(dummy, &active, true);
        assert!(l0.ends_with(" · 预计 03:40 交付（p75 04:10）"), "{l0}");
        let det = human_detail(dummy, &active);
        let det_lines: Vec<&str> = det.lines().collect();
        assert_eq!(det_lines.len(), 2, "{det}");
        assert_eq!(
            det_lines[1],
            "  预估：03:40 交付（p75 04:10）〔慢层+审查 4m，剩 ~6m；返工率 28%〕"
        );

        let blocked = json!({
            "run": "20261125-030001-sign",
            "name": "sign-eta",
            "state": "delivered",
            "elapsedSeconds": 300,
            "agent": "pi",
            "tier": "write",
            "eta": {
                "stage": "blocked",
                "stoppedClock": true,
                "shortText": "卡在主控·待签字 18m",
                "detailText": "停表：卡在主控·待签字 18m"
            }
        });
        let l0_blk = human_line(dummy, &blocked, true);
        assert!(l0_blk.ends_with(" · 卡在主控·待签字 18m"), "{l0_blk}");
        let det_blk = human_detail(dummy, &blocked);
        assert!(det_blk.contains("  停表：卡在主控·待签字 18m"), "{det_blk}");

        let done = json!({
            "run": "20261125-030002-done",
            "name": "done-eta",
            "state": "delivered",
            "applied": true,
            "elapsedSeconds": 300,
            "agent": "pi",
            "tier": "write",
            "eta": {
                "stage": "done",
                "shortText": "已合入",
                "detailText": "已合入"
            }
        });
        let l0_done = human_line(dummy, &done, true);
        assert_eq!(l0_done.matches("已合入").count(), 1, "{l0_done}");
        assert_eq!(human_detail(dummy, &done).lines().count(), 1);
    }

    #[test]
    fn remote_off_and_active_lane_render_in_human_line() {
        let dummy = Path::new("/nonexistent/delegate-test-lane-tag");
        let st_off = json!({
            "run": "20261126-100000-off",
            "name": "local-task",
            "state": "running",
            "elapsedSeconds": 10,
            "agent": "pi",
            "tier": "write",
            "remoteOff": true,
            "lane": {"queue": "local", "running": true, "label": "accept 20261126-100000-off"}
        });
        let line_off = human_line(dummy, &st_off, false);
        assert!(line_off.contains("远程：关"), "{line_off}");
        assert!(line_off.contains("本机队列（运行中）"), "{line_off}");

        let st_light = json!({
            "run": "20261126-100001-light",
            "name": "cpu-check",
            "state": "running",
            "elapsedSeconds": 12,
            "agent": "pi",
            "tier": "write",
            "lane": {"queue": "remote-light", "running": true, "label": "accept 20261126-100001-light"}
        });
        assert!(human_line(dummy, &st_light, false).contains("远端轻队列（运行中）"));

        let st_heavy = json!({
            "run": "20261126-100002-heavy",
            "name": "gpu-train",
            "state": "running",
            "elapsedSeconds": 60,
            "agent": "pi",
            "tier": "write",
            "lane": {"queue": "remote-heavy", "running": false, "label": "gpu: uv run train.py"}
        });
        assert!(human_line(dummy, &st_heavy, false).contains("远端重队列（排队中）"));

        let st_remote = json!({
            "run": "20261126-100003-m4",
            "name": "reply to 底盘蒸馏 M4",
            "chainName": "底盘蒸馏 M4",
            "chainRound": 2,
            "state": "running",
            "elapsedSeconds": 300,
            "agent": "agy",
            "tier": "write",
            "remoteExec": {
                "host": "gpu-1",
                "active": true,
                "running": true,
                "queue": "remote-heavy",
                "cmd": "uv run python train.py",
                "elapsedSeconds": 120,
                "outputTail": ["epoch 1 loss=0.4", "epoch 2 loss=0.2"]
            },
            "authorRounds": [
                {"round": 1, "run": "20261126-090000-m4-r1", "kind": "initial", "path": "/tmp/archive/m4-r1/record/result.md", "totalLines": 20, "totalChars": 600, "current": false},
                {"round": 2, "run": "20261126-090000-m4-r1", "kind": "rework", "path": "/tmp/archive/m4-r1/record/round-2-result.md", "totalLines": 25, "totalChars": 750, "current": false}
            ]
        });
        let line = super::human_line_with_repo(dummy, &st_remote, true, Some("rl-sim"));
        assert!(line.starts_with("[rl-sim] 底盘蒸馏 M4（第 2 轮）"), "{line}");
        assert!(line.contains("远端:@gpu-1 uv run python train.py 2 分钟"), "{line}");
        let detail = super::human_detail(dummy, &st_remote);
        assert!(detail.contains("远端执行：@gpu-1 · 重队列 · 运行中 · uv run python train.py（已跑 2 分钟）"), "{detail}");
        assert!(detail.contains("远端输出尾部（最近 2 行）"), "{detail}");
        assert!(detail.contains("epoch 2 loss=0.2"), "{detail}");
        assert!(detail.contains("各轮原文（共 3 轮"), "{detail}");
        assert!(detail.contains("round-2-result.md（共 25 行 / 750 字）"), "{detail}");
        assert!(detail.contains("第 3 轮（20261126-100003-m4 · reply，进行中）"), "{detail}");
    }
}
