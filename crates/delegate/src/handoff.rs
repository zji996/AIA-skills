use crate::changes;
use crate::common::*;
use crate::runs;
use crate::worktree;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

fn state_label(status: &Value) -> String {
    if status["queue"].is_object() {
        return "排队中".into();
    }
    if b(status, "question") {
        return "提问".into();
    }
    match s(status, "state") {
        "running" => "运行中",
        "waiting" => "等待中",
        "starting" => "启动中",
        "delivered" => "已交付",
        "answered" => "已答复",
        "rejected" => "未通过",
        "timeout" => "超时",
        "stalled" => "卡住",
        "stopped" => "已停止",
        "needs-redispatch" => "需要重派",
        "malformed" => "答复畸形",
        "failed" => "失败",
        "crashed" => "进程异常",
        "killed" => "已终止",
        other => other,
    }
    .into()
}

fn md_cell(text: &str) -> String {
    text.replace(['\n', '\r'], " ").replace('|', "\\|")
}

fn count_todo_master(patch: &[u8]) -> usize {
    String::from_utf8_lossy(patch)
        .lines()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++ "))
        .map(|line| line.matches("TODO(master)").count())
        .sum()
}

fn migration_files(paths: &[String]) -> Vec<String> {
    let mut out = paths
        .iter()
        .filter(|p| p.starts_with("migrations/") || p.contains("/migrations/"))
        .filter_map(|p| Path::new(p).file_name().and_then(|n| n.to_str()))
        .map(str::to_string)
        .collect::<Vec<_>>();
    out.sort();
    out.dedup();
    out
}

struct NetInfo {
    files: Vec<String>,
    added: i64,
    deleted: i64,
    has_counts: bool,
    patch: Vec<u8>,
}

fn net_info(run: &Path, meta: &Value, status: &Value) -> NetInfo {
    let rec = json(run.join("changes.json"));
    if rec.is_object() && !s(&rec, "after").is_empty() {
        let mut top = PathBuf::from(s(&rec, "top"));
        if !top.is_dir() {
            top = PathBuf::from(s(&meta["worktree"], "source"));
        }
        let after_tree = s(&rec, "after");
        let base_val = if meta["worktree"].is_object() && s(meta, "mode") == "write" {
            worktree::effective_chain_base(run, meta, &top)
        } else {
            json!({"tree": s(&rec, "base"), "large": {}})
        };
        let base_tree = s(&base_val, "tree");
        let after_val = json!({"tree": after_tree, "large": rec["afterLarge"]});
        let changes_list = if top.is_dir() && !base_tree.is_empty() {
            changes::tree_changes(&top, &base_val, &after_val, meta["worktree"].is_object()).ok()
        } else {
            None
        }
        .or_else(|| rec["changes"].as_array().cloned())
        .unwrap_or_default();

        let files = changes_list
            .iter()
            .map(|c| s(c, "path").to_string())
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>();
        let added = changes_list.iter().map(|c| n(c, "added")).sum();
        let deleted = changes_list.iter().map(|c| n(c, "deleted")).sum();
        let patch = if top.is_dir() && !base_tree.is_empty() {
            git(&top, &["diff", "--no-renames", base_tree, after_tree])
                .ok()
                .unwrap_or_else(|| fs::read(run.join("changes.patch")).unwrap_or_default())
        } else {
            fs::read(run.join("changes.patch")).unwrap_or_default()
        };
        return NetInfo {
            files,
            added,
            deleted,
            has_counts: true,
            patch,
        };
    }
    let files = strings(&status["files"]);
    let patch = fs::read(run.join("changes.patch")).unwrap_or_default();
    NetInfo {
        files,
        added: 0,
        deleted: 0,
        has_counts: false,
        patch,
    }
}

fn next_command(run: &Path, meta: &Value, status: &Value) -> String {
    let id = s(status, "run");
    let st = s(status, "state");
    if runs::active(st) {
        return format!("`delegate wait {id}`");
    }
    if b(status, "question") {
        return format!("`delegate reply {id} '<决定>'`");
    }
    if matches!(st, "delivered" | "answered") {
        if s(meta, "mode") == "write" && meta["worktree"].is_object() {
            return format!("`delegate diff {id} --total && delegate apply {id}`");
        }
        if s(meta, "mode") == "write" {
            return format!("`delegate diff {id}`");
        }
        if !run.join(".delivered").exists() && run.join("result.md").is_file() {
            return format!("`delegate result {id}`");
        }
        return format!("`delegate clean {id}`");
    }
    if matches!(st, "timeout" | "stalled") {
        return format!("`delegate reply {id} '继续'`");
    }
    format!("`delegate status {id}`")
}

fn net_base_short(run: &Path, meta: &Value) -> String {
    let sha = s(meta, "netBase").to_string();
    let sha = if !sha.is_empty() {
        sha
    } else {
        changes::resolve_net_base(run, meta)
            .map(|nb| nb.commit)
            .unwrap_or_default()
    };
    if sha.is_empty() {
        "-".into()
    } else {
        sha[..sha.len().min(7)].to_string()
    }
}

pub fn render() -> String {
    let all = runs::all_runs();
    let mut superseded = BTreeSet::new();
    for run in &all {
        if run.join(".applied-cleaned").exists() {
            continue;
        }
        let meta = json(run.join("meta.json"));
        let parent = s(&meta, "parent");
        if !parent.is_empty() {
            superseded.insert(parent.to_string());
        }
    }

    let mut rows = Vec::new();
    let mut overlaps: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for run in &all {
        if run.join(".applied-cleaned").exists() {
            continue;
        }
        let meta = json(run.join("meta.json"));
        let run_id = {
            let from_meta = s(&meta, "run");
            if !from_meta.is_empty() {
                from_meta.to_string()
            } else {
                run.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }
        };
        if superseded.contains(&run_id) {
            continue;
        }
        let st = runs::state(run);
        if !runs::active(&st) {
            worktree::detect_applied(run, &meta);
        }
        if run.join(".applied").exists() && !run.join(".generate-pending").exists() {
            continue;
        }

        let status = runs::status(run);
        let display_name = {
            let n = s(&meta, "name");
            if n.is_empty() {
                run_id.clone()
            } else {
                n.to_string()
            }
        };
        let label = state_label(&status);
        let base_short = net_base_short(run, &meta);
        let info = net_info(run, &meta, &status);
        let net_change = if info.has_counts {
            if info.files.is_empty() {
                "0".into()
            } else {
                format!("{}f +{}/-{}", info.files.len(), info.added, info.deleted)
            }
        } else if !info.files.is_empty() {
            format!("~{}f", info.files.len())
        } else {
            "-".into()
        };
        let todos = count_todo_master(&info.patch);
        let migrations = migration_files(&info.files);
        let migrations_col = if migrations.is_empty() {
            "-".into()
        } else {
            migrations.join(", ")
        };
        let next = next_command(run, &meta, &status);

        for path in &info.files {
            let entry = overlaps.entry(path.clone()).or_default();
            if !entry.contains(&display_name) {
                entry.push(display_name.clone());
            }
        }

        rows.push(format!(
            "| {} | {} | `{}` | {} | {} | {} | {} |",
            md_cell(&display_name),
            md_cell(&label),
            md_cell(&base_short),
            md_cell(&net_change),
            todos,
            md_cell(&migrations_col),
            md_cell(&next),
        ));
    }

    let head_note = std::env::current_dir()
        .ok()
        .and_then(|cwd| git_top(&cwd))
        .and_then(|top| git_text(&top, &["rev-parse", "--short=7", "HEAD"]).ok())
        .filter(|h| !h.is_empty())
        .map(|h| format!(" · 主干 HEAD `{h}`"))
        .unwrap_or_default();

    let mut out = String::new();
    out.push_str("# Delegate Handoff\n\n");
    out.push_str(&format!(
        "生成时间：{} · 未清理未合入：{} 个{}\n\n",
        iso(),
        rows.len(),
        head_note
    ));

    if rows.is_empty() {
        out.push_str("（当前无未清理、未合入的任务）\n");
    } else {
        out.push_str("| 名字 | 状态 | netBase | 净改动 | TODO(master) | 迁移 | 下一步 |\n");
        out.push_str("| --- | --- | --- | --- | ---: | --- | --- |\n");
        for row in &rows {
            out.push_str(row);
            out.push('\n');
        }
    }

    out.push_str("\n## 重叠修改文件\n\n");
    let shared: Vec<(&String, &Vec<String>)> = overlaps
        .iter()
        .filter(|(_, names)| names.len() >= 2)
        .collect();
    if shared.is_empty() {
        out.push_str("（无）\n");
    } else {
        for (path, names) in shared {
            out.push_str(&format!("- `{path}`: {}\n", names.join(", ")));
        }
    }
    out.push_str("\n详情：`delegate status <run>` · 完整改动：`delegate diff <run> --total`\n");
    out
}

pub fn command(args: &[String]) -> Res<i32> {
    let mut out_path: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" | "-o" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| format!("argument {}: expected one argument", args[i - 1]))?;
                out_path = Some(PathBuf::from(value));
            }
            other => return Err(format!("unrecognized arguments: {other}")),
        }
        i += 1;
    }
    let path = out_path.unwrap_or_else(|| runs_root().join("HANDOFF.md"));
    let content = render();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    write(&path, &content)?;
    print!("{content}");
    println!("\n已写入：{}", path.display());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::{count_todo_master, migration_files};

    #[test]
    fn todo_master_counts_only_added_lines_not_context_or_deleted_or_headers() {
        let patch = concat!(
            "diff --git a/TODO(master).rs b/TODO(master).rs\n",
            "--- a/TODO(master).rs\n",
            "+++ b/TODO(master).rs\n",
            "@@ -1,4 +1,5 @@\n",
            " // TODO(master): unchanged context\n",
            "-// TODO(master): removed\n",
            "+// TODO(master): first added\n",
            "+// normal line\n",
            "+// TODO(master): second and TODO(master): third\n",
        );
        assert_eq!(count_todo_master(patch.as_bytes()), 3);
    }

    #[test]
    fn migration_files_matches_root_and_nested_migrations_dirs() {
        let paths = vec![
            "migrations/0001_init.sql".into(),
            "apps/api/migrations/0149_add_index.sql".into(),
            "src/migrations_helper.rs".into(),
            "src/app.rs".into(),
        ];
        assert_eq!(
            migration_files(&paths),
            vec!["0001_init.sql", "0149_add_index.sql"]
        );
    }
}
