//! Automatically maintained per-workspace delegation status board (`BOARD.md` or
//! `.delegate.json` `board`). Atomic projection from live run metadata and `history.jsonl`;
//! write errors log diagnostics and never block delegation operations.
use crate::archive;
use crate::common::*;
use crate::config;
use crate::history::{self, HistoryEntry};
use crate::runs;
use crate::worktree;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn clean_inline(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn first_conclusion_sentence(conclusion: &str) -> String {
    for line in conclusion.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // Skip bare markdown headings like `## 结论` so the actual finding shows up.
        let stripped = t.trim_start_matches('#').trim();
        if t.starts_with('#') && stripped.chars().count() <= 12 {
            continue;
        }
        let body = stripped
            .trim_start_matches(['-', '*', '•', ' '])
            .trim();
        if !body.is_empty() {
            return history::clip_chars(body, 60);
        }
    }
    history::clip_chars(&clean_inline(conclusion), 60)
}

fn format_duration_short(seconds: i64) -> String {
    let s = seconds.max(0);
    if s < 60 {
        format!("{s} 秒")
    } else if s < 3600 {
        format!("{} 分钟", s / 60)
    } else {
        format!("{} 小时 {} 分", s / 3600, (s % 3600) / 60)
    }
}

fn format_short_time(iso_str: &str) -> String {
    let Some(ts) = iso_epoch(iso_str) else {
        return "未知时间".into();
    };
    let today = format_time(true, "%Y-%m-%d");
    let day = format_epoch(ts, true, "%Y-%m-%d");
    if day == today {
        format_epoch(ts, true, "%H:%M")
    } else {
        format_epoch(ts, true, "%m-%d %H:%M")
    }
}

fn latest_step(run: &Path, meta: &Value) -> String {
    let evs = runs::events(run);
    if let Some(last) = evs.iter().rev().find(|x| {
        !["session", "turn", "result", "settled", "agent_end", "bash_done"].contains(&s(x, "e"))
    }) {
        let ev = s(last, "e");
        let detail = ["cmd", "path", "arg", "detail"]
            .iter()
            .map(|k| s(last, k))
            .find(|x| !x.is_empty())
            .unwrap_or("");
        if detail.is_empty() {
            return history::clip_chars(ev, 48);
        }
        let mut clean = clean_inline(detail);
        for root in [
            s(meta, "top"),
            s(meta, "workdir"),
            s(&meta["worktree"], "source"),
        ] {
            if !root.is_empty() {
                clean = clean.replace(&format!("{root}/"), "").replace(root, ".");
            }
        }
        return history::clip_chars(&format!("{ev}: {clean}"), 48);
    }
    "等待动作".into()
}

fn changes_scale(files_count: usize, added: i64, deleted: i64, dirs: &BTreeMap<String, usize>) -> String {
    if files_count == 0 {
        return "无改动".into();
    }
    let mut top: Vec<(&String, &usize)> = dirs.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let dir_part = top
        .iter()
        .take(2)
        .map(|(d, c)| format!("{d} {c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = format!("改 {files_count} 文件 +{added}/-{deleted}");
    if !dir_part.is_empty() {
        out.push_str(&format!("（{dir_part}）"));
    }
    out
}

fn accept_short_verdict(meta: &Value, sum: &Value) -> &'static str {
    if sum["accept"].is_object() {
        if b(&sum["accept"], "notExecuted") {
            "验收未执行"
        } else if b(sum, "infrastructureFailure") {
            "验收基础设施失败"
        } else if b(&sum["accept"], "ok") {
            "验收通过"
        } else {
            "验收未通过"
        }
    } else if !s(meta, "accept").is_empty() {
        "验收未运行"
    } else {
        "未设验收"
    }
}

fn failure_reason(st: &str, sum: &Value) -> String {
    let label = history::state_label(st, false, false);
    if !sum["protectViolation"].is_null() {
        return format!("{label}：触及保护路径");
    }
    if !sum["sourceWriteViolation"].is_null() {
        return format!("{label}：写到了源仓库");
    }
    if !sum["readOnlyViolation"].is_null() {
        return format!("{label}：只读任务产生了写入");
    }
    if b(&sum["accept"], "notExecuted") || b(sum, "infrastructureFailure") {
        return format!("{label}：验收未执行（排队/基础设施）");
    }
    if st == "rejected" && sum["accept"].is_object() && !b(&sum["accept"], "ok") {
        return format!("{label}：验收未通过");
    }
    let err = s(sum, "error");
    if !err.is_empty() {
        return format!("{label}：{}", history::clip_chars(&clean_inline(err), 48));
    }
    let transient = s(sum, "transientError");
    if !transient.is_empty() {
        return format!(
            "{label}：{}",
            history::clip_chars(&clean_inline(transient), 48)
        );
    }
    label
}

fn failure_next_short(id: &str, st: &str, sum: &Value) -> String {
    if b(sum, "infrastructureFailure") || b(&sum["accept"], "notExecuted") {
        return format!("delegate reply {id} '重试验收'");
    }
    match st {
        "rejected" | "malformed" => format!("delegate reply {id} '<修正>' 或接手"),
        "timeout" | "stalled" => format!("delegate reply {id} '继续完成'（不计返工）"),
        "stopped" => format!("delegate clean {id}"),
        "needs-redispatch" => format!("delegate status {id} 获取重派命令"),
        _ => format!("delegate result {id} 或重派"),
    }
}

fn current_source_root() -> Option<PathBuf> {
    let cwd = env::current_dir().ok()?;
    if let Some(top) = git_top(&cwd) {
        return Some(top);
    }
    // In tests or non-git cwds with explicit DELEGATE_RUNS, infer source repo from latest run.
    if env::var_os("DELEGATE_RUNS")
        .or_else(|| env::var_os("PI_DELEGATE_RUNS"))
        .is_some_and(|v| !v.is_empty())
    {
        if let Some(last) = runs::all_runs().last() {
            let meta = json(last.join("meta.json"));
            let root = history::source_root_for_meta(&meta);
            if !root.is_empty() {
                return Some(PathBuf::from(root));
            }
        }
    }
    None
}

pub fn resolve_board_path(key: &str, source_root: Option<&Path>) -> PathBuf {
    if let Some(root) = source_root.filter(|p| !p.as_os_str().is_empty()) {
        if let Ok(cfg) = config::load(Some(root)) {
            if let Some(board) = cfg.value["board"].as_str().filter(|s| !s.trim().is_empty()) {
                let p = Path::new(board);
                return if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    root.join(p)
                };
            }
        }
    }
    history::history_dir_for_key(key).join("BOARD.md")
}

fn canonical_run(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn machine_in_flight_count(local_active: &[PathBuf]) -> usize {
    // Read-only scan of slot files: never block on `.start.lock`, because board refresh
    // is called from inside `start` and `admit_waiting` while `.start.lock` may be held.
    let mut set: HashSet<PathBuf> = fs::read_dir(state_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "slot"))
        .map(|entry| PathBuf::from(read(entry.path()).trim()))
        .filter(|run| run.join("meta.json").is_file() && runs::active(&runs::state(run)))
        .map(|p| canonical_run(&p))
        .collect();
    for run in local_active {
        set.insert(canonical_run(run));
    }
    set.len()
}

fn runs_in_root(runs_dir: Option<&Path>) -> Vec<PathBuf> {
    let Some(dir) = runs_dir.filter(|d| d.is_dir()) else {
        return runs::all_runs();
    };
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("meta.json").is_file())
        .collect();
    v.sort_by_key(|p| json(p.join("meta.json"))["startedNs"].as_u64().unwrap_or(0));
    v
}

pub fn render(key: &str) -> String {
    render_for(key, None)
}

pub fn render_for(key: &str, runs_dir: Option<&Path>) -> String {
    let all = runs_in_root(runs_dir);
    let hist_entries = history::load_entries(key);
    let hist_by_id: HashMap<String, &HistoryEntry> =
        hist_entries.iter().map(|e| (e.run.clone(), e)).collect();

    let replied_ids = runs::superseded_by_reply(&all, &hist_entries);

    let now = epoch() as i64;
    let day_cutoff = now - 86400;

    // Each vec holds (sort_ts, id, line) so every section is sorted oldest-on-top, newest-on-bottom.
    let mut running_rows: Vec<(i64, String, String)> = Vec::new();
    let mut pending_apply_rows: Vec<(i64, String, String)> = Vec::new();
    let mut pending_answer_rows: Vec<(i64, String, String)> = Vec::new();
    let mut applied_24h_map: HashMap<String, (i64, String)> = HashMap::new();
    let mut failed_rows: Vec<(i64, String, String)> = Vec::new();

    let mut running_exec = 0usize;
    let mut queued_count = 0usize;
    let mut local_active_paths = Vec::new();
    let mut all_known_ids = HashSet::<String>::new();

    for run in &all {
        let meta = json(run.join("meta.json"));
        let st = runs::state(run);
        let sum = json(run.join("summary.json"));
        let id = if !s(&meta, "run").is_empty() {
            s(&meta, "run").to_string()
        } else {
            run.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        all_known_ids.insert(id.clone());
        let name = clean_inline(s(&meta, "name"));
        let started_ts = meta["startedEpoch"]
            .as_i64()
            .or_else(|| iso_epoch(s(&meta, "startedAt")))
            .unwrap_or(now);
        let finished_iso = s(&sum, "finishedAt");
        let finished_ts = iso_epoch(finished_iso).unwrap_or(started_ts);

        let is_replied = replied_ids.contains(&id);
        let applied_json = json(run.join(".applied"));
        let applied_at = if !s(&meta, "appliedAt").is_empty() {
            s(&meta, "appliedAt").to_string()
        } else {
            s(&applied_json, "at").to_string()
        };
        let applied_head = if !s(&meta, "appliedHead").is_empty() {
            s(&meta, "appliedHead").to_string()
        } else {
            s(&applied_json, "head").to_string()
        };
        let is_applied = (run.join(".applied").exists() && !run.join(".generate-pending").exists())
            || !applied_at.is_empty();
        let has_explicit_read = !s(&meta, "resultReadAt").is_empty();
        let is_delivered_read =
            has_explicit_read || (run.join(".delivered").exists() && !run.join(".truncated").exists());

        // Track 24h applied runs from live dirs (merged with history below).
        if is_applied && !applied_at.is_empty() {
            if let Some(ts) = iso_epoch(&applied_at) {
                if ts >= day_cutoff {
                    let head_part = if applied_head.len() >= 7 {
                        format!(" · HEAD {}", &applied_head[..7])
                    } else if !applied_head.is_empty() {
                        format!(" · HEAD {applied_head}")
                    } else {
                        String::new()
                    };
                    let line = format!(
                        "- {name} · 合入 {}{head_part} · delegate show {id}",
                        format_short_time(&applied_at)
                    );
                    applied_24h_map.insert(id.clone(), (ts, line));
                }
            }
        }

        if matches!(st.as_str(), "running" | "starting" | "waiting") {
            local_active_paths.push(run.clone());
            let model_label = history::compact_model_label(
                s(&meta, "agent"),
                s(&meta, "model"),
                s(&meta, "modelName"),
                s(&meta, "tier"),
            );
            let tier_label = if !s(&meta, "tier").is_empty() {
                s(&meta, "tier")
            } else {
                "固定"
            };
            let prompt = history::extract_user_prompt(run, &meta);
            let first_line = history::clip_chars(&clean_inline(&history::prompt_first_line(&prompt)), 60);
            let (time_part, step_part) = if st == "waiting" {
                queued_count += 1;
                let q = crate::queue::status(run);
                let since_ts = q
                    .as_ref()
                    .and_then(|v| iso_epoch(s(v, "since")))
                    .unwrap_or(started_ts);
                let reason = q
                    .as_ref()
                    .map(|v| s(v, "reason").to_string())
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| "容量排队".into());
                (
                    format!("排队 {}", format_duration_short(now - since_ts)),
                    format!("排队中（{}）", history::clip_chars(&reason, 36)),
                )
            } else {
                running_exec += 1;
                (
                    format!("已跑 {}", format_duration_short(now - started_ts)),
                    latest_step(run, &meta),
                )
            };
            let line = format!(
                "- {name} · {model_label} · {tier_label} · {time_part} · {step_part} · {first_line} · delegate show {id}"
            );
            running_rows.push((started_ts, id, line));
            continue;
        }

        let unresolved_conflicts = sum["conflictMarkers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|path| {
                !worktree::conflict_labels(&Path::new(s(&meta["worktree"], "source")).join(path))
                    .is_empty()
            });

        if b(&sum, "question") {
            if !is_replied {
                let conclusion = history::extract_conclusion(&read(run.join("result.md")));
                let summary_line = first_conclusion_sentence(&conclusion);
                let line = format!(
                    "- {name} · 提问 · 交付 {} · {} · delegate show {id}",
                    format_short_time(finished_iso),
                    if summary_line.is_empty() {
                        "等待主控决定".into()
                    } else {
                        summary_line
                    }
                );
                pending_answer_rows.push((finished_ts, id, line));
            }
            continue;
        }

        if matches!(st.as_str(), "delivered" | "answered") {
            let changes_val = sum
                .get("pendingChanges")
                .filter(|v| v.is_object())
                .unwrap_or(&sum["changes"]);
            let pending_files = runs::chain_pending_apply_files(run, &meta, &sum, &all);
            let has_worktree_changes = pending_files > 0;
            if unresolved_conflicts || (has_worktree_changes && !is_applied && !is_replied) {
                let conclusion = history::extract_conclusion(&read(run.join("result.md")));
                let summary_line = first_conclusion_sentence(&conclusion);
                let (files, dirs) = history::files_and_dirs(run, &meta, &sum);
                let fc = changes_val["files"]
                    .as_u64()
                    .map(|x| x as usize)
                    .unwrap_or(files.len());
                let scale = changes_scale(
                    fc,
                    n(changes_val, "added"),
                    n(changes_val, "deleted"),
                    &dirs,
                );
                let verdict = if unresolved_conflicts {
                    "有冲突待解"
                } else {
                    accept_short_verdict(&meta, &sum)
                };
                let line = format!(
                    "- {name} · 交付 {} · {} · {scale} · {verdict} · delegate show {id}",
                    format_short_time(finished_iso),
                    if summary_line.is_empty() {
                        "已交付改动".into()
                    } else {
                        summary_line
                    }
                );
                pending_apply_rows.push((finished_ts, id, line));
            } else if !is_applied && !is_replied && !is_delivered_read {
                let conclusion = history::extract_conclusion(&read(run.join("result.md")));
                let summary_line = first_conclusion_sentence(&conclusion);
                let line = format!(
                    "- {name} · 待读 · 交付 {} · {} · delegate show {id}",
                    format_short_time(finished_iso),
                    if summary_line.is_empty() {
                        "已答复，待查看".into()
                    } else {
                        summary_line
                    }
                );
                pending_answer_rows.push((finished_ts, id, line));
            }
            continue;
        }

        // Failure or interruption states: rejected, failed, timeout, stalled, malformed,
        // crashed, killed, stopped, needs-redispatch.
        // Stay visible until replied to, explicitly read with `result`, applied, or cleaned.
        let handled = if st == "needs-redispatch" {
            false
        } else {
            is_replied || has_explicit_read || is_applied || run.join(".applied-cleaned").exists()
        };
        if !handled {
            let reason = failure_reason(&st, &sum);
            let next = failure_next_short(&id, &st, &sum);
            let line = format!("- {name} · {reason} · 下一步：{next} · delegate show {id}");
            failed_rows.push((finished_ts, id, line));
        }
    }

    // Merge 24h applied entries from history.jsonl (covers cleaned runs applied in last 24h).
    for e in &hist_entries {
        all_known_ids.insert(e.run.clone());
        if !e.applied_at.is_empty() && !applied_24h_map.contains_key(&e.run) {
            if let Some(ts) = iso_epoch(&e.applied_at) {
                if ts >= day_cutoff {
                    let head_part = if e.applied_head.len() >= 7 {
                        format!(" · HEAD {}", &e.applied_head[..7])
                    } else if !e.applied_head.is_empty() {
                        format!(" · HEAD {}", e.applied_head)
                    } else {
                        String::new()
                    };
                    let name = clean_inline(&e.name);
                    let line = format!(
                        "- {name} · 合入 {}{head_part} · delegate show {}",
                        format_short_time(&e.applied_at),
                        e.run
                    );
                    applied_24h_map.insert(e.run.clone(), (ts, line));
                }
            }
        }
    }

    running_rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    pending_apply_rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    pending_answer_rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut applied_24h_rows: Vec<(i64, String, String)> = applied_24h_map
        .into_iter()
        .map(|(id, (ts, line))| (ts, id, line))
        .collect();
    applied_24h_rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    failed_rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    let machine_total = machine_in_flight_count(&local_active_paths);
    let updated_hm = format_time(true, "%H:%M");

    let mut out = String::new();
    out.push_str(&format!(
        "在跑 {running_exec}（排队 {queued_count}）· 整机在途 {machine_total} · 待审合 {} · 待回答 {} · 24 小时内已合入 {} · 失败或中断 {} · 更新于 {updated_hm}\n",
        pending_apply_rows.len(),
        pending_answer_rows.len(),
        applied_24h_rows.len(),
        failed_rows.len(),
    ));

    type BoardRow = (i64, String, String);
    let sections: [(&str, &[BoardRow]); 5] = [
        ("在跑", &running_rows),
        ("待审合", &pending_apply_rows),
        ("待回答", &pending_answer_rows),
        ("24 小时内已合入", &applied_24h_rows),
        ("失败或中断", &failed_rows),
    ];
    for (title, rows) in sections {
        if rows.is_empty() {
            continue;
        }
        out.push_str(&format!("\n## {title}\n"));
        for (_, _, line) in rows {
            out.push_str(line);
            out.push('\n');
        }
    }

    let arch = archive::usage();
    let history_total = hist_by_id.len().max(all_known_ids.len());
    out.push('\n');
    let mut footer = format!(
        "归档占用：{}（{} 份） · 历史总计：{history_total} 条：delegate history",
        format_bytes(arch.bytes),
        arch.count
    );
    if arch.fs_critical {
        if let Some(avail) = arch.fs_avail_bytes {
            footer.push_str(&format!(
                " · 警告：归档文件系统剩余仅 {}，将按最久未读淘汰事件日志",
                format_bytes(avail)
            ));
        }
    }
    out.push_str(&footer);
    out.push('\n');
    out
}

fn write_atomic_to(lock_dir: &Path, board_path: &Path, content: &str) -> Res<()> {
    fs::create_dir_all(lock_dir).map_err(|e| e.to_string())?;
    let _guard = crate::locks::acquire(crate::locks::LockKind::Board, &lock_dir.join("board.lock"), true).map_err(|e| e.to_string())?;
    if let Some(parent) = board_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file_name = board_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let tmp = board_path.with_file_name(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        now_ns()
    ));
    if let Err(err) = write(&tmp, content) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = fs::rename(&tmp, board_path) {
        let _ = fs::remove_file(&tmp);
        return Err(err.to_string());
    }
    Ok(())
}

fn refresh_with(
    key: &str,
    source_root: Option<&Path>,
    runs_dir: Option<&Path>,
) -> Res<(PathBuf, String)> {
    let board_path = resolve_board_path(key, source_root);
    let content = render_for(key, runs_dir);
    let lock_dir = history::history_dir_for_key(key);
    write_atomic_to(&lock_dir, &board_path, &content)?;
    Ok((board_path, content))
}

pub fn refresh_for_meta(meta: &Value) {
    let key = history::history_key_for_run(meta);
    let src = history::source_root_for_meta(meta);
    let source_root = (!src.is_empty()).then(|| PathBuf::from(&src));
    let runs_dir = Path::new(s(meta, "dir")).parent().map(Path::to_path_buf);
    if let Err(err) = refresh_with(&key, source_root.as_deref(), runs_dir.as_deref()) {
        eprintln!("delegate: board update failed: {err}");
    }
}

pub fn refresh_for_run(run: &Path) {
    let meta = json(run.join("meta.json"));
    if meta.is_object() {
        refresh_for_meta(&meta);
    } else {
        refresh_current();
    }
}

pub fn refresh_current() {
    let key = history::current_history_key();
    let src = current_source_root();
    if let Err(err) = refresh_with(&key, src.as_deref(), None) {
        eprintln!("delegate: board update failed: {err}");
    }
}

pub fn command(args: &[String]) -> Res<i32> {
    let mut show_path = false;
    for arg in args {
        match arg.as_str() {
            "--path" => show_path = true,
            other => return Err(format!("unrecognized arguments: {other}")),
        }
    }
    let key = history::current_history_key();
    let src = current_source_root();
    let board_path = resolve_board_path(&key, src.as_deref());
    if show_path {
        println!("{}", board_path.display());
        return Ok(0);
    }
    let content = render(&key);
    let lock_dir = history::history_dir_for_key(&key);
    if let Err(err) = write_atomic_to(&lock_dir, &board_path, &content) {
        eprintln!(
            "delegate: board update failed ({}): {err}",
            board_path.display()
        );
    }
    print!("{content}");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_atomic_board_writes_never_produce_partial_file() {
        let dir = std::env::temp_dir().join(format!(
            "delegate-board-atomic-{}-{}",
            std::process::id(),
            now_ns()
        ));
        let lock_dir = dir.join("lock");
        let target = dir.join("BOARD.md");
        fs::create_dir_all(&lock_dir).unwrap();

        let bodies: Vec<String> = (0..8)
            .map(|i| {
                format!(
                    "在跑 {i}（排队 0）· 整机在途 {i} · 待审合 0 · 待回答 0 · 24 小时内已合入 0 · 失败或中断 0 · 更新于 02:00\n{}\nEND-{i}\n",
                    format!("- task-{i} · line payload\n").repeat(64)
                )
            })
            .collect();

        let mut handles = Vec::new();
        for body in bodies.clone() {
            let lock_dir = lock_dir.clone();
            let target = target.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..25 {
                    write_atomic_to(&lock_dir, &target, &body).unwrap();
                    let observed = fs::read_to_string(&target).unwrap();
                    assert!(
                        observed.starts_with("在跑 ") && observed.contains("\nEND-"),
                        "observed torn/partial board file: {observed:?}"
                    );
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let final_content = fs::read_to_string(&target).unwrap();
        assert!(bodies.contains(&final_content));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_conclusion_sentence_skips_bare_heading() {
        assert_eq!(
            first_conclusion_sentence("## 结论\n已修复头像上传 500。\n\n## 证据\na.rs:1"),
            "已修复头像上传 500。"
        );
        assert_eq!(
            first_conclusion_sentence("- 发现 2 处竞态条件"),
            "发现 2 处竞态条件"
        );
    }
}
