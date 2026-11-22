use crate::common::*;
use crate::runs;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteItem {
    pub text: String,
    pub created_at: String,
    pub done: bool,
    pub done_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainNotes {
    pub root: String,
    pub runs: Vec<String>,
    pub name: String,
    pub latest_run: String,
    pub cleaned: bool,
    pub applied: bool,
    pub items: Vec<NoteItem>,
}

impl ChainNotes {
    pub fn pending_count(&self) -> usize {
        self.items.iter().filter(|item| !item.done).count()
    }

    pub fn items_json(&self) -> Value {
        Value::Array(
            self.items
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    let mut obj = json!({
                        "index": idx + 1,
                        "text": item.text,
                        "createdAt": item.created_at,
                        "done": item.done,
                    });
                    if let Some(done_at) = &item.done_at {
                        obj["doneAt"] = json!(done_at);
                    }
                    obj
                })
                .collect(),
        )
    }

    pub fn to_json(&self) -> Value {
        json!({
            "root": self.root,
            "runs": self.runs,
            "name": self.name,
            "latestRun": self.latest_run,
            "cleaned": self.cleaned,
            "applied": self.applied,
            "pendingCount": self.pending_count(),
            "items": self.items_json(),
        })
    }

    pub fn matches_any(&self, ids: &[String]) -> bool {
        ids.iter()
            .any(|id| id == &self.root || id == &self.latest_run || self.runs.iter().any(|r| r == id))
    }

    fn from_value(v: &Value) -> Option<Self> {
        let root = s(v, "root").to_string();
        let latest_run = s(v, "latestRun").to_string();
        if root.is_empty() && latest_run.is_empty() {
            return None;
        }
        let mut runs = strings(&v["runs"]);
        if !root.is_empty() && !runs.contains(&root) {
            runs.insert(0, root.clone());
        }
        if !latest_run.is_empty() && !runs.contains(&latest_run) {
            runs.push(latest_run.clone());
        }
        let items = v["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let text = s(item, "text").to_string();
                if text.is_empty() {
                    return None;
                }
                Some(NoteItem {
                    text,
                    created_at: s(item, "createdAt").to_string(),
                    done: b(item, "done"),
                    done_at: item["doneAt"]
                        .as_str()
                        .filter(|x| !x.is_empty())
                        .map(str::to_string),
                })
            })
            .collect();
        let root = if root.is_empty() {
            latest_run.clone()
        } else {
            root
        };
        let latest_run = if latest_run.is_empty() {
            root.clone()
        } else {
            latest_run
        };
        let name = {
            let n = s(v, "name");
            if n.is_empty() {
                latest_run.clone()
            } else {
                n.to_string()
            }
        };
        Some(Self {
            root,
            runs,
            name,
            latest_run,
            cleaned: b(v, "cleaned"),
            applied: b(v, "applied"),
            items,
        })
    }

    fn to_storage_json(&self) -> Value {
        json!({
            "root": self.root,
            "runs": self.runs,
            "name": self.name,
            "latestRun": self.latest_run,
            "cleaned": self.cleaned,
            "applied": self.applied,
            "items": self.items.iter().map(|item| {
                let mut obj = json!({
                    "text": item.text,
                    "createdAt": item.created_at,
                    "done": item.done,
                });
                if let Some(done_at) = &item.done_at {
                    obj["doneAt"] = json!(done_at);
                }
                obj
            }).collect::<Vec<_>>(),
        })
    }
}

/// Strip leading `"reply to "` prefixes, returning `(base_name, stripped_count)`.
pub fn strip_reply_prefixes(name: &str) -> (&str, usize) {
    let mut rest = name;
    let mut count = 0;
    while let Some(stripped) = rest.strip_prefix("reply to ") {
        rest = stripped;
        count += 1;
    }
    (rest, count)
}

pub fn run_id(run: &Path, meta: &Value) -> String {
    let id = s(meta, "run");
    if !id.is_empty() {
        id.to_string()
    } else {
        run.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }
}

/// Compute `(chain_name, round_number)` for a run by walking `parent` links.
/// Round 1 is a standalone run; a reply to round 1 is round 2, etc.
pub fn chain_display_info(run: &Path) -> (String, usize) {
    let mut current = run.to_path_buf();
    let mut seen = BTreeSet::new();
    let mut steps = 0usize;
    let mut missing_parent = false;
    let mut top_meta = Value::Null;
    let mut top_id = String::new();

    loop {
        let meta = json(current.join("meta.json"));
        let id = run_id(&current, &meta);
        if id.is_empty() || !seen.insert(id.clone()) {
            break;
        }
        steps += 1;
        top_id = id;
        top_meta = meta;
        let parent = s(&top_meta, "parent");
        if parent.is_empty() {
            break;
        }
        let Some(parent_dir) = current.parent().map(|p| p.join(parent)) else {
            missing_parent = true;
            break;
        };
        if parent_dir.join("meta.json").is_file() {
            current = parent_dir;
        } else {
            missing_parent = true;
            break;
        }
    }

    let raw_name = {
        let n = s(&top_meta, "name");
        if n.is_empty() {
            top_id.as_str()
        } else {
            n
        }
    };
    let (base, prefixes) = strip_reply_prefixes(raw_name);
    let extra_rounds = if missing_parent {
        prefixes.max(1)
    } else {
        prefixes
    };
    let round = steps.max(1) + extra_rounds;
    let chain_name = if base.is_empty() {
        if raw_name.is_empty() {
            top_id
        } else {
            raw_name.to_string()
        }
    } else {
        base.to_string()
    };
    (chain_name, round)
}

/// Return `(root_id, chain_name, member_ids, latest_run_id)` for a run's reply chain.
pub fn chain_info_for(run: &Path, all: &[PathBuf]) -> (String, String, Vec<String>, String) {
    let mut by_id: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut parents: BTreeMap<String, String> = BTreeMap::new();
    let mut order: BTreeMap<String, usize> = BTreeMap::new();

    for (idx, p) in all.iter().enumerate() {
        let meta = json(p.join("meta.json"));
        let id = run_id(p, &meta);
        if id.is_empty() {
            continue;
        }
        order.insert(id.clone(), idx);
        by_id.insert(id.clone(), p.clone());
        let parent = s(&meta, "parent");
        if !parent.is_empty() {
            parents.insert(id.clone(), parent.to_string());
            children
                .entry(parent.to_string())
                .or_default()
                .push(id.clone());
        }
    }

    let meta = json(run.join("meta.json"));
    let start_id = run_id(run, &meta);
    if !start_id.is_empty() && !by_id.contains_key(&start_id) {
        by_id.insert(start_id.clone(), run.to_path_buf());
        let parent = s(&meta, "parent");
        if !parent.is_empty() {
            parents.insert(start_id.clone(), parent.to_string());
            children
                .entry(parent.to_string())
                .or_default()
                .push(start_id.clone());
        }
    }

    // Walk upward to find the earliest ancestor ID (even if its directory was cleaned).
    let mut root_id = start_id.clone();
    let mut seen_up = BTreeSet::new();
    seen_up.insert(root_id.clone());
    loop {
        let parent = parents.get(&root_id).cloned().or_else(|| {
            by_id
                .get(&root_id)
                .map(|p| s(&json(p.join("meta.json")), "parent").to_string())
                .filter(|x| !x.is_empty())
        });
        match parent {
            Some(p) if !p.is_empty() && seen_up.insert(p.clone()) => {
                root_id = p;
            }
            _ => break,
        }
    }

    // BFS across parent <-> child edges to gather the whole connected chain.
    let mut visited = BTreeSet::new();
    let mut queue = VecDeque::new();
    for seed in [&root_id, &start_id] {
        if !seed.is_empty() && visited.insert(seed.clone()) {
            queue.push_back(seed.clone());
        }
    }
    while let Some(cur) = queue.pop_front() {
        if let Some(p) = parents.get(&cur) {
            if !p.is_empty() && visited.insert(p.clone()) {
                queue.push_back(p.clone());
            }
        }
        if let Some(kids) = children.get(&cur) {
            for kid in kids {
                if visited.insert(kid.clone()) {
                    queue.push_back(kid.clone());
                }
            }
        }
    }

    let mut members: Vec<String> = visited.into_iter().collect();
    members.sort_by_key(|id| (order.get(id).copied().unwrap_or(0), id.clone()));

    let latest_run = members
        .iter()
        .rev()
        .find(|id| by_id.contains_key(*id))
        .cloned()
        .unwrap_or_else(|| start_id.clone());

    let (chain_name, _) = by_id
        .get(&latest_run)
        .map(|p| chain_display_info(p))
        .unwrap_or_else(|| chain_display_info(run));

    (root_id, chain_name, members, latest_run)
}

fn notes_path(root: &Path) -> PathBuf {
    root.join("notes.json")
}

fn notes_lock(root: &Path) -> PathBuf {
    root.join(".notes.lock")
}

pub fn load_in(root: &Path) -> Vec<ChainNotes> {
    let val = json(notes_path(root));
    val["chains"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(ChainNotes::from_value)
        .collect()
}

fn save_in(root: &Path, chains: &[ChainNotes]) -> Res<()> {
    fs::create_dir_all(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let val = json!({
        "chains": chains.iter().map(ChainNotes::to_storage_json).collect::<Vec<_>>()
    });
    write_json(notes_path(root), &val)
}

pub fn load() -> Vec<ChainNotes> {
    load_in(&runs_root())
}

/// Record the current repository / runs directory in machine state so cross-repo tools
/// (like the Claude Code Stop hook) can discover live, questioning, unmerged, or noted runs.
pub fn register_repo() {
    let runs_dir = runs_root();
    let repo_dir = runs_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .filter(|_| {
            runs_dir.ends_with(".local/run/delegate") || runs_dir.ends_with(".local/run/pi")
        })
        .map(Path::to_path_buf)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|cwd| git_top(&cwd).or(Some(cwd)))
        })
        .unwrap_or_else(|| runs_dir.clone());
    register_repo_paths(&state_dir(), &repo_dir, &runs_dir);
}

pub fn register_repo_paths(state: &Path, repo_dir: &Path, runs_dir: &Path) {
    let _ = fs::create_dir_all(state);
    let Ok(_guard) = lock(&state.join(".repos.lock"), true, false) else {
        return;
    };
    let path = state.join("repos.json");
    let mut doc = json(&path);
    if !doc.is_object() {
        doc = json!({"repos": {}});
    }
    if !doc["repos"].is_object() {
        doc["repos"] = json!({});
    }
    let runs_key = fs::canonicalize(runs_dir)
        .unwrap_or_else(|_| runs_dir.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let repo_val = fs::canonicalize(repo_dir)
        .unwrap_or_else(|_| repo_dir.to_path_buf())
        .to_string_lossy()
        .into_owned();
    if let Some(map) = doc["repos"].as_object_mut() {
        map.retain(|k, _| k == &runs_key || Path::new(k).is_dir());
        map.insert(
            runs_key.clone(),
            json!({
                "repo": repo_val,
                "runs": runs_key,
                "updatedAt": iso(),
            }),
        );
    }
    let _ = write_json(&path, &doc);
}

pub fn for_run_in(root: &Path, run: &Path, all: &[PathBuf]) -> Option<ChainNotes> {
    let chains = load_in(root);
    if chains.is_empty() {
        return None;
    }
    let (_, _, members, _) = chain_info_for(run, all);
    chains.into_iter().find(|entry| entry.matches_any(&members))
}

fn is_archived_path(run: &Path) -> bool {
    run.starts_with(cache_dir().join("archive"))
        || run
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|n| n == ".applied")
}

fn notes_root_for_run(run: &Path) -> PathBuf {
    if is_archived_path(run) {
        return runs_root();
    }
    run.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(runs_root)
}

pub fn for_run(run: &Path, all: &[PathBuf]) -> Option<ChainNotes> {
    let root = if is_archived_path(run) {
        runs_root()
    } else {
        run.parent()
            .filter(|p| notes_path(p).is_file())
            .map(Path::to_path_buf)
            .unwrap_or_else(runs_root)
    };
    if !notes_path(&root).is_file() {
        return None;
    }
    for_run_in(&root, run, all)
}

pub fn for_run_lazy(run: &Path) -> Option<ChainNotes> {
    let root = if is_archived_path(run) {
        runs_root()
    } else {
        run.parent()
            .filter(|p| notes_path(p).is_file())
            .map(Path::to_path_buf)
            .unwrap_or_else(runs_root)
    };
    if !notes_path(&root).is_file() {
        return None;
    }
    let all = runs::all_runs();
    for_run_in(&root, run, &all)
}

pub fn pending_count_for_run(run: &Path) -> usize {
    for_run_lazy(run).map_or(0, |c| c.pending_count())
}

/// Return chains stored in `notes.json` that still have pending notes and have no
/// remaining run directory in `all`.
pub fn cleaned_pending_chains(all: &[PathBuf]) -> Vec<ChainNotes> {
    let live_ids: BTreeSet<String> = all
        .iter()
        .map(|p| run_id(p, &json(p.join("meta.json"))))
        .filter(|id| !id.is_empty())
        .collect();
    load()
        .into_iter()
        .filter(|c| {
            c.pending_count() > 0
                && !c.runs.iter().any(|id| live_ids.contains(id))
                && !live_ids.contains(&c.root)
                && !live_ids.contains(&c.latest_run)
        })
        .collect()
}

pub fn resolve_cleaned(reference: &str) -> Res<ChainNotes> {
    resolve_cleaned_in(&runs_root(), reference)
}

fn resolve_cleaned_in(root: &Path, reference: &str) -> Res<ChainNotes> {
    let chains = load_in(root);
    if chains.is_empty() {
        return Err(format!("no notes found under {}", root.display()));
    }
    if reference == "last" {
        if let Some(c) = chains.iter().rev().find(|c| c.pending_count() > 0) {
            return Ok(c.clone());
        }
        return chains
            .into_iter()
            .last()
            .ok_or_else(|| format!("no notes found under {}", root.display()));
    }
    if let Some(c) = chains
        .iter()
        .find(|c| c.latest_run == reference || c.root == reference || c.runs.iter().any(|r| r == reference))
    {
        return Ok(c.clone());
    }
    let named: Vec<&ChainNotes> = chains.iter().filter(|c| c.name == reference).collect();
    if named.len() == 1 {
        return Ok(named[0].clone());
    }
    let sub: Vec<&ChainNotes> = chains
        .iter()
        .filter(|c| {
            c.latest_run.contains(reference)
                || c.root.contains(reference)
                || c.name.contains(reference)
                || c.runs.iter().any(|r| r.contains(reference))
        })
        .collect();
    if sub.len() == 1 {
        return Ok(sub[0].clone());
    }
    Err(format!(
        "run '{reference}' matched {} note chains under {}",
        sub.len(),
        root.display()
    ))
}

/// Called by `runs::remove` before deleting a run directory so pending notes survive `clean`.
pub fn on_run_removed(run: &Path, meta: &Value) {
    let Some(root) = run.parent() else {
        return;
    };
    if !notes_path(root).is_file() {
        return;
    }
    let Ok(_guard) = lock(&notes_lock(root), true, false) else {
        return;
    };
    let mut chains = load_in(root);
    if chains.is_empty() {
        return;
    }
    let id = run_id(run, meta);
    let parent = s(meta, "parent").to_string();
    let mut probe_ids = vec![id.clone()];
    if !parent.is_empty() {
        probe_ids.push(parent.clone());
    }
    let Some(pos) = chains.iter().position(|c| c.matches_any(&probe_ids)) else {
        return;
    };
    let entry = &mut chains[pos];
    if !id.is_empty() && !entry.runs.contains(&id) {
        entry.runs.push(id.clone());
    }
    if !parent.is_empty() && !entry.runs.contains(&parent) {
        entry.runs.push(parent);
    }
    if run.join(".applied").exists() || run.join(".applied-cleaned").exists() {
        entry.applied = true;
    }
    if entry.name.is_empty() || entry.name == entry.latest_run {
        let (name, _) = chain_display_info(run);
        if !name.is_empty() {
            entry.name = name;
        }
    }
    if !id.is_empty() {
        entry.latest_run = id.clone();
    }
    // Check if any OTHER run directory in this chain will remain after `run` is deleted.
    let others_remain = entry
        .runs
        .iter()
        .any(|r| r != &id && root.join(r).join("meta.json").is_file());
    if !others_remain {
        if entry.pending_count() == 0 {
            chains.remove(pos);
        } else {
            entry.cleaned = true;
        }
    }
    let _ = save_in(root, &chains);
}

fn sync_chain_entry(
    chains: &mut Vec<ChainNotes>,
    run: &Path,
    all: &[PathBuf],
    create: bool,
) -> Option<usize> {
    let (root_id, chain_name, members, latest_run) = chain_info_for(run, all);
    let applied = members.iter().any(|id| {
        all.iter().any(|p| {
            run_id(p, &json(p.join("meta.json"))) == *id
                && (p.join(".applied").exists() || p.join(".applied-cleaned").exists())
        })
    }) || run.join(".applied").exists()
        || run.join(".applied-cleaned").exists();

    if let Some(pos) = chains.iter().position(|c| c.matches_any(&members)) {
        let entry = &mut chains[pos];
        for id in &members {
            if !entry.runs.contains(id) {
                entry.runs.push(id.clone());
            }
        }
        if !chain_name.is_empty() {
            entry.name = chain_name;
        }
        if !latest_run.is_empty() {
            entry.latest_run = latest_run;
        }
        entry.cleaned = false;
        entry.applied |= applied;
        return Some(pos);
    }
    if !create {
        return None;
    }
    chains.push(ChainNotes {
        root: root_id,
        runs: members,
        name: chain_name,
        latest_run,
        cleaned: false,
        applied,
        items: Vec::new(),
    });
    Some(chains.len() - 1)
}

fn format_items_lines(entry: &ChainNotes) -> Vec<String> {
    entry
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let mark = if item.done { "x" } else { " " };
            let done_suffix = item
                .done_at
                .as_deref()
                .map(|t| format!("，完成于 {t}"))
                .unwrap_or_default();
            format!(
                "  {}. [{}] {} ({}{})",
                i + 1,
                mark,
                item.text,
                item.created_at,
                done_suffix
            )
        })
        .collect()
}

fn print_chain(entry: &ChainNotes) {
    let pending = entry.pending_count();
    let total = entry.items.len();
    let mut tags = Vec::new();
    if entry.applied {
        tags.push("已合入");
    }
    if entry.cleaned {
        tags.push("已清理");
    }
    let tag_str = if tags.is_empty() {
        String::new()
    } else {
        format!(" · {}", tags.join("·"))
    };
    println!(
        "{}（{}）{} · 未完成 {} / 共 {} 条",
        entry.name, entry.latest_run, tag_str, pending, total
    );
    for line in format_items_lines(entry) {
        println!("{line}");
    }
    if pending > 0 {
        println!(
            "标完成：delegate note {} --done <序号> 或 --done --all",
            entry.latest_run
        );
    }
}

enum Target {
    Live(PathBuf),
    Cleaned(String),
}

fn resolve_target(reference: &str) -> Res<Target> {
    let resolved = runs::resolve_head(reference).ok();
    if let Some(run) = resolved.as_ref().filter(|p| !is_archived_path(p)) {
        return Ok(Target::Live(run.clone()));
    }
    if let Ok(cleaned) = resolve_cleaned(reference) {
        return Ok(Target::Cleaned(cleaned.latest_run));
    }
    if let Some(run) = resolved {
        return Ok(Target::Live(run));
    }
    let cleaned = resolve_cleaned(reference)?;
    Ok(Target::Cleaned(cleaned.latest_run))
}

pub fn command(args: &[String]) -> Res<i32> {
    let mut done_flag = false;
    let mut all_flag = false;
    let mut json_flag = false;
    let mut positional: Vec<String> = Vec::new();
    let mut after_dd = false;

    for arg in args {
        if after_dd {
            positional.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => after_dd = true,
            "--done" => done_flag = true,
            "--all" => all_flag = true,
            "--json" => json_flag = true,
            other if other.starts_with('-') => {
                return Err(format!("unrecognized arguments: {other}"));
            }
            _ => positional.push(arg.clone()),
        }
    }

    if done_flag {
        if positional.is_empty() {
            return Err("note --done requires <run>".into());
        }
        let reference = &positional[0];
        let index_args = &positional[1..];
        if all_flag && !index_args.is_empty() {
            return Err("cannot combine --all with explicit note indices".into());
        }
        let target = resolve_target(reference)?;
        let root = match &target {
            Target::Live(run) => notes_root_for_run(run),
            Target::Cleaned(_) => runs_root(),
        };
        fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
        let _guard = lock(&notes_lock(&root), true, false).map_err(|e| e.to_string())?;
        let mut chains = load_in(&root);
        let all = runs::all_runs();
        let pos = match &target {
            Target::Live(run) => sync_chain_entry(&mut chains, run, &all, false),
            Target::Cleaned(id) => chains.iter().position(|c| c.latest_run == *id),
        };
        let Some(pos) = pos else {
            if json_flag {
                println!("{}", json!({"ok": true, "pendingCount": 0, "items": []}));
            } else {
                println!("{reference}：无待办");
            }
            return Ok(0);
        };

        let entry = &mut chains[pos];
        let pending_indices: Vec<usize> = entry
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| !item.done)
            .map(|(idx, _)| idx)
            .collect();

        if pending_indices.is_empty() {
            let snapshot = entry.clone();
            let cleaned_empty = entry.cleaned;
            if cleaned_empty {
                chains.remove(pos);
                save_in(&root, &chains)?;
            }
            if json_flag {
                println!("{}", snapshot.to_json());
            } else {
                println!("{}：已无未完成待办", snapshot.name);
            }
            return Ok(0);
        }

        let now = iso();
        let to_mark: Vec<usize> = if all_flag {
            pending_indices.clone()
        } else if !index_args.is_empty() {
            let total = entry.items.len();
            let mut chosen = Vec::new();
            for raw in index_args {
                let idx = raw.parse::<usize>().map_err(|_| {
                    format!(
                        "invalid note index '{raw}': expected integer 1..={total} or --all"
                    )
                })?;
                if idx == 0 || idx > total {
                    return Err(format!(
                        "note index {idx} out of range (valid: 1..={total})"
                    ));
                }
                chosen.push(idx - 1);
            }
            chosen
        } else if pending_indices.len() == 1 {
            pending_indices.clone()
        } else {
            let listing = format_items_lines(entry).join("\n");
            return Err(format!(
                "{} 有 {} 条未完成待办，请指定序号（如 delegate note {} --done 1）或 --all：\n{listing}",
                entry.name,
                pending_indices.len(),
                entry.latest_run
            ));
        };

        for idx in to_mark {
            if !entry.items[idx].done {
                entry.items[idx].done = true;
                entry.items[idx].done_at = Some(now.clone());
            }
        }

        let snapshot = entry.clone();
        let remaining = snapshot.pending_count();
        let has_live_dir = snapshot
            .runs
            .iter()
            .any(|id| root.join(id).join("meta.json").is_file());
        if !has_live_dir && remaining == 0 {
            chains.remove(pos);
        }
        save_in(&root, &chains)?;

        if json_flag {
            println!("{}", snapshot.to_json());
        } else {
            println!("已完成 {} 待办（剩余 {} 条未完成）", snapshot.name, remaining);
            if remaining > 0 {
                for (i, item) in snapshot.items.iter().enumerate() {
                    if !item.done {
                        println!("  {}. [ ] {} ({})", i + 1, item.text, item.created_at);
                    }
                }
            }
        }
        return Ok(0);
    }

    // No positional args: list all chains with pending notes (or all with --all).
    if positional.is_empty() {
        let chains = load();
        let filtered: Vec<ChainNotes> = chains
            .into_iter()
            .filter(|c| all_flag || c.pending_count() > 0)
            .collect();
        if json_flag {
            for c in &filtered {
                println!("{}", c.to_json());
            }
        } else if filtered.is_empty() {
            println!("（无未完成待办）");
        } else {
            for (idx, c) in filtered.iter().enumerate() {
                if idx > 0 {
                    println!();
                }
                print_chain(c);
            }
        }
        return Ok(0);
    }

    if all_flag {
        return Err("--all is only valid with `delegate note` (list) or `delegate note <run> --done --all`".into());
    }

    let reference = &positional[0];
    let text_parts = &positional[1..];

    // Single positional arg: list notes for this run's chain.
    if text_parts.is_empty() {
        let target = resolve_target(reference)?;
        let all = runs::all_runs();
        let entry = match &target {
            Target::Live(run) => for_run(run, &all),
            Target::Cleaned(id) => load().into_iter().find(|c| c.latest_run == *id),
        };
        match entry {
            Some(c) => {
                if json_flag {
                    println!("{}", c.to_json());
                } else {
                    print_chain(&c);
                }
            }
            None => {
                let (name, latest_run) = match &target {
                    Target::Live(run) => {
                        let (_, name, _, latest) = chain_info_for(run, &all);
                        (name, latest)
                    }
                    Target::Cleaned(id) => (id.clone(), id.clone()),
                };
                if json_flag {
                    println!(
                        "{}",
                        json!({"name": name, "latestRun": latest_run, "pendingCount": 0, "items": []})
                    );
                } else {
                    println!("{name}（{latest_run}）：无待办");
                }
            }
        }
        return Ok(0);
    }

    // Append a new note to the run's chain.
    let text = text_parts.join(" ").trim().to_string();
    if text.is_empty() {
        return Err("note text cannot be empty".into());
    }
    let target = resolve_target(reference)?;
    let root = match &target {
        Target::Live(run) => notes_root_for_run(run),
        Target::Cleaned(_) => runs_root(),
    };
    fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    let _guard = lock(&notes_lock(&root), true, false).map_err(|e| e.to_string())?;
    let mut chains = load_in(&root);
    let all = runs::all_runs();
    let pos = match &target {
        Target::Live(run) => sync_chain_entry(&mut chains, run, &all, true).unwrap(),
        Target::Cleaned(id) => chains
            .iter()
            .position(|c| c.latest_run == *id)
            .ok_or_else(|| format!("run '{reference}' not found"))?,
    };
    let created_at = iso();
    let entry = &mut chains[pos];
    entry.items.push(NoteItem {
        text: text.clone(),
        created_at,
        done: false,
        done_at: None,
    });
    let added_index = entry.items.len();
    let snapshot = entry.clone();
    save_in(&root, &chains)?;
    register_repo();

    if json_flag {
        let mut out = snapshot.to_json();
        out["addedIndex"] = json!(added_index);
        println!("{out}");
    } else {
        let pending = snapshot.pending_count();
        let suffix = if pending > 1 {
            format!("（当前共 {pending} 条未完成）")
        } else {
            String::new()
        };
        println!(
            "已记录待办 #{added_index}（{}）：{text}{suffix}",
            snapshot.name
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "delegate-notes-{label}-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_run(root: &Path, id: &str, name: &str, parent: Option<&str>, ns: u64) -> PathBuf {
        let dir = root.join(id);
        fs::create_dir_all(&dir).unwrap();
        let mut meta = json!({
            "run": id,
            "name": name,
            "startedNs": ns,
            "mode": "write",
        });
        if let Some(p) = parent {
            meta["parent"] = json!(p);
        }
        write_json(dir.join("meta.json"), &meta).unwrap();
        dir
    }

    #[test]
    fn strip_reply_prefixes_counts_nested_replies() {
        assert_eq!(strip_reply_prefixes("合入员"), ("合入员", 0));
        assert_eq!(strip_reply_prefixes("reply to 合入员"), ("合入员", 1));
        assert_eq!(
            strip_reply_prefixes("reply to reply to 合入员"),
            ("合入员", 2)
        );
    }

    #[test]
    fn chain_display_info_tracks_rounds_and_missing_ancestors() {
        let root = temp_dir("display");
        let r1 = make_run(&root, "20261122-100000-merger", "合入员", None, 1);
        let r2 = make_run(
            &root,
            "20261122-100100-reply-1",
            "reply to 合入员",
            Some("20261122-100000-merger"),
            2,
        );
        let r3 = make_run(
            &root,
            "20261122-100200-reply-2",
            "reply to reply to 合入员",
            Some("20261122-100100-reply-1"),
            3,
        );
        assert_eq!(chain_display_info(&r1), ("合入员".into(), 1));
        assert_eq!(chain_display_info(&r2), ("合入员".into(), 2));
        assert_eq!(chain_display_info(&r3), ("合入员".into(), 3));

        // Remove r1 and r2: r3 still recovers chain name and round count from prefixes.
        fs::remove_dir_all(&r1).unwrap();
        fs::remove_dir_all(&r2).unwrap();
        assert_eq!(chain_display_info(&r3), ("合入员".into(), 3));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn notes_follow_reply_chain_and_survive_clean() {
        let root = temp_dir("chain");
        let r1 = make_run(&root, "20261122-100000-a", "服务升级", None, 1);
        let r2 = make_run(
            &root,
            "20261122-100100-b",
            "reply to 服务升级",
            Some("20261122-100000-a"),
            2,
        );
        let all = vec![r1.clone(), r2.clone()];

        // Attach a note on r1.
        let mut chains = Vec::new();
        let pos = sync_chain_entry(&mut chains, &r1, &all, true).unwrap();
        chains[pos].items.push(NoteItem {
            text: "换线上服务".into(),
            created_at: "2026-11-22T02:00:00Z".into(),
            done: false,
            done_at: None,
        });
        save_in(&root, &chains).unwrap();

        // Looking up via r2 returns the same chain's note.
        let found = for_run_in(&root, &r2, &all).expect("r2 should see r1 note");
        assert_eq!(found.name, "服务升级");
        assert_eq!(found.pending_count(), 1);
        assert_eq!(found.items[0].text, "换线上服务");

        // Clean r1 while r2 still exists: stays uncleaned.
        on_run_removed(&r1, &json(r1.join("meta.json")));
        fs::remove_dir_all(&r1).unwrap();
        let after_r1 = load_in(&root);
        assert_eq!(after_r1.len(), 1);
        assert!(!after_r1[0].cleaned);

        // Clean r2 (last run in chain): pending note is preserved and marked cleaned.
        write(r2.join(".applied"), "{}\n").unwrap();
        on_run_removed(&r2, &json(r2.join("meta.json")));
        fs::remove_dir_all(&r2).unwrap();
        let after_r2 = load_in(&root);
        assert_eq!(after_r2.len(), 1);
        assert!(after_r2[0].cleaned);
        assert!(after_r2[0].applied);
        assert_eq!(after_r2[0].pending_count(), 1);

        // Resolving cleaned run by name or either run id works.
        assert_eq!(
            resolve_cleaned_in(&root, "服务升级").unwrap().latest_run,
            "20261122-100100-b"
        );
        assert_eq!(
            resolve_cleaned_in(&root, "20261122-100000-a").unwrap().name,
            "服务升级"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cleaned_chain_with_all_done_notes_is_pruned_on_remove() {
        let root = temp_dir("prune");
        let r1 = make_run(&root, "20261122-100000-done", "已完成", None, 1);
        let all = vec![r1.clone()];
        let mut chains = Vec::new();
        let pos = sync_chain_entry(&mut chains, &r1, &all, true).unwrap();
        chains[pos].items.push(NoteItem {
            text: "真机验证".into(),
            created_at: "2026-11-22T02:00:00Z".into(),
            done: true,
            done_at: Some("2026-11-22T02:05:00Z".into()),
        });
        save_in(&root, &chains).unwrap();

        on_run_removed(&r1, &json(r1.join("meta.json")));
        fs::remove_dir_all(&r1).unwrap();
        assert!(load_in(&root).is_empty());

        let _ = fs::remove_dir_all(&root);
    }
}
