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
    Overview,
    Runs,
    Events,
    Merges,
    Candidates,
    Work,
    Accounts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverviewBarMode {
    Purpose,
    Model,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkFilterMode {
    All,
    OwnerOnly,
    DroppedOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkFoldKind {
    Done,
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkRow {
    GroupHeader {
        bucket: WorkBucket,
        count: usize,
    },
    Item {
        work_idx: usize,
        depth: usize,
        /// For each ancestor level 0..depth-1, true if a vertical guide `│` continues downward.
        branch_guides: Vec<bool>,
        is_last_sibling: bool,
        /// Number of additional prerequisites beyond the parent this item is attached to.
        extra_deps: usize,
    },
    FoldToggle {
        kind: WorkFoldKind,
        count: usize,
        expanded: bool,
    },
}

impl WorkRow {
    pub fn is_selectable(&self) -> bool {
        matches!(self, WorkRow::Item { .. } | WorkRow::FoldToggle { .. })
    }
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
    WorkCard {
        work_id: String,
        scroll: usize,
    },
    ConfirmAccountSwitch {
        id: String,
        email: String,
        current_active: Option<String>,
        is_already_active: bool,
        running_count: usize,
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
    SwitchAgyAccount(String),
    RefreshQuota,
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
    pub filtered_accounts: Vec<usize>,
    pub selected_account: Option<usize>,
    pub accounts_scroll: usize,
    pub revealed_accounts: HashSet<String>,
    pub switching_account: Option<String>,
    pub account_switch_error: Option<String>,
    pub overview_scroll: usize,
    pub overview_bar_mode: OverviewBarMode,
    pub quota_refreshing: bool,
    pub work_rows: Vec<WorkRow>,
    pub selected_work_row: Option<usize>,
    pub work_scroll_line: usize,
    pub work_filter_mode: WorkFilterMode,
    pub work_done_expanded: bool,
    pub work_dropped_expanded: bool,
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
            filtered_accounts: Vec::new(),
            selected_account: None,
            accounts_scroll: 0,
            revealed_accounts: HashSet::new(),
            switching_account: None,
            account_switch_error: None,
            overview_scroll: 0,
            overview_bar_mode: OverviewBarMode::Purpose,
            quota_refreshing: false,
            work_rows: Vec::new(),
            selected_work_row: None,
            work_scroll_line: 0,
            work_filter_mode: WorkFilterMode::All,
            work_done_expanded: false,
            work_dropped_expanded: false,
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

        // Rebuild filtered agy accounts list preserving selection by account id.
        let prev_acct_id = self.selected_account_item().map(|a| a.id.clone());
        self.filtered_accounts = self
            .snapshot
            .agy_accounts
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.matches_filter(&query))
            .map(|(i, _)| i)
            .collect();
        if self.filtered_accounts.is_empty() {
            self.selected_account = None;
            self.accounts_scroll = 0;
        } else if let Some(ref aid) = prev_acct_id {
            self.selected_account = self
                .filtered_accounts
                .iter()
                .position(|&idx| self.snapshot.agy_accounts.accounts[idx].id == *aid)
                .or(Some(0));
        } else {
            // Default select active account if present, else first
            self.selected_account = self
                .filtered_accounts
                .iter()
                .position(|&idx| self.snapshot.agy_accounts.accounts[idx].active)
                .or(Some(0));
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

        self.rebuild_work_rows(None);
    }

    pub fn selected_work_item(&self) -> Option<&WorkItem> {
        let row_idx = self.selected_work_row?;
        match self.work_rows.get(row_idx)? {
            WorkRow::Item { work_idx, .. } => self.snapshot.work.get(*work_idx),
            _ => None,
        }
    }

    fn current_work_selection_id(&self) -> Option<String> {
        let row_idx = self.selected_work_row?;
        match self.work_rows.get(row_idx)? {
            WorkRow::Item { work_idx, .. } => {
                self.snapshot.work.get(*work_idx).map(|w| w.id.clone())
            }
            WorkRow::FoldToggle { kind, .. } => Some(match kind {
                WorkFoldKind::Done => "__fold_done__".to_string(),
                WorkFoldKind::Dropped => "__fold_dropped__".to_string(),
            }),
            WorkRow::GroupHeader { .. } => None,
        }
    }

    /// Rebuild the L1 Work View row list with status grouping, owner pinning,
    /// dependency chain indentation (`work_closed` / `run_applied`), and folded done/dropped sections.
    pub fn rebuild_work_rows(&mut self, preserve_id: Option<String>) {
        let prev_id = preserve_id.or_else(|| self.current_work_selection_id());
        let query = self.filter.trim().to_lowercase();
        let filtering = !query.is_empty();
        let mode = self.work_filter_mode;

        let visible_indices: Vec<usize> = self
            .snapshot
            .work
            .iter()
            .enumerate()
            .filter(|(_, w)| {
                let mode_ok = match mode {
                    WorkFilterMode::All => true,
                    WorkFilterMode::OwnerOnly => w.owner.is_some(),
                    WorkFilterMode::DroppedOnly => {
                        w.state == "done" && w.closed_kind == "dropped"
                    }
                };
                mode_ok && w.matches_filter(&query)
            })
            .map(|(i, _)| i)
            .collect();

        let mut rows = Vec::new();

        if mode == WorkFilterMode::DroppedOnly {
            let mut dropped: Vec<usize> = visible_indices;
            dropped.sort_by(|&a, &b| {
                let wa = &self.snapshot.work[a];
                let wb = &self.snapshot.work[b];
                wb.owner
                    .is_some()
                    .cmp(&wa.owner.is_some())
                    .then_with(|| wb.updated_at.cmp(&wa.updated_at))
                    .then_with(|| wa.id.cmp(&wb.id))
            });
            if !dropped.is_empty() {
                rows.push(WorkRow::GroupHeader {
                    bucket: WorkBucket::Dropped,
                    count: dropped.len(),
                });
                for idx in dropped {
                    rows.push(WorkRow::Item {
                        work_idx: idx,
                        depth: 0,
                        branch_guides: Vec::new(),
                        is_last_sibling: true,
                        extra_deps: 0,
                    });
                }
            }
            self.finish_work_rows(rows, prev_id);
            return;
        }

        // Partition visible indices into Owner (open), Engineering Open, Done, and Dropped.
        let mut owner_open: Vec<usize> = Vec::new();
        let mut eng_open: Vec<usize> = Vec::new();
        let mut done_items: Vec<usize> = Vec::new();
        let mut dropped_items: Vec<usize> = Vec::new();

        for idx in visible_indices {
            let w = &self.snapshot.work[idx];
            match w.bucket() {
                WorkBucket::Owner => owner_open.push(idx),
                WorkBucket::Done => done_items.push(idx),
                WorkBucket::Dropped => dropped_items.push(idx),
                _ => eng_open.push(idx),
            }
        }

        // 1. Owner items pinned at top: 动手 -> 点头 -> 看卡 -> 方向, then ID
        owner_open.sort_by(|&a, &b| {
            let wa = &self.snapshot.work[a];
            let wb = &self.snapshot.work[b];
            let ka = wa.owner.as_ref().map(|o| o.kind);
            let kb = wb.owner.as_ref().map(|o| o.kind);
            ka.cmp(&kb).then_with(|| wa.id.cmp(&wb.id))
        });
        if !owner_open.is_empty() {
            rows.push(WorkRow::GroupHeader {
                bucket: WorkBucket::Owner,
                count: owner_open.len(),
            });
            for idx in owner_open {
                rows.push(WorkRow::Item {
                    work_idx: idx,
                    depth: 0,
                    branch_guides: Vec::new(),
                    is_last_sibling: true,
                    extra_deps: 0,
                });
            }
        }

        // 2. Engineering open items: build dependency parent links among open engineering items
        let eng_set: HashSet<usize> = eng_open.iter().copied().collect();
        let id_to_idx: HashMap<String, usize> = eng_open
            .iter()
            .map(|&i| (self.snapshot.work[i].id.clone(), i))
            .collect();

        // Map each engineering item to its first open dependency parent (work_closed or run_applied)
        let mut parent_of: HashMap<usize, usize> = HashMap::new();
        let mut extra_deps_of: HashMap<usize, usize> = HashMap::new();
        let mut children_of: HashMap<usize, Vec<usize>> = HashMap::new();

        for &idx in &eng_open {
            let w = &self.snapshot.work[idx];
            let mut first_parent: Option<usize> = None;
            let mut dep_count: usize = 0;
            for cond in &w.wait_on {
                if let Some(wid) = cond.strip_prefix("work_closed:") {
                    dep_count += 1;
                    if first_parent.is_none() {
                        let norm = normalize_wid_for_match(wid);
                        if let Some(&pidx) = id_to_idx.get(&norm) {
                            if pidx != idx && eng_set.contains(&pidx) {
                                first_parent = Some(pidx);
                            }
                        }
                    }
                } else if let Some(run_ref) = cond.strip_prefix("run_applied:") {
                    dep_count += 1;
                    if first_parent.is_none() {
                        if let Some(pidx) = find_work_by_run_ref(&self.snapshot.work, &eng_open, run_ref) {
                            if pidx != idx {
                                first_parent = Some(pidx);
                            }
                        }
                    }
                } else if cond.starts_with("owner:")
                    || cond.starts_with("run_delivered:")
                    || cond.starts_with("after_time:")
                    || cond.starts_with("manual:")
                {
                    dep_count += 1;
                }
            }
            if let Some(pidx) = first_parent {
                let p_bucket = self.snapshot.work[pidx].bucket();
                let c_bucket = w.bucket();
                // Attach under parent if parent has >= priority (smaller bucket ordinal) or child is Waiting
                let can_attach = p_bucket <= c_bucket || c_bucket == WorkBucket::Waiting;
                if can_attach && !creates_parent_cycle(idx, pidx, &parent_of) {
                    parent_of.insert(idx, pidx);
                    extra_deps_of.insert(idx, dep_count.saturating_sub(1));
                    children_of.entry(pidx).or_default().push(idx);
                }
            }
        }

        let item_sort_key = |idx: usize| -> (WorkBucket, u8, String) {
            let w = &self.snapshot.work[idx];
            // Within NeedsAction, put attention before verify
            let sub = if w.state == "attention" {
                0
            } else if w.state == "verify" {
                1
            } else {
                2
            };
            (w.bucket(), sub, w.id.clone())
        };

        for kids in children_of.values_mut() {
            kids.sort_by_key(|&i| item_sort_key(i));
        }

        let open_buckets = [
            WorkBucket::NeedsAction,
            WorkBucket::InProgress,
            WorkBucket::Ready,
            WorkBucket::Waiting,
            WorkBucket::Deferred,
        ];
        let mut emitted: HashSet<usize> = HashSet::new();

        for bucket in open_buckets {
            let mut roots: Vec<usize> = eng_open
                .iter()
                .copied()
                .filter(|&i| !parent_of.contains_key(&i) && self.snapshot.work[i].bucket() == bucket)
                .collect();
            roots.sort_by_key(|&i| item_sort_key(i));
            if roots.is_empty() {
                continue;
            }
            rows.push(WorkRow::GroupHeader {
                bucket,
                count: roots.len(),
            });
            for root in roots {
                emit_work_subtree(
                    root,
                    0,
                    &mut Vec::new(),
                    true,
                    &children_of,
                    &extra_deps_of,
                    &mut emitted,
                    &mut rows,
                );
            }
        }

        // Safety net: emit any un-emitted open engineering items (e.g. disconnected cycle)
        let mut leftover: Vec<usize> = eng_open
            .iter()
            .copied()
            .filter(|i| !emitted.contains(i))
            .collect();
        leftover.sort_by_key(|&i| item_sort_key(i));
        for idx in leftover {
            rows.push(WorkRow::Item {
                work_idx: idx,
                depth: 0,
                branch_guides: Vec::new(),
                is_last_sibling: true,
                extra_deps: 0,
            });
        }

        // 3. Folded Done section (newest first; expanded when text-filtering or explicitly toggled)
        done_items.sort_by(|&a, &b| {
            let wa = &self.snapshot.work[a];
            let wb = &self.snapshot.work[b];
            wb.updated_at
                .cmp(&wa.updated_at)
                .then_with(|| wb.id.cmp(&wa.id))
        });
        if !done_items.is_empty() {
            let expanded = self.work_done_expanded || filtering;
            rows.push(WorkRow::FoldToggle {
                kind: WorkFoldKind::Done,
                count: done_items.len(),
                expanded,
            });
            if expanded {
                for idx in done_items {
                    rows.push(WorkRow::Item {
                        work_idx: idx,
                        depth: 0,
                        branch_guides: Vec::new(),
                        is_last_sibling: true,
                        extra_deps: 0,
                    });
                }
            }
        }

        // 4. Folded Dropped (已否) section
        dropped_items.sort_by(|&a, &b| {
            let wa = &self.snapshot.work[a];
            let wb = &self.snapshot.work[b];
            wb.owner
                .is_some()
                .cmp(&wa.owner.is_some())
                .then_with(|| wb.updated_at.cmp(&wa.updated_at))
                .then_with(|| wb.id.cmp(&wa.id))
        });
        if !dropped_items.is_empty() {
            let expanded = self.work_dropped_expanded || filtering;
            rows.push(WorkRow::FoldToggle {
                kind: WorkFoldKind::Dropped,
                count: dropped_items.len(),
                expanded,
            });
            if expanded {
                for idx in dropped_items {
                    rows.push(WorkRow::Item {
                        work_idx: idx,
                        depth: 0,
                        branch_guides: Vec::new(),
                        is_last_sibling: true,
                        extra_deps: 0,
                    });
                }
            }
        }

        self.finish_work_rows(rows, prev_id);
    }

    fn finish_work_rows(&mut self, rows: Vec<WorkRow>, prev_id: Option<String>) {
        self.work_rows = rows;
        let mut next_sel = None;
        if let Some(ref key) = prev_id {
            for (i, row) in self.work_rows.iter().enumerate() {
                match row {
                    WorkRow::Item { work_idx, .. }
                        if self.snapshot.work[*work_idx].id == *key =>
                    {
                        next_sel = Some(i);
                        break;
                    }
                    WorkRow::FoldToggle {
                        kind: WorkFoldKind::Done,
                        ..
                    } if key == "__fold_done__" => {
                        next_sel = Some(i);
                        break;
                    }
                    WorkRow::FoldToggle {
                        kind: WorkFoldKind::Dropped,
                        ..
                    } if key == "__fold_dropped__" => {
                        next_sel = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
        }
        if next_sel.is_none() {
            next_sel = self
                .work_rows
                .iter()
                .position(|r| matches!(r, WorkRow::Item { .. }))
                .or_else(|| self.work_rows.iter().position(WorkRow::is_selectable));
        }
        self.selected_work_row = next_sel;
        if self.selected_work_row.is_none() {
            self.work_scroll_line = 0;
        }
    }

    pub fn move_work_selection(&mut self, delta: isize) -> bool {
        let selectable: Vec<usize> = self
            .work_rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.is_selectable().then_some(i))
            .collect();
        if selectable.is_empty() {
            self.selected_work_row = None;
            return false;
        }
        let cur_pos = self
            .selected_work_row
            .and_then(|sel| selectable.iter().position(|&i| i == sel))
            .unwrap_or(0);
        let next_pos = if delta >= 0 {
            (cur_pos + delta as usize).min(selectable.len() - 1)
        } else {
            cur_pos.saturating_sub((-delta) as usize)
        };
        let next_row = Some(selectable[next_pos]);
        if next_row != self.selected_work_row {
            self.selected_work_row = next_row;
            true
        } else {
            false
        }
    }

    pub fn toggle_selected_work_fold(&mut self) -> bool {
        let Some(row_idx) = self.selected_work_row else {
            return false;
        };
        let Some(&WorkRow::FoldToggle { kind, .. }) = self.work_rows.get(row_idx) else {
            return false;
        };
        let preserve = match kind {
            WorkFoldKind::Done => {
                self.work_done_expanded = !self.work_done_expanded;
                "__fold_done__".to_string()
            }
            WorkFoldKind::Dropped => {
                self.work_dropped_expanded = !self.work_dropped_expanded;
                "__fold_dropped__".to_string()
            }
        };
        self.rebuild_work_rows(Some(preserve));
        self.dirty = true;
        true
    }

    /// Jump from a Work item (or open WorkCard) to its linked Run in the Runs view.
    pub fn jump_to_work_run(&mut self, work_id: Option<&str>) -> bool {
        let item = match work_id {
            Some(id) => self.snapshot.work_item_by_id(id).cloned(),
            None => self.selected_work_item().cloned(),
        };
        let Some(item) = item else {
            self.set_toast("当前没有选中的工作项");
            return false;
        };
        let Some((repo_idx, run_idx, run_ref)) = self.snapshot.find_run_for_work(&item) else {
            if let Some(last_rid) = item.runs.last() {
                self.set_toast(format!("关联 run 已清理（delegate show {last_rid}）"));
            } else {
                self.set_toast("该工作项暂无关联 run");
            }
            return false;
        };
        let target_repo = self.snapshot.repos[repo_idx].path.clone();
        let target_run_id = run_ref.run.clone();
        let target_group = run_ref.group;
        let is_stale = run_ref.is_stale_pending();

        // Ensure the run is not hidden inside a collapsed fold or filter
        if !self.filter.is_empty() && !run_ref.matches_filter(&self.filter.trim().to_lowercase()) {
            self.filter.clear();
        }
        match target_group {
            GroupKind::MergedUncleaned => {
                self.expanded_folds
                    .insert((target_repo.clone(), CollapseKind::MergedUncleaned));
            }
            GroupKind::Stopped => {
                self.expanded_folds
                    .insert((target_repo.clone(), CollapseKind::Stopped));
            }
            GroupKind::Pending if is_stale => {
                self.expanded_folds
                    .insert((target_repo.clone(), CollapseKind::StalePending));
            }
            _ => {}
        }
        self.rebuild_rows_inner(Some(SelectionKey::Run(
            target_repo.clone(),
            target_run_id.clone(),
        )));
        if let Some(pos) = self.rows.iter().position(|r| {
            matches!(
                r,
                ListRow::Run {
                    repo_idx: ri,
                    run_idx: rj
                } if *ri == repo_idx && *rj == run_idx
            )
        }) {
            self.selected_row = Some(pos);
        }
        self.overlay = Overlay::None;
        self.tab = ViewTab::Runs;
        self.focus = FocusPane::List;
        self.detail_scroll = 0;
        self.dirty = true;
        true
    }

    /// Jump to a specific work item ID in the Work tab (expanding done/dropped folds if needed).
    pub fn jump_to_work_id(&mut self, work_id: &str) -> bool {
        let Some(item) = self.snapshot.work_item_by_id(work_id).cloned() else {
            return false;
        };
        if item.state == "done" {
            if item.closed_kind == "dropped" {
                self.work_dropped_expanded = true;
            } else {
                self.work_done_expanded = true;
            }
        }
        self.work_filter_mode = WorkFilterMode::All;
        self.rebuild_work_rows(Some(work_id.to_string()));
        self.tab = ViewTab::Work;
        self.dirty = true;
        true
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

    pub fn selected_account_item(&self) -> Option<&AgyAccountItem> {
        let pos = self.selected_account?;
        let &idx = self.filtered_accounts.get(pos)?;
        self.snapshot.agy_accounts.accounts.get(idx)
    }

    pub fn is_account_revealed(&self, id: &str) -> bool {
        self.revealed_accounts.contains(id)
    }

    pub fn move_account_selection(&mut self, delta: isize) -> bool {
        if self.filtered_accounts.is_empty() {
            self.selected_account = None;
            return false;
        }
        let len = self.filtered_accounts.len();
        let cur = self.selected_account.unwrap_or(0);
        let next = if delta >= 0 {
            (cur + delta as usize).min(len - 1)
        } else {
            cur.saturating_sub((-delta) as usize)
        };
        if Some(next) != self.selected_account {
            self.selected_account = Some(next);
            self.detail_scroll = 0;
            true
        } else {
            false
        }
    }

    pub fn open_account_switch_confirm(&mut self) -> bool {
        if self.switching_account.is_some() {
            self.set_toast("正在切换账号，请稍候…");
            return false;
        }
        let Some(acct) = self.selected_account_item().cloned() else {
            self.set_toast("当前没有可切换的 agy 账号");
            return false;
        };
        self.overlay = Overlay::ConfirmAccountSwitch {
            id: acct.id,
            email: acct.email,
            current_active: self.snapshot.agy_accounts.active.clone(),
            is_already_active: acct.active,
            running_count: self.snapshot.agy_accounts.running_agy.len(),
        };
        self.dirty = true;
        true
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
            Overlay::ConfirmAccountSwitch { id, .. } => {
                let target_id = id.clone();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                        self.overlay = Overlay::None;
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::Enter | KeyCode::Char('y') => {
                        self.overlay = Overlay::None;
                        self.switching_account = Some(target_id.clone());
                        self.account_switch_error = None;
                        self.dirty = true;
                        AppAction::SwitchAgyAccount(target_id)
                    }
                    _ => AppAction::None,
                }
            }
            Overlay::WorkCard { work_id, scroll } => {
                let wid = work_id.clone();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter | KeyCode::Left => {
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
                        *scroll = scroll.saturating_add(12);
                        self.dirty = true;
                        AppAction::Redraw
                    }
                    KeyCode::PageUp => {
                        *scroll = scroll.saturating_sub(12);
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
                    KeyCode::Char('t') | KeyCode::Right => {
                        if self.jump_to_work_run(Some(&wid)) {
                            if self.needs_selected_result() {
                                AppAction::LoadSelectedResult
                            } else {
                                AppAction::Redraw
                            }
                        } else {
                            AppAction::Redraw
                        }
                    }
                    KeyCode::Char('y') => {
                        let cmd = self
                            .snapshot
                            .work_item_by_id(&wid)
                            .map(|w| build_work_next_command(&self.delegate_bin, &self.cwd, w))
                            .unwrap_or_default();
                        if cmd.is_empty() {
                            AppAction::None
                        } else {
                            AppAction::CopyClipboard(cmd)
                        }
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
            KeyCode::Char('0') => {
                if self.tab != ViewTab::Overview {
                    self.tab = ViewTab::Overview;
                    self.overview_scroll = 0;
                    self.detail_scroll = 0;
                    self.dirty = true;
                    return AppAction::Redraw;
                }
                return AppAction::None;
            }
            KeyCode::Char('O') => {
                self.tab = if self.tab == ViewTab::Overview {
                    ViewTab::Runs
                } else {
                    ViewTab::Overview
                };
                self.overview_scroll = 0;
                self.detail_scroll = 0;
                self.dirty = true;
                return AppAction::Redraw;
            }
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
            KeyCode::Char('5') => {
                if self.tab != ViewTab::Work {
                    self.tab = ViewTab::Work;
                    self.detail_scroll = 0;
                    self.dirty = true;
                    return AppAction::Redraw;
                }
                return AppAction::None;
            }
            KeyCode::Char('6') => {
                if self.tab != ViewTab::Accounts {
                    self.tab = ViewTab::Accounts;
                    self.detail_scroll = 0;
                    self.dirty = true;
                    return AppAction::Redraw;
                }
                return AppAction::None;
            }
            KeyCode::Char('A') => {
                self.tab = if self.tab == ViewTab::Accounts {
                    ViewTab::Runs
                } else {
                    ViewTab::Accounts
                };
                self.detail_scroll = 0;
                self.dirty = true;
                return AppAction::Redraw;
            }
            KeyCode::Char('w') => {
                if self.tab == ViewTab::Work {
                    self.tab = ViewTab::Runs;
                    self.detail_scroll = 0;
                    self.dirty = true;
                    return AppAction::Redraw;
                } else {
                    if self.tab == ViewTab::Runs {
                        if let Some(run) = self.selected_run().cloned() {
                            if let Some(wid) = self.snapshot.work_id_for_run(&run) {
                                if self.jump_to_work_id(&wid) {
                                    return AppAction::Redraw;
                                }
                            }
                        }
                    }
                    self.tab = ViewTab::Work;
                    self.detail_scroll = 0;
                    self.dirty = true;
                    return AppAction::Redraw;
                }
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

        if self.tab == ViewTab::Overview {
            return match key.code {
                KeyCode::Char('q') => AppAction::Quit,
                KeyCode::Down | KeyCode::Char('j') => {
                    self.overview_scroll = self.overview_scroll.saturating_add(1);
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.overview_scroll = self.overview_scroll.saturating_sub(1);
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    self.overview_scroll = self.overview_scroll.saturating_add(8);
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::PageUp => {
                    self.overview_scroll = self.overview_scroll.saturating_sub(8);
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.overview_scroll = 0;
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.overview_scroll = usize::MAX / 2;
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::Char('v') => {
                    self.overview_bar_mode = match self.overview_bar_mode {
                        OverviewBarMode::Purpose => OverviewBarMode::Model,
                        OverviewBarMode::Model => OverviewBarMode::Purpose,
                    };
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::Char('Q') => AppAction::RefreshQuota,
                KeyCode::Char('r') => AppAction::Refresh,
                KeyCode::Char('?') => {
                    self.overlay = Overlay::Help;
                    self.dirty = true;
                    AppAction::Redraw
                }
                _ => AppAction::None,
            };
        }

        if self.tab == ViewTab::Accounts {
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
                        if self.move_account_selection(1) {
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
                        if self.move_account_selection(-1) {
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
                        if self.move_account_selection(10) {
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
                        if self.move_account_selection(-10) {
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
                        if !self.filtered_accounts.is_empty() {
                            self.selected_account = Some(0);
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
                        if !self.filtered_accounts.is_empty() {
                            self.selected_account = Some(self.filtered_accounts.len() - 1);
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
                KeyCode::Enter | KeyCode::Char('u') => {
                    self.open_account_switch_confirm();
                    AppAction::Redraw
                }
                KeyCode::Char('i') => {
                    if let Some(id) = self.selected_account_item().map(|a| a.id.clone()) {
                        if !self.revealed_accounts.remove(&id) {
                            self.revealed_accounts.insert(id);
                        }
                        self.dirty = true;
                        AppAction::Redraw
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::Char('y') => {
                    if let Some(a) = self.selected_account_item() {
                        let cmd = format!(
                            "{} agy account use {}",
                            shell_quote(&self.delegate_bin.to_string_lossy()),
                            shell_quote(&a.id)
                        );
                        AppAction::CopyClipboard(cmd)
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::Char('Q') => AppAction::RefreshQuota,
                KeyCode::Char('r') => AppAction::Refresh,
                KeyCode::Char('/') => {
                    self.filter_mode = true;
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::Esc => {
                    let mut changed = false;
                    if !self.filter.is_empty() {
                        self.filter.clear();
                        self.rebuild_rows(None);
                        changed = true;
                    }
                    if self.account_switch_error.is_some() {
                        self.account_switch_error = None;
                        changed = true;
                    }
                    if changed {
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

        if self.tab == ViewTab::Work {
            return match key.code {
                KeyCode::Char('q') => AppAction::Quit,
                KeyCode::Down | KeyCode::Char('j') => {
                    if self.move_work_selection(1) {
                        self.dirty = true;
                        AppAction::Redraw
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if self.move_work_selection(-1) {
                        self.dirty = true;
                        AppAction::Redraw
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::PageDown => {
                    if self.move_work_selection(10) {
                        self.dirty = true;
                        AppAction::Redraw
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::PageUp => {
                    if self.move_work_selection(-10) {
                        self.dirty = true;
                        AppAction::Redraw
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    let first = self.work_rows.iter().position(WorkRow::is_selectable);
                    if first != self.selected_work_row {
                        self.selected_work_row = first;
                        self.dirty = true;
                        AppAction::Redraw
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::End | KeyCode::Char('G') => {
                    let last = self.work_rows.iter().rposition(WorkRow::is_selectable);
                    if last != self.selected_work_row {
                        self.selected_work_row = last;
                        self.dirty = true;
                        AppAction::Redraw
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::Enter => {
                    if self.toggle_selected_work_fold() {
                        AppAction::Redraw
                    } else if let Some(w) = self.selected_work_item().cloned() {
                        self.overlay = Overlay::WorkCard {
                            work_id: w.id,
                            scroll: 0,
                        };
                        self.dirty = true;
                        AppAction::Redraw
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::Char(' ') => {
                    if self.toggle_selected_work_fold() {
                        AppAction::Redraw
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::Char('t') | KeyCode::Right => {
                    if self.jump_to_work_run(None) {
                        if self.needs_selected_result() {
                            AppAction::LoadSelectedResult
                        } else {
                            AppAction::Redraw
                        }
                    } else {
                        AppAction::Redraw
                    }
                }
                KeyCode::Char('o') => {
                    self.work_filter_mode = if self.work_filter_mode == WorkFilterMode::OwnerOnly {
                        WorkFilterMode::All
                    } else {
                        WorkFilterMode::OwnerOnly
                    };
                    self.work_scroll_line = 0;
                    self.rebuild_work_rows(None);
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::Char('x') => {
                    self.work_filter_mode =
                        if self.work_filter_mode == WorkFilterMode::DroppedOnly {
                            WorkFilterMode::All
                        } else {
                            WorkFilterMode::DroppedOnly
                        };
                    self.work_scroll_line = 0;
                    self.rebuild_work_rows(None);
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::Char('y') => {
                    if let Some(w) = self.selected_work_item().cloned() {
                        let cmd = build_work_next_command(&self.delegate_bin, &self.cwd, &w);
                        AppAction::CopyClipboard(cmd)
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::Char('Q') => AppAction::RefreshQuota,
                KeyCode::Char('r') => AppAction::Refresh,
                KeyCode::Char('/') => {
                    self.filter_mode = true;
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::Esc => {
                    if !self.filter.is_empty() || self.work_filter_mode != WorkFilterMode::All {
                        self.filter.clear();
                        self.work_filter_mode = WorkFilterMode::All;
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
                            "最终 mergeAccept 未通过，请先排查报告再合回。".to_string()
                        } else {
                            // Keeps commits intact on the source branch; push remains a separate manual step.
                            "把集成提交链落回源仓库，不推送远端。".to_string()
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
                        // `radar pick` only creates a task brief draft and never dispatches automatically.
                        self.overlay = Overlay::CommandModal {
                            action_label: "生成任务说明草稿 (radar pick)".to_string(),
                            command: cmd,
                            note: String::new(),
                        };
                        self.dirty = true;
                        AppAction::Redraw
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::Char('Q') => AppAction::RefreshQuota,
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
                KeyCode::Char('Q') => AppAction::RefreshQuota,
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
                    String::new(),
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
                // Copied via local clipboard tool (wl-copy/xclip/xsel) plus OSC 52.
                return "已复制到剪贴板".to_string();
            }
        }
    }
    // Fallback: sent to terminal clipboard via OSC 52.
    "已复制到剪贴板".to_string()
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

fn normalize_wid_for_match(raw: &str) -> String {
    let t = raw.trim();
    for (up, low, pad) in [('W', 'w', 3usize), ('Q', 'q', 2usize), ('D', 'd', 3usize)] {
        if let Some(rest) = t.strip_prefix(up).or_else(|| t.strip_prefix(low)) {
            if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                if let Ok(n) = rest.parse::<u64>() {
                    return format!("{up}{n:0pad$}");
                }
            }
        }
    }
    t.to_string()
}

fn find_work_by_run_ref(work: &[WorkItem], candidates: &[usize], run_ref: &str) -> Option<usize> {
    let needle = run_ref.trim();
    if needle.is_empty() {
        return None;
    }
    // Exact match first, then unique suffix/contains match (e.g. short run slug)
    for &idx in candidates {
        if work[idx].runs.iter().any(|r| r == needle) {
            return Some(idx);
        }
    }
    candidates
        .iter()
        .find(|&&idx| {
            work[idx]
                .runs
                .iter()
                .any(|r| r.ends_with(needle) || r.contains(needle))
        })
        .copied()
}

fn creates_parent_cycle(
    child: usize,
    proposed_parent: usize,
    parent_of: &HashMap<usize, usize>,
) -> bool {
    let mut cur = Some(proposed_parent);
    let mut steps = 0usize;
    while let Some(node) = cur {
        if node == child {
            return true;
        }
        steps += 1;
        if steps > parent_of.len() + 2 {
            return true;
        }
        cur = parent_of.get(&node).copied();
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn emit_work_subtree(
    idx: usize,
    depth: usize,
    guides: &mut Vec<bool>,
    is_last_sibling: bool,
    children_of: &HashMap<usize, Vec<usize>>,
    extra_deps_of: &HashMap<usize, usize>,
    emitted: &mut HashSet<usize>,
    rows: &mut Vec<WorkRow>,
) {
    if !emitted.insert(idx) {
        return;
    }
    let extra_deps = extra_deps_of.get(&idx).copied().unwrap_or(0);
    rows.push(WorkRow::Item {
        work_idx: idx,
        depth,
        branch_guides: guides.clone(),
        is_last_sibling,
        extra_deps,
    });
    if let Some(kids) = children_of.get(&idx) {
        let len = kids.len();
        for (pos, &kid) in kids.iter().enumerate() {
            let kid_last = pos + 1 == len;
            if depth > 0 {
                guides.push(!is_last_sibling);
            }
            emit_work_subtree(
                kid,
                depth + 1,
                guides,
                kid_last,
                children_of,
                extra_deps_of,
                emitted,
                rows,
            );
            if depth > 0 {
                guides.pop();
            }
        }
    }
}

pub fn build_work_next_command(delegate_bin: &Path, cwd: &Path, w: &WorkItem) -> String {
    let bin = shell_quote(&delegate_bin.to_string_lossy());
    let next = if w.next.is_empty() {
        format!("work show {}", w.id)
    } else {
        w.next.clone()
    };
    let cmd = if let Some(stripped) = next.strip_prefix("delegate ") {
        format!("{bin} {stripped}")
    } else if next.starts_with("work ")
        || next.starts_with("start ")
        || next.starts_with("apply ")
        || next.starts_with("reply ")
        || next.starts_with("result ")
        || next.starts_with("status ")
        || next.starts_with("wait")
    {
        format!("{bin} {next}")
    } else {
        format!("{bin} work show {}", w.id)
    };
    if w.repo.is_empty() {
        cmd
    } else {
        format_repo_cmd(cwd, Path::new(&w.repo), &cmd)
    }
}
