use crate::data::extract_result_excerpt;
use crate::model::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPane {
    List,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewTab {
    Runs,
    Events,
    Merges,
    Candidates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteAction {
    Apply,
    Clean,
    Stop,
    Reply,
}

impl WriteAction {
    pub fn label(self) -> &'static str {
        match self {
            WriteAction::Apply => "合入 (apply)",
            WriteAction::Clean => "清理 (clean)",
            WriteAction::Stop => "停止 (stop)",
            WriteAction::Reply => "续做/答复 (reply)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    None,
    FullResult {
        run_id: String,
        title: String,
        content: String,
        scroll: usize,
    },
    DiffStat {
        run_id: String,
        title: String,
        content: String,
        scroll: usize,
    },
    CommandModal {
        action_label: String,
        command: String,
        note: String,
    },
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CollapseKind {
    StalePending,
    MergedUncleaned,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListRow {
    RepoHeader {
        repo_idx: usize,
    },
    GroupHeader {
        repo_idx: usize,
        group: GroupKind,
        count: usize,
    },
    ToggleFold {
        repo_idx: usize,
        kind: CollapseKind,
        count: usize,
        newest_ago_seconds: Option<u64>,
        expanded: bool,
        standalone: bool,
    },
    Run {
        repo_idx: usize,
        run_idx: usize,
    },
    DagQueueHeader {
        count: usize,
    },
    DagQueueEntry {
        idx: usize,
    },
}

impl ListRow {
    pub fn is_selectable(&self) -> bool {
        matches!(
            self,
            ListRow::Run { .. } | ListRow::ToggleFold { .. } | ListRow::DagQueueEntry { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectionKey {
    Run(PathBuf, String),
    Fold(PathBuf, CollapseKind),
    DagQueue(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppAction {
    None,
    Redraw,
    Refresh,
    LoadSelectedResult,
    OpenFullResult,
    OpenDiffStat,
    CopyClipboard(String),
    OpenPager(String),
    Quit,
    QuitAndPrint(String),
}

pub struct App {
    pub snapshot: AppSnapshot,
    pub last_fingerprint: u64,
    pub rows: Vec<ListRow>,
    pub selected_row: Option<usize>,
    pub list_scroll: usize,
    pub detail_scroll: usize,
    pub focus: FocusPane,
    pub tab: ViewTab,
    pub filtered_events: Vec<usize>,
    pub selected_event: Option<usize>,
    pub events_scroll: usize,
    pub events_follow: bool,
    pub filtered_merges: Vec<usize>,
    pub selected_merge: Option<usize>,
    pub merges_scroll: usize,
    pub filtered_candidates: Vec<usize>,
    pub selected_candidate: Option<usize>,
    pub candidates_scroll: usize,
    pub filter: String,
    pub filter_mode: bool,
    pub expanded_folds: HashSet<(PathBuf, CollapseKind)>,
    pub overlay: Overlay,
    pub result_cache: HashMap<String, Result<String, String>>,
    pub diff_cache: HashMap<String, Result<String, String>>,
    pub toast: Option<(String, Instant)>,
    pub refresh_interval_secs: u64,
    pub delegate_bin: PathBuf,
    pub cwd: PathBuf,
    pub loading: bool,
    pub dirty: bool,
}

impl App {
    pub fn new(
        snapshot: AppSnapshot,
        delegate_bin: PathBuf,
        cwd: PathBuf,
        refresh_interval_secs: u64,
    ) -> Self {
        let fp = snapshot.data_fingerprint();
        let mut app = Self {
            snapshot,
            last_fingerprint: fp,
            rows: Vec::new(),
            selected_row: None,
            list_scroll: 0,
            detail_scroll: 0,
            focus: FocusPane::List,
            tab: ViewTab::Runs,
            filtered_events: Vec::new(),
            selected_event: None,
            events_scroll: 0,
            events_follow: true,
            filtered_merges: Vec::new(),
            selected_merge: None,
            merges_scroll: 0,
            filtered_candidates: Vec::new(),
            selected_candidate: None,
            candidates_scroll: 0,
            filter: String::new(),
            filter_mode: false,
            expanded_folds: HashSet::new(),
            overlay: Overlay::None,
            result_cache: HashMap::new(),
            diff_cache: HashMap::new(),
            toast: None,
            refresh_interval_secs,
            delegate_bin,
            cwd,
            loading: false,
            dirty: true,
        };
        app.rebuild_rows(None);
        app
    }

    /// Apply a newly collected snapshot; returns true if anything changed and a redraw is needed.
    pub fn apply_snapshot(&mut self, incoming: AppSnapshot, force_update_time: bool) -> bool {
        let was_loading = self.loading;
        self.loading = false;
        let new_fp = incoming.data_fingerprint();
        if new_fp == self.last_fingerprint && !force_update_time && !was_loading {
            return false;
        }
        let prev_key = self.current_selection_key();
        self.snapshot = incoming;
        self.last_fingerprint = new_fp;
        self.rebuild_rows_inner(prev_key);
        self.dirty = true;
        true
    }

    fn current_selection_key(&self) -> Option<SelectionKey> {
        let row_idx = self.selected_row?;
        match self.rows.get(row_idx)? {
            ListRow::Run { repo_idx, run_idx } => {
                let r = self.snapshot.repos.get(*repo_idx)?.runs.get(*run_idx)?;
                Some(SelectionKey::Run(r.repo_path.clone(), r.run.clone()))
            }
            ListRow::ToggleFold { repo_idx, kind, .. } => {
                let repo = self.snapshot.repos.get(*repo_idx)?;
                Some(SelectionKey::Fold(repo.path.clone(), *kind))
            }
            ListRow::DagQueueEntry { idx } => {
                let q = self.snapshot.dag_queue.get(*idx)?;
                Some(SelectionKey::DagQueue(q.name.clone()))
            }
            _ => None,
        }
    }

    pub fn rebuild_rows(&mut self, preserve: Option<(PathBuf, String)>) {
        self.rebuild_rows_inner(preserve.map(|(p, id)| SelectionKey::Run(p, id)));
    }

    fn rebuild_rows_inner(&mut self, preserve: Option<SelectionKey>) {
        let query = self.filter.trim().to_lowercase();
        let filtering = !query.is_empty();
        let mut rows = Vec::new();

        for (repo_idx, repo) in self.snapshot.repos.iter().enumerate() {
            let mut matching_by_group: Vec<(GroupKind, Vec<usize>)> = Vec::new();
            for group in GroupKind::ALL {
                let idxs: Vec<usize> = repo
                    .runs
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.group == group && r.matches_filter(&query))
                    .map(|(i, _)| i)
                    .collect();
                if !idxs.is_empty() {
                    matching_by_group.push((group, idxs));
                }
            }
            if matching_by_group.is_empty() {
                continue;
            }

            rows.push(ListRow::RepoHeader { repo_idx });

            for (group, idxs) in matching_by_group {
                if filtering {
                    rows.push(ListRow::GroupHeader {
                        repo_idx,
                        group,
                        count: idxs.len(),
                    });
                    for run_idx in idxs {
                        rows.push(ListRow::Run { repo_idx, run_idx });
                    }
                    continue;
                }

                match group {
                    GroupKind::Pending => {
                        let mut recent = Vec::new();
                        let mut stale = Vec::new();
                        for run_idx in idxs {
                            if repo.runs[run_idx].is_stale_pending() {
                                stale.push(run_idx);
                            } else {
                                recent.push(run_idx);
                            }
                        }
                        if !recent.is_empty() {
                            rows.push(ListRow::GroupHeader {
                                repo_idx,
                                group: GroupKind::Pending,
                                count: recent.len() + stale.len(),
                            });
                            for run_idx in recent {
                                rows.push(ListRow::Run { repo_idx, run_idx });
                            }
                            if !stale.is_empty() {
                                let expanded = self
                                    .expanded_folds
                                    .contains(&(repo.path.clone(), CollapseKind::StalePending));
                                let newest_ago = stale
                                    .iter()
                                    .map(|&i| repo.runs[i].last_activity_seconds())
                                    .min();
                                rows.push(ListRow::ToggleFold {
                                    repo_idx,
                                    kind: CollapseKind::StalePending,
                                    count: stale.len(),
                                    newest_ago_seconds: newest_ago,
                                    expanded,
                                    standalone: false,
                                });
                                if expanded {
                                    for run_idx in stale {
                                        rows.push(ListRow::Run { repo_idx, run_idx });
                                    }
                                }
                            }
                        } else if !stale.is_empty() {
                            let expanded = self
                                .expanded_folds
                                .contains(&(repo.path.clone(), CollapseKind::StalePending));
                            let newest_ago = stale
                                .iter()
                                .map(|&i| repo.runs[i].last_activity_seconds())
                                .min();
                            rows.push(ListRow::ToggleFold {
                                repo_idx,
                                kind: CollapseKind::StalePending,
                                count: stale.len(),
                                newest_ago_seconds: newest_ago,
                                expanded,
                                standalone: true,
                            });
                            if expanded {
                                for run_idx in stale {
                                    rows.push(ListRow::Run { repo_idx, run_idx });
                                }
                            }
                        }
                    }
                    GroupKind::Running | GroupKind::Queued => {
                        rows.push(ListRow::GroupHeader {
                            repo_idx,
                            group,
                            count: idxs.len(),
                        });
                        for run_idx in idxs {
                            rows.push(ListRow::Run { repo_idx, run_idx });
                        }
                    }
                    GroupKind::MergedUncleaned | GroupKind::Stopped => {
                        let kind = if group == GroupKind::MergedUncleaned {
                            CollapseKind::MergedUncleaned
                        } else {
                            CollapseKind::Stopped
                        };
                        let expanded = self.expanded_folds.contains(&(repo.path.clone(), kind));
                        let newest_ago = idxs
                            .iter()
                            .map(|&i| repo.runs[i].last_activity_seconds())
                            .min();
                        rows.push(ListRow::ToggleFold {
                            repo_idx,
                            kind,
                            count: idxs.len(),
                            newest_ago_seconds: newest_ago,
                            expanded,
                            standalone: true,
                        });
                        if expanded {
                            for run_idx in idxs {
                                rows.push(ListRow::Run { repo_idx, run_idx });
                            }
                        }
                    }
                }
            }
        }
        // Append deterministic DAG queue items if any match filter.
        let matching_queue: Vec<usize> = self
            .snapshot
            .dag_queue
            .iter()
            .enumerate()
            .filter(|(_, q)| q.status != "cancelled" && q.matches_filter(&query))
            .map(|(i, _)| i)
            .collect();
        if !matching_queue.is_empty() {
            rows.push(ListRow::DagQueueHeader {
                count: matching_queue.len(),
            });
            for idx in matching_queue {
                rows.push(ListRow::DagQueueEntry { idx });
            }
        }
        self.rows = rows;

        // Rebuild filtered events list and keep follow cursor synced.
        let prev_event_seq = self.selected_event_item().map(|e| e.seq);
        self.filtered_events = self
            .snapshot
            .events
            .iter()
            .enumerate()
            .filter(|(_, ev)| ev.matches_filter(&query))
            .map(|(i, _)| i)
            .collect();
        if self.filtered_events.is_empty() {
            self.selected_event = None;
            self.events_scroll = 0;
        } else if self.events_follow {
            self.selected_event = Some(self.filtered_events.len() - 1);
        } else if let Some(seq) = prev_event_seq {
            self.selected_event = self
                .filtered_events
                .iter()
                .position(|&idx| self.snapshot.events[idx].seq == seq)
                .or(Some(self.filtered_events.len() - 1));
        } else {
            self.selected_event = Some(self.filtered_events.len() - 1);
        }

        // Rebuild filtered merges list preserving selection by merge_run id.
        let prev_merge_id = self.selected_merge_item().map(|m| m.merge_run.clone());
        self.filtered_merges = self
            .snapshot
            .merges
            .iter()
            .enumerate()
            .filter(|(_, m)| m.matches_filter(&query))
            .map(|(i, _)| i)
            .collect();
        if self.filtered_merges.is_empty() {
            self.selected_merge = None;
            self.merges_scroll = 0;
        } else if let Some(ref mid) = prev_merge_id {
            self.selected_merge = self
                .filtered_merges
                .iter()
                .position(|&idx| self.snapshot.merges[idx].merge_run == *mid)
                .or(Some(0));
        } else {
            self.selected_merge = Some(0);
        }

        // Rebuild filtered radar candidates list preserving selection by candidate id.
        let prev_cand_id = self
            .selected_candidate_item()
            .map(|c| (c.repo_path.clone(), c.id.clone()));
        self.filtered_candidates = self
            .snapshot
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| c.matches_filter(&query))
            .map(|(i, _)| i)
            .collect();
        if self.filtered_candidates.is_empty() {
            self.selected_candidate = None;
            self.candidates_scroll = 0;
        } else if let Some((ref repo, ref cid)) = prev_cand_id {
            self.selected_candidate = self
                .filtered_candidates
                .iter()
                .position(|&idx| {
                    let c = &self.snapshot.candidates[idx];
                    c.repo_path == *repo && c.id == *cid
                })
                .or(Some(0));
        } else {
            self.selected_candidate = Some(0);
        }

        // Restore prior selection if present; else prefer first visible Run, falling back to first selectable.
        let mut next_sel = None;
        if let Some(key) = preserve {
            for (row_idx, row) in self.rows.iter().enumerate() {
                match (&key, row) {
                    (
                        SelectionKey::Run(prev_repo, prev_run),
                        ListRow::Run { repo_idx, run_idx },
                    ) => {
                        let r = &self.snapshot.repos[*repo_idx].runs[*run_idx];
                        if &r.repo_path == prev_repo && &r.run == prev_run {
                            next_sel = Some(row_idx);
                            break;
                        }
                    }
                    (
                        SelectionKey::Fold(prev_repo, prev_kind),
                        ListRow::ToggleFold { repo_idx, kind, .. },
                    ) if &self.snapshot.repos[*repo_idx].path == prev_repo && kind == prev_kind => {
                        next_sel = Some(row_idx);
                        break;
                    }
                    (SelectionKey::DagQueue(prev_name), ListRow::DagQueueEntry { idx })
                        if &self.snapshot.dag_queue[*idx].name == prev_name =>
                    {
                        next_sel = Some(row_idx);
                        break;
                    }
                    _ => {}
                }
            }
        }
        if next_sel.is_none() {
            next_sel = self
                .rows
                .iter()
                .position(|r| matches!(r, ListRow::Run { .. }))
                .or_else(|| self.rows.iter().position(ListRow::is_selectable));
        }
        if next_sel != self.selected_row {
            self.selected_row = next_sel;
            self.detail_scroll = 0;
        }
        if self.selected_row.is_none() {
            self.list_scroll = 0;
        }
    }

    pub fn toggle_selected_fold(&mut self) -> bool {
        let Some(row_idx) = self.selected_row else {
            return false;
        };
        let Some(&ListRow::ToggleFold { repo_idx, kind, .. }) = self.rows.get(row_idx) else {
            return false;
        };
        let Some(repo) = self.snapshot.repos.get(repo_idx) else {
            return false;
        };
        let key = (repo.path.clone(), kind);
        if !self.expanded_folds.remove(&key) {
            self.expanded_folds.insert(key);
        }
        self.rebuild_rows_inner(Some(SelectionKey::Fold(repo.path.clone(), kind)));
        self.dirty = true;
        true
    }

    pub fn selected_run(&self) -> Option<&RunItem> {
        let row_idx = self.selected_row?;
        match self.rows.get(row_idx)? {
            ListRow::Run { repo_idx, run_idx } => {
                self.snapshot.repos.get(*repo_idx)?.runs.get(*run_idx)
            }
            _ => None,
        }
    }

    pub fn selected_dag_queue(&self) -> Option<&DagQueueItem> {
        let row_idx = self.selected_row?;
        match self.rows.get(row_idx)? {
            ListRow::DagQueueEntry { idx } => self.snapshot.dag_queue.get(*idx),
            _ => None,
        }
    }

    pub fn selected_event_item(&self) -> Option<&EventItem> {
        let pos = self.selected_event?;
        let &ev_idx = self.filtered_events.get(pos)?;
        self.snapshot.events.get(ev_idx)
    }

    pub fn selected_merge_item(&self) -> Option<&MergeRunItem> {
        let pos = self.selected_merge?;
        let &idx = self.filtered_merges.get(pos)?;
        self.snapshot.merges.get(idx)
    }

    pub fn selected_candidate_item(&self) -> Option<&RadarCandidateItem> {
        let pos = self.selected_candidate?;
        let &idx = self.filtered_candidates.get(pos)?;
        self.snapshot.candidates.get(idx)
    }

    pub fn move_merge_selection(&mut self, delta: isize) -> bool {
        if self.filtered_merges.is_empty() {
            self.selected_merge = None;
            return false;
        }
        let len = self.filtered_merges.len();
        let cur = self.selected_merge.unwrap_or(0);
        let next = if delta >= 0 {
            (cur + delta as usize).min(len - 1)
        } else {
            cur.saturating_sub((-delta) as usize)
        };
        if Some(next) != self.selected_merge {
            self.selected_merge = Some(next);
            self.detail_scroll = 0;
            true
        } else {
            false
        }
    }

    pub fn move_candidate_selection(&mut self, delta: isize) -> bool {
        if self.filtered_candidates.is_empty() {
            self.selected_candidate = None;
            return false;
        }
        let len = self.filtered_candidates.len();
        let cur = self.selected_candidate.unwrap_or(0);
        let next = if delta >= 0 {
            (cur + delta as usize).min(len - 1)
        } else {
            cur.saturating_sub((-delta) as usize)
        };
        if Some(next) != self.selected_candidate {
            self.selected_candidate = Some(next);
            self.detail_scroll = 0;
            true
        } else {
            false
        }
    }

    pub fn move_event_selection(&mut self, delta: isize) -> bool {
        if self.filtered_events.is_empty() {
            self.selected_event = None;
            return false;
        }
        let len = self.filtered_events.len();
        let cur = self.selected_event.unwrap_or(len - 1);
        let next = if delta >= 0 {
            (cur + delta as usize).min(len - 1)
        } else {
            cur.saturating_sub((-delta) as usize)
        };
        self.events_follow = next + 1 >= len;
        if Some(next) != self.selected_event {
            self.selected_event = Some(next);
            self.detail_scroll = 0;
            true
        } else {
            false
        }
    }

    pub fn selected_result_excerpt(&self) -> Option<(String, bool, usize)> {
        let run = self.selected_run()?;
        let cached = self.result_cache.get(&run.cache_key())?;
        match cached {
            Ok(text) => {
                let total_chars = text.chars().count();
                let (excerpt, truncated) = extract_result_excerpt(text, 4, 14);
                Some((excerpt, truncated, total_chars))
            }
            Err(_) => None,
        }
    }

    pub fn selected_result_error(&self) -> Option<&str> {
        let run = self.selected_run()?;
        match self.result_cache.get(&run.cache_key())? {
            Err(e) => Some(e.as_str()),
            Ok(_) => None,
        }
    }

    pub fn needs_selected_result(&self) -> bool {
        let Some(run) = self.selected_run() else {
            return false;
        };
        run.has_result && !self.result_cache.contains_key(&run.cache_key())
    }

    pub fn set_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
        self.dirty = true;
    }

    pub fn expire_toast(&mut self) -> bool {
        if let Some((_, at)) = &self.toast {
            if at.elapsed() >= Duration::from_secs(4) {
                self.toast = None;
                self.dirty = true;
                return true;
            }
        }
        false
    }

    pub fn open_command_modal(&mut self, action: WriteAction) {
        let Some(run) = self.selected_run().cloned() else {
            self.set_toast("当前没有选中的任务");
            return;
        };
        let (command, note) = build_write_command(&self.delegate_bin, &self.cwd, &run, action);
        self.overlay = Overlay::CommandModal {
            action_label: action.label().to_string(),
            command,
            note,
        };
        self.dirty = true;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> AppAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return AppAction::Quit;
        }

        if self.filter_mode {
            return self.handle_filter_key(key);
        }

        match &mut self.overlay {
            Overlay::None => self.handle_main_key(key),
            Overlay::Help => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter
                ) {
                    self.overlay = Overlay::None;
                    self.dirty = true;
                    AppAction::Redraw
                } else {
                    AppAction::None
                }
            }
            Overlay::CommandModal { command, .. } => {
                let cmd_clone = command.clone();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.overlay = Overlay::None;
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::Char('y') | KeyCode::Enter => {
                        self.overlay = Overlay::None;
                        self.dirty = true;
                        AppAction::CopyClipboard(cmd_clone)
                    }
                    KeyCode::Char('o') => AppAction::QuitAndPrint(cmd_clone),
                    _ => AppAction::None,
                }
            }
            Overlay::FullResult {
                run_id,
                content,
                scroll,
                ..
            } => {
                let run_id_clone = run_id.clone();
                let content_clone = content.clone();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                        self.overlay = Overlay::None;
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *scroll = scroll.saturating_add(1);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *scroll = scroll.saturating_sub(1);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::PageDown | KeyCode::Char(' ') => {
                        *scroll = scroll.saturating_add(15);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::PageUp => {
                        *scroll = scroll.saturating_sub(15);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::Home | KeyCode::Char('g') => {
                        *scroll = 0;
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::End | KeyCode::Char('G') => {
                        *scroll = usize::MAX / 2;
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::Char('P') => AppAction::OpenPager(content_clone),
                    KeyCode::Char('y') => {
                        let cmd = self
                            .selected_run()
                            .map(|r| {
                                format_repo_cmd(
                                    &self.cwd,
                                    &r.repo_path,
                                    &format!(
                                        "{} result {}",
                                        shell_quote(&self.delegate_bin.to_string_lossy()),
                                        shell_quote(&run_id_clone)
                                    ),
                                )
                            })
                            .unwrap_or_default();
                        AppAction::CopyClipboard(cmd)
                    }
                    _ => AppAction::None,
                }
            }
            Overlay::DiffStat { run_id, scroll, .. } => {
                let run_id_clone = run_id.clone();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('d') => {
                        self.overlay = Overlay::None;
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *scroll = scroll.saturating_add(1);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *scroll = scroll.saturating_sub(1);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::PageDown => {
                        *scroll = scroll.saturating_add(15);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::PageUp => {
                        *scroll = scroll.saturating_sub(15);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::Home | KeyCode::Char('g') => {
                        *scroll = 0;
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::End | KeyCode::Char('G') => {
                        *scroll = usize::MAX / 2;
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::Char('y') => {
                        let cmd = self
                            .selected_run()
                            .map(|r| {
                                format_repo_cmd(
                                    &self.cwd,
                                    &r.repo_path,
                                    &format!(
                                        "{} diff --total {}",
                                        shell_quote(&self.delegate_bin.to_string_lossy()),
                                        shell_quote(&run_id_clone)
                                    ),
                                )
                            })
                            .unwrap_or_default();
                        AppAction::CopyClipboard(cmd)
                    }
                    _ => AppAction::None,
                }
            }
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Enter => {
                self.filter_mode = false;
                self.dirty = true;
                if self.needs_selected_result() {
                    AppAction::LoadSelectedResult
                } else {
                    AppAction::Redraw
                }
            }
            KeyCode::Esc => {
                self.filter_mode = false;
                if !self.filter.is_empty() {
                    let prev = self.selected_run().map(|r| (r.repo_path.clone(), r.run.clone()));
                    self.filter.clear();
                    self.rebuild_rows(prev);
                }
                self.dirty = true;
                AppAction::Redraw
            }
            KeyCode::Backspace => {
                let prev = self.selected_run().map(|r| (r.repo_path.clone(), r.run.clone()));
                self.filter.pop();
                self.rebuild_rows(prev);
                self.dirty = true;
                AppAction::Redraw
            }
            KeyCode::Char(c) => {
                let prev = self.selected_run().map(|r| (r.repo_path.clone(), r.run.clone()));
                self.filter.push(c);
                self.rebuild_rows(prev);
                self.dirty = true;
                AppAction::Redraw
            }
            _ => AppAction::None,
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Char('1') => {
                if self.tab != ViewTab::Runs {
                    self.tab = ViewTab::Runs;
                    self.detail_scroll = 0;
                    self.dirty = true;
                    return AppAction::Redraw;
                }
                return AppAction::None;
            }
            KeyCode::Char('2') => {
                if self.tab != ViewTab::Events {
                    self.tab = ViewTab::Events;
                    self.detail_scroll = 0;
                    self.dirty = true;
                    return AppAction::Redraw;
                }
                return AppAction::None;
            }
            KeyCode::Char('3') => {
                if self.tab != ViewTab::Merges {
                    self.tab = ViewTab::Merges;
                    self.detail_scroll = 0;
                    self.dirty = true;
                    return AppAction::Redraw;
                }
                return AppAction::None;
            }
            KeyCode::Char('4') => {
                if self.tab != ViewTab::Candidates {
                    self.tab = ViewTab::Candidates;
                    self.detail_scroll = 0;
                    self.dirty = true;
                    return AppAction::Redraw;
                }
                return AppAction::None;
            }
            KeyCode::Char('e') => {
                self.tab = if self.tab == ViewTab::Events {
                    ViewTab::Runs
                } else {
                    ViewTab::Events
                };
                self.detail_scroll = 0;
                self.dirty = true;
                return AppAction::Redraw;
            }
            KeyCode::Char('m') => {
                self.tab = if self.tab == ViewTab::Merges {
                    ViewTab::Runs
                } else {
                    ViewTab::Merges
                };
                self.detail_scroll = 0;
                self.dirty = true;
                return AppAction::Redraw;
            }
            KeyCode::Char('b') => {
                self.tab = if self.tab == ViewTab::Candidates {
                    ViewTab::Runs
                } else {
                    ViewTab::Candidates
                };
                self.detail_scroll = 0;
                self.dirty = true;
                return AppAction::Redraw;
            }
            KeyCode::Char('f') if self.tab == ViewTab::Events => {
                self.events_follow = !self.events_follow;
                if self.events_follow && !self.filtered_events.is_empty() {
                    self.selected_event = Some(self.filtered_events.len() - 1);
                    self.detail_scroll = 0;
                }
                self.dirty = true;
                return AppAction::Redraw;
            }
            _ => {}
        }

        if matches!(self.tab, ViewTab::Merges | ViewTab::Candidates) {
            let is_merges = self.tab == ViewTab::Merges;
            return match key.code {
                KeyCode::Char('q') => AppAction::Quit,
                KeyCode::Tab | KeyCode::BackTab => {
                    self.focus = match self.focus {
                        FocusPane::List => FocusPane::Detail,
                        FocusPane::Detail => FocusPane::List,
                    };
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::Down | KeyCode::Char('j') => match self.focus {
                    FocusPane::List => {
                        let moved = if is_merges {
                            self.move_merge_selection(1)
                        } else {
                            self.move_candidate_selection(1)
                        };
                        if moved {
                            self.dirty = true;
                            AppAction::Redraw
                        } else {
                            AppAction::None
                        }
                    }
                    FocusPane::Detail => {
                        self.detail_scroll = self.detail_scroll.saturating_add(1);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                },
                KeyCode::Up | KeyCode::Char('k') => match self.focus {
                    FocusPane::List => {
                        let moved = if is_merges {
                            self.move_merge_selection(-1)
                        } else {
                            self.move_candidate_selection(-1)
                        };
                        if moved {
                            self.dirty = true;
                            AppAction::Redraw
                        } else {
                            AppAction::None
                        }
                    }
                    FocusPane::Detail => {
                        self.detail_scroll = self.detail_scroll.saturating_sub(1);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                },
                KeyCode::PageDown => match self.focus {
                    FocusPane::List => {
                        let moved = if is_merges {
                            self.move_merge_selection(10)
                        } else {
                            self.move_candidate_selection(10)
                        };
                        if moved {
                            self.dirty = true;
                            AppAction::Redraw
                        } else {
                            AppAction::None
                        }
                    }
                    FocusPane::Detail => {
                        self.detail_scroll = self.detail_scroll.saturating_add(10);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                },
                KeyCode::PageUp => match self.focus {
                    FocusPane::List => {
                        let moved = if is_merges {
                            self.move_merge_selection(-10)
                        } else {
                            self.move_candidate_selection(-10)
                        };
                        if moved {
                            self.dirty = true;
                            AppAction::Redraw
                        } else {
                            AppAction::None
                        }
                    }
                    FocusPane::Detail => {
                        self.detail_scroll = self.detail_scroll.saturating_sub(10);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                },
                KeyCode::Home | KeyCode::Char('g') => match self.focus {
                    FocusPane::List => {
                        let has_items = if is_merges {
                            !self.filtered_merges.is_empty()
                        } else {
                            !self.filtered_candidates.is_empty()
                        };
                        if has_items {
                            if is_merges {
                                self.selected_merge = Some(0);
                            } else {
                                self.selected_candidate = Some(0);
                            }
                            self.detail_scroll = 0;
                            self.dirty = true;
                            AppAction::Redraw
                        } else {
                            AppAction::None
                        }
                    }
                    FocusPane::Detail => {
                        self.detail_scroll = 0;
                        self.dirty = true;
                        AppAction::Redraw
                    }
                },
                KeyCode::End | KeyCode::Char('G') => match self.focus {
                    FocusPane::List => {
                        let len = if is_merges {
                            self.filtered_merges.len()
                        } else {
                            self.filtered_candidates.len()
                        };
                        if len > 0 {
                            if is_merges {
                                self.selected_merge = Some(len - 1);
                            } else {
                                self.selected_candidate = Some(len - 1);
                            }
                            self.detail_scroll = 0;
                            self.dirty = true;
                            AppAction::Redraw
                        } else {
                            AppAction::None
                        }
                    }
                    FocusPane::Detail => {
                        self.detail_scroll = usize::MAX / 2;
                        self.dirty = true;
                        AppAction::Redraw
                    }
                },
                KeyCode::Char('a') | KeyCode::Enter | KeyCode::Char('y') if is_merges => {
                    if let Some(m) = self.selected_merge_item().cloned() {
                        let cmd = format_repo_cmd(
                            &self.cwd,
                            &m.repo_path,
                            &format!(
                                "{} apply --keep-commits {}",
                                shell_quote(&self.delegate_bin.to_string_lossy()),
                                shell_quote(&m.merge_run)
                            ),
                        );
                        let note = if m.applied {
                            "该合入批次已合回源仓库。".to_string()
                        } else if !m.merge_accept_ok {
                            "注意：最终 mergeAccept 未通过，请先排查报告再合回。".to_string()
                        } else {
                            "把集成提交链原样拿到源仓库，push 仍由主控执行。".to_string()
                        };
                        self.overlay = Overlay::CommandModal {
                            action_label: "合回源仓库 (apply --keep-commits)".to_string(),
                            command: cmd,
                            note,
                        };
                        self.dirty = true;
                        AppAction::Redraw
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::Char('p') | KeyCode::Enter | KeyCode::Char('y') if !is_merges => {
                    if let Some(c) = self.selected_candidate_item().cloned() {
                        let cmd = format_repo_cmd(
                            &self.cwd,
                            &c.repo_path,
                            &format!(
                                "{} radar pick {}",
                                shell_quote(&self.delegate_bin.to_string_lossy()),
                                shell_quote(&c.id)
                            ),
                        );
                        self.overlay = Overlay::CommandModal {
                            action_label: "生成任务说明草稿 (radar pick)".to_string(),
                            command: cmd,
                            note: "仅生成任务说明草稿，不会自动派活。".to_string(),
                        };
                        self.dirty = true;
                        AppAction::Redraw
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::Char('r') => AppAction::Refresh,
                KeyCode::Char('/') => {
                    self.filter_mode = true;
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::Esc => {
                    if !self.filter.is_empty() {
                        self.filter.clear();
                        self.rebuild_rows(None);
                        self.dirty = true;
                        AppAction::Redraw
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::Char('?') => {
                    self.overlay = Overlay::Help;
                    self.dirty = true;
                    AppAction::Redraw
                }
                _ => AppAction::None,
            };
        }

        if self.tab == ViewTab::Events {
            return match key.code {
                KeyCode::Char('q') => AppAction::Quit,
                KeyCode::Tab | KeyCode::BackTab => {
                    self.focus = match self.focus {
                        FocusPane::List => FocusPane::Detail,
                        FocusPane::Detail => FocusPane::List,
                    };
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::Down | KeyCode::Char('j') => match self.focus {
                    FocusPane::List => {
                        if self.move_event_selection(1) {
                            self.dirty = true;
                            AppAction::Redraw
                        } else {
                            AppAction::None
                        }
                    }
                    FocusPane::Detail => {
                        self.detail_scroll = self.detail_scroll.saturating_add(1);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                },
                KeyCode::Up | KeyCode::Char('k') => match self.focus {
                    FocusPane::List => {
                        if self.move_event_selection(-1) {
                            self.dirty = true;
                            AppAction::Redraw
                        } else {
                            AppAction::None
                        }
                    }
                    FocusPane::Detail => {
                        self.detail_scroll = self.detail_scroll.saturating_sub(1);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                },
                KeyCode::PageDown => match self.focus {
                    FocusPane::List => {
                        if self.move_event_selection(10) {
                            self.dirty = true;
                            AppAction::Redraw
                        } else {
                            AppAction::None
                        }
                    }
                    FocusPane::Detail => {
                        self.detail_scroll = self.detail_scroll.saturating_add(10);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                },
                KeyCode::PageUp => match self.focus {
                    FocusPane::List => {
                        if self.move_event_selection(-10) {
                            self.dirty = true;
                            AppAction::Redraw
                        } else {
                            AppAction::None
                        }
                    }
                    FocusPane::Detail => {
                        self.detail_scroll = self.detail_scroll.saturating_sub(10);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                },
                KeyCode::Home | KeyCode::Char('g') => match self.focus {
                    FocusPane::List => {
                        if !self.filtered_events.is_empty() {
                            self.selected_event = Some(0);
                            self.events_follow = false;
                            self.detail_scroll = 0;
                            self.dirty = true;
                            AppAction::Redraw
                        } else {
                            AppAction::None
                        }
                    }
                    FocusPane::Detail => {
                        self.detail_scroll = 0;
                        self.dirty = true;
                        AppAction::Redraw
                    }
                },
                KeyCode::End | KeyCode::Char('G') => match self.focus {
                    FocusPane::List => {
                        if !self.filtered_events.is_empty() {
                            self.selected_event = Some(self.filtered_events.len() - 1);
                            self.events_follow = true;
                            self.detail_scroll = 0;
                            self.dirty = true;
                            AppAction::Redraw
                        } else {
                            AppAction::None
                        }
                    }
                    FocusPane::Detail => {
                        self.detail_scroll = usize::MAX / 2;
                        self.dirty = true;
                        AppAction::Redraw
                    }
                },
                KeyCode::Char('r') => AppAction::Refresh,
                KeyCode::Char('/') => {
                    self.filter_mode = true;
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::Esc => {
                    if !self.filter.is_empty() {
                        self.filter.clear();
                        self.rebuild_rows(None);
                        self.dirty = true;
                        AppAction::Redraw
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::Char('?') => {
                    self.overlay = Overlay::Help;
                    self.dirty = true;
                    AppAction::Redraw
                }
                _ => AppAction::None,
            };
        }

        match key.code {
            KeyCode::Char('q') => AppAction::Quit,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    FocusPane::List => FocusPane::Detail,
                    FocusPane::Detail => FocusPane::List,
                };
                self.dirty = true;
                AppAction::Redraw
            }
            KeyCode::Down | KeyCode::Char('j') => match self.focus {
                FocusPane::List => {
                    if self.move_selection(1) {
                        self.dirty = true;
                        if self.needs_selected_result() {
                            AppAction::LoadSelectedResult
                        } else {
                            AppAction::Redraw
                        }
                    } else {
                        AppAction::None
                    }
                }
                FocusPane::Detail => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                    self.dirty = true;
                    AppAction::Redraw
                }
            },
            KeyCode::Up | KeyCode::Char('k') => match self.focus {
                FocusPane::List => {
                    if self.move_selection(-1) {
                        self.dirty = true;
                        if self.needs_selected_result() {
                            AppAction::LoadSelectedResult
                        } else {
                            AppAction::Redraw
                        }
                    } else {
                        AppAction::None
                    }
                }
                FocusPane::Detail => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                    self.dirty = true;
                    AppAction::Redraw
                }
            },
            KeyCode::PageDown => match self.focus {
                FocusPane::List => {
                    if self.move_selection(10) {
                        self.dirty = true;
                        if self.needs_selected_result() {
                            AppAction::LoadSelectedResult
                        } else {
                            AppAction::Redraw
                        }
                    } else {
                        AppAction::None
                    }
                }
                FocusPane::Detail => {
                    self.detail_scroll = self.detail_scroll.saturating_add(10);
                    self.dirty = true;
                    AppAction::Redraw
                }
            },
            KeyCode::PageUp => match self.focus {
                FocusPane::List => {
                    if self.move_selection(-10) {
                        self.dirty = true;
                        if self.needs_selected_result() {
                            AppAction::LoadSelectedResult
                        } else {
                            AppAction::Redraw
                        }
                    } else {
                        AppAction::None
                    }
                }
                FocusPane::Detail => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(10);
                    self.dirty = true;
                    AppAction::Redraw
                }
            },
            KeyCode::Home | KeyCode::Char('g') => match self.focus {
                FocusPane::List => {
                    let first = self.rows.iter().position(ListRow::is_selectable);
                    if first != self.selected_row {
                        self.selected_row = first;
                        self.detail_scroll = 0;
                        self.dirty = true;
                        if self.needs_selected_result() {
                            AppAction::LoadSelectedResult
                        } else {
                            AppAction::Redraw
                        }
                    } else {
                        AppAction::None
                    }
                }
                FocusPane::Detail => {
                    self.detail_scroll = 0;
                    self.dirty = true;
                    AppAction::Redraw
                }
            },
            KeyCode::End | KeyCode::Char('G') => match self.focus {
                FocusPane::List => {
                    let last = self.rows.iter().rposition(ListRow::is_selectable);
                    if last != self.selected_row {
                        self.selected_row = last;
                        self.detail_scroll = 0;
                        self.dirty = true;
                        if self.needs_selected_result() {
                            AppAction::LoadSelectedResult
                        } else {
                            AppAction::Redraw
                        }
                    } else {
                        AppAction::None
                    }
                }
                FocusPane::Detail => {
                    self.detail_scroll = usize::MAX / 2;
                    self.dirty = true;
                    AppAction::Redraw
                }
            },
            KeyCode::Enter => {
                if self.toggle_selected_fold() {
                    AppAction::Redraw
                } else if self.selected_run().is_some() {
                    AppAction::OpenFullResult
                } else {
                    AppAction::None
                }
            }
            KeyCode::Char(' ') => {
                if self.toggle_selected_fold() {
                    AppAction::Redraw
                } else {
                    AppAction::None
                }
            }
            KeyCode::Char('d') => {
                if self.selected_run().is_some() {
                    AppAction::OpenDiffStat
                } else {
                    AppAction::None
                }
            }
            KeyCode::Char('r') => AppAction::Refresh,
            KeyCode::Char('/') => {
                self.filter_mode = true;
                self.dirty = true;
                AppAction::Redraw
            }
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    let prev = self.selected_run().map(|r| (r.repo_path.clone(), r.run.clone()));
                    self.filter.clear();
                    self.rebuild_rows(prev);
                    self.dirty = true;
                    AppAction::Redraw
                } else {
                    AppAction::None
                }
            }
            KeyCode::Char('a') => {
                self.open_command_modal(WriteAction::Apply);
                AppAction::Redraw
            }
            KeyCode::Char('c') => {
                self.open_command_modal(WriteAction::Clean);
                AppAction::Redraw
            }
            KeyCode::Char('s') => {
                self.open_command_modal(WriteAction::Stop);
                AppAction::Redraw
            }
            KeyCode::Char('p') | KeyCode::Char('R') => {
                self.open_command_modal(WriteAction::Reply);
                AppAction::Redraw
            }
            KeyCode::Char('y') => {
                if let Some(run) = self.selected_run().cloned() {
                    let action = recommended_action(&run);
                    let (cmd, _) = build_write_command(&self.delegate_bin, &self.cwd, &run, action);
                    AppAction::CopyClipboard(cmd)
                } else {
                    AppAction::None
                }
            }
            KeyCode::Char('?') => {
                self.overlay = Overlay::Help;
                self.dirty = true;
                AppAction::Redraw
            }
            _ => AppAction::None,
        }
    }

    pub fn move_selection(&mut self, delta: isize) -> bool {
        let selectable_indices: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.is_selectable().then_some(i))
            .collect();
        if selectable_indices.is_empty() {
            self.selected_row = None;
            return false;
        }
        let cur_pos = self
            .selected_row
            .and_then(|sel| selectable_indices.iter().position(|&i| i == sel))
            .unwrap_or(0);
        let next_pos = if delta >= 0 {
            (cur_pos + delta as usize).min(selectable_indices.len() - 1)
        } else {
            cur_pos.saturating_sub((-delta) as usize)
        };
        let next_row = Some(selectable_indices[next_pos]);
        if next_row != self.selected_row {
            self.selected_row = next_row;
            self.detail_scroll = 0;
            true
        } else {
            false
        }
    }
}

pub fn recommended_action(run: &RunItem) -> WriteAction {
    if matches!(run.group, GroupKind::Running | GroupKind::Queued) {
        WriteAction::Stop
    } else if run.applied {
        WriteAction::Clean
    } else if run.question || matches!(run.state.as_str(), "rejected" | "failed" | "timeout" | "stalled") {
        WriteAction::Reply
    } else if run.has_worktree && run.display_changes().0 > 0 {
        WriteAction::Apply
    } else {
        WriteAction::Clean
    }
}

pub fn build_write_command(
    delegate_bin: &Path,
    cwd: &Path,
    run: &RunItem,
    action: WriteAction,
) -> (String, String) {
    let bin = shell_quote(&delegate_bin.to_string_lossy());
    let id = shell_quote(&run.run);
    let (inner, note) = match action {
        WriteAction::Apply => {
            let note = if matches!(run.group, GroupKind::Running | GroupKind::Queued) {
                "任务仍在运行/排队中，建议结束后再合入。".to_string()
            } else if run.applied {
                "该任务已合入。".to_string()
            } else if run.has_protect_violation() {
                "检测到受保护路径改动，合入前请先核对 diff。".to_string()
            } else {
                String::new()
            };
            (format!("{bin} diff --total {id} && {bin} apply {id}"), note)
        }
        WriteAction::Clean => {
            let needs_force = !run.applied && matches!(run.group, GroupKind::Pending);
            let cmd = if needs_force {
                format!("{bin} clean --force {id}")
            } else {
                format!("{bin} clean {id}")
            };
            let note = if matches!(run.group, GroupKind::Running | GroupKind::Queued) {
                "活跃任务需先 stop 后再 clean。".to_string()
            } else if needs_force {
                "未合入任务需 --force，清理前自动归档记录与证据。".to_string()
            } else {
                String::new()
            };
            (cmd, note)
        }
        WriteAction::Stop => {
            let note = if matches!(run.group, GroupKind::Running | GroupKind::Queued) {
                String::new()
            } else {
                "该任务已结束。".to_string()
            };
            (format!("{bin} stop {id}"), note)
        }
        WriteAction::Reply => {
            let (cmd, note) = if matches!(run.group, GroupKind::Running | GroupKind::Queued) {
                (
                    format!("{bin} reply --when-done {id} '<追问内容>'"),
                    "运行中使用 --when-done，结束后自动续做。".to_string(),
                )
            } else if run.question {
                (
                    format!("{bin} reply {id} '<决定内容>'"),
                    "回答提问不消耗返工次数。".to_string(),
                )
            } else {
                (
                    format!("{bin} reply {id} '<下一步说明>'"),
                    String::new(),
                )
            };
            (cmd, note)
        }
    };
    (format_repo_cmd(cwd, &run.repo_path, &inner), note)
}

fn format_repo_cmd(cwd: &Path, repo: &Path, cmd: &str) -> String {
    if cwd == repo || repo.as_os_str().is_empty() {
        cmd.to_string()
    } else {
        format!("cd {} && {cmd}", shell_quote(&repo.to_string_lossy()))
    }
}

pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b':' | b'=' | b'+'))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Copy text using OSC 52 plus best-effort local clipboard tool (`wl-copy`/`xclip`/`xsel`).
pub fn copy_to_clipboard(text: &str) -> String {
    let b64 = base64_encode(text.as_bytes());
    let mut stdout = std::io::stdout().lock();
    let _ = write!(stdout, "\x1b]52;c;{b64}\x07");
    let _ = stdout.flush();

    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (prog, args) in candidates {
        if let Ok(mut child) = Command::new(prog)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().is_ok_and(|s| s.success()) {
                return format!("已复制到剪贴板 ({prog} + OSC 52)");
            }
        }
    }
    "已发送到终端剪贴板 (OSC 52)".to_string()
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}
