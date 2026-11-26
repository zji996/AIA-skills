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
    Work,
    Runs,
    Quota,
    Events,
}

impl ViewTab {
    pub const ALL: [ViewTab; 5] = [
        ViewTab::Overview,
        ViewTab::Work,
        ViewTab::Runs,
        ViewTab::Quota,
        ViewTab::Events,
    ];

    pub fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|&t| t == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        let idx = Self::ALL.iter().position(|&t| t == self).unwrap_or(0);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunsGroupMode {
    ByStatus,
    ByRepo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverviewFocus {
    Attention,
    Running,
    WorkChains,
    Deliveries,
}

impl OverviewFocus {
    pub const ALL: [OverviewFocus; 4] = [
        OverviewFocus::Attention,
        OverviewFocus::Running,
        OverviewFocus::WorkChains,
        OverviewFocus::Deliveries,
    ];

    pub fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|&f| f == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        let idx = Self::ALL.iter().position(|&f| f == self).unwrap_or(0);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverviewAttentionEntry {
    OwnerWork { work_idx: usize },
    PendingRun { repo_idx: usize, run_idx: usize },
    StaleFold { count: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkFilterMode {
    All,
    OwnerOnly,
    DroppedOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkFoldKind {
    Deferred,
    Candidates,
    Done,
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkRow {
    GroupHeader {
        bucket: WorkBucket,
        own_count: usize,
        attached_downstream: usize,
        ephemeral_count: usize,
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
    Candidate {
        cand_idx: usize,
    },
}

impl WorkRow {
    pub fn is_selectable(&self) -> bool {
        matches!(
            self,
            WorkRow::Item { .. } | WorkRow::FoldToggle { .. } | WorkRow::Candidate { .. }
        )
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
    Merges,
    Collected,
    MergedUncleaned,
    Stopped,
    DispatchedQueue,
    StaleRepos,
}

const GLOBAL_FOLD_SENTINEL: &str = "__global__";

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
    MergeEntry {
        idx: usize,
    },
}

impl ListRow {
    pub fn is_selectable(&self) -> bool {
        matches!(
            self,
            ListRow::Run { .. }
                | ListRow::ToggleFold { .. }
                | ListRow::DagQueueEntry { .. }
                | ListRow::MergeEntry { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectionKey {
    Run(PathBuf, String),
    Fold(PathBuf, CollapseKind),
    DagQueue(String),
    Merge(String),
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
    pub runs_group_mode: RunsGroupMode,
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
    pub overview_focus: OverviewFocus,
    pub overview_attention_sel: usize,
    pub overview_attention_scroll: usize,
    pub overview_stale_expanded: bool,
    pub overview_running_sel: usize,
    pub overview_running_scroll: usize,
    pub overview_work_sel: usize,
    pub overview_work_scroll: usize,
    pub overview_deliveries_sel: usize,
    pub overview_deliveries_scroll: usize,
    pub overview_scroll: usize,
    pub quota_models_expanded: bool,
    pub quota_refreshing: bool,
    pub quota_last_error: Option<String>,
    pub lines_refreshing: bool,
    pub line_stats_error: Option<String>,
    pub show_lane_events: bool,
    pub hidden_lane_events: usize,
    pub work_rows: Vec<WorkRow>,
    pub selected_work_row: Option<usize>,
    pub work_scroll_line: usize,
    pub work_filter_mode: WorkFilterMode,
    pub work_deferred_expanded: bool,
    pub work_candidates_expanded: bool,
    pub work_done_expanded: bool,
    pub work_dropped_expanded: bool,
    pub filter: String,
    pub filter_mode: bool,
    pub show_all: bool,
    pub expanded_folds: HashSet<(PathBuf, CollapseKind)>,
    pub overlay: Overlay,
    pub result_cache: HashMap<String, Result<String, String>>,
    pub diff_cache: HashMap<String, Result<String, String>>,
    pub card_cache: HashMap<String, Option<RunCardL1>>,
    pub toast: Option<(String, Instant)>,
    pub refresh_interval_secs: u64,
    pub delegate_bin: PathBuf,
    pub cwd: PathBuf,
    pub last_terminal_width: u16,
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
            tab: ViewTab::Overview,
            runs_group_mode: RunsGroupMode::ByStatus,
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
            overview_focus: OverviewFocus::Attention,
            overview_attention_sel: 0,
            overview_attention_scroll: 0,
            overview_stale_expanded: false,
            overview_running_sel: 0,
            overview_running_scroll: 0,
            overview_work_sel: 0,
            overview_work_scroll: 0,
            overview_deliveries_sel: 0,
            overview_deliveries_scroll: 0,
            overview_scroll: 0,
            quota_models_expanded: false,
            quota_refreshing: false,
            quota_last_error: None,
            lines_refreshing: false,
            line_stats_error: None,
            show_lane_events: false,
            hidden_lane_events: 0,
            work_rows: Vec::new(),
            selected_work_row: None,
            work_scroll_line: 0,
            work_filter_mode: WorkFilterMode::All,
            work_deferred_expanded: false,
            work_candidates_expanded: false,
            work_done_expanded: false,
            work_dropped_expanded: false,
            filter: String::new(),
            filter_mode: false,
            show_all: false,
            expanded_folds: HashSet::new(),
            overlay: Overlay::None,
            result_cache: HashMap::new(),
            diff_cache: HashMap::new(),
            card_cache: HashMap::new(),
            toast: None,
            refresh_interval_secs,
            delegate_bin,
            cwd,
            last_terminal_width: 180,
            loading: false,
            dirty: true,
        };
        app.rebuild_rows(None);
        app
    }

    /// Apply a newly collected snapshot; returns true if anything changed and a redraw is needed.
    pub fn apply_snapshot(&mut self, mut incoming: AppSnapshot, force_update_time: bool) -> bool {
        let was_loading = self.loading;
        self.loading = false;
        if incoming.line_stats.is_none() && self.snapshot.line_stats.is_some() {
            incoming.line_stats = self.snapshot.line_stats.clone();
        }
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
                let path = self
                    .snapshot
                    .repos
                    .get(*repo_idx)
                    .map(|r| r.path.clone())
                    .unwrap_or_else(|| PathBuf::from(GLOBAL_FOLD_SENTINEL));
                Some(SelectionKey::Fold(path, *kind))
            }
            ListRow::DagQueueEntry { idx } => {
                let q = self.snapshot.dag_queue.get(*idx)?;
                Some(SelectionKey::DagQueue(q.name.clone()))
            }
            ListRow::MergeEntry { idx } => {
                let m = self.snapshot.merges.get(*idx)?;
                Some(SelectionKey::Merge(m.merge_run.clone()))
            }
            _ => None,
        }
    }

    pub fn rebuild_rows(&mut self, preserve: Option<(PathBuf, String)>) {
        self.rebuild_rows_inner(preserve.map(|(p, id)| SelectionKey::Run(p, id)));
    }

    fn is_stale_other_repo(&self, repo: &RepoGroup) -> bool {
        if self.cwd.starts_with(&repo.path) || repo.path.starts_with(&self.cwd) {
            return false;
        }
        if repo
            .runs
            .iter()
            .any(|r| matches!(r.group, GroupKind::Running | GroupKind::Queued))
        {
            return false;
        }
        let newest = repo
            .runs
            .iter()
            .map(RunItem::last_activity_seconds)
            .filter(|&s| s != u64::MAX)
            .min();
        newest.is_some_and(|s| s > STALE_REPO_SECS)
    }

    fn append_repo_rows(
        &self,
        repo_idx: usize,
        repo: &RepoGroup,
        query: &str,
        filtering: bool,
        rows: &mut Vec<ListRow>,
    ) {
        let mut matching_by_group: Vec<(GroupKind, Vec<usize>)> = Vec::new();
        for group in GroupKind::ALL {
            let idxs: Vec<usize> = repo
                .runs
                .iter()
                .enumerate()
                .filter(|(_, r)| r.group == group && r.matches_filter(query))
                .map(|(i, _)| i)
                .collect();
            if !idxs.is_empty() {
                matching_by_group.push((group, idxs));
            }
        }
        if matching_by_group.is_empty() {
            return;
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
                        if !self.show_all && repo.runs[run_idx].is_stale_pending() {
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
                                .filter(|&s| s != u64::MAX)
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
                            .filter(|&s| s != u64::MAX)
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
                GroupKind::Collected | GroupKind::MergedUncleaned | GroupKind::Stopped => {
                    let kind = match group {
                        GroupKind::Collected => CollapseKind::Collected,
                        GroupKind::MergedUncleaned => CollapseKind::MergedUncleaned,
                        _ => CollapseKind::Stopped,
                    };
                    let expanded = self.expanded_folds.contains(&(repo.path.clone(), kind));
                    let newest_ago = idxs
                        .iter()
                        .map(|&i| repo.runs[i].last_activity_seconds())
                        .filter(|&s| s != u64::MAX)
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

    fn build_runs_rows_by_status(
        &self,
        query: &str,
        filtering: bool,
        rows: &mut Vec<ListRow>,
        active_queue: &[usize],
        dispatched_queue: &[usize],
        matching_merges: &[usize],
    ) {
        let global_path = PathBuf::from(GLOBAL_FOLD_SENTINEL);

        let open_work_ids: HashSet<&str> = self
            .snapshot
            .work
            .iter()
            .filter(|w| !w.ephemeral && w.state != "done")
            .map(|w| w.id.as_str())
            .collect();
        let open_work_runs: HashSet<&str> = self
            .snapshot
            .work
            .iter()
            .filter(|w| !w.ephemeral && w.state != "done")
            .flat_map(|w| w.runs.iter().map(String::as_str))
            .collect();

        let mut fresh_pending: Vec<(usize, usize)> = Vec::new();
        let mut stale_pending: Vec<(usize, usize)> = Vec::new();
        let mut running: Vec<(usize, usize)> = Vec::new();
        let mut queued: Vec<(usize, usize)> = Vec::new();
        let mut collected: Vec<(usize, usize)> = Vec::new();
        let mut merged_uncleaned: Vec<(usize, usize)> = Vec::new();
        let mut stopped: Vec<(usize, usize)> = Vec::new();

        for (repo_idx, repo) in self.snapshot.repos.iter().enumerate() {
            for (run_idx, r) in repo.runs.iter().enumerate() {
                if !r.matches_filter(query) {
                    continue;
                }
                match r.group {
                    GroupKind::Pending => {
                        let attached = open_work_runs.contains(r.run.as_str())
                            || r.work_id
                                .as_deref()
                                .is_some_and(|wid| open_work_ids.contains(wid));
                        if !self.show_all && !filtering && r.is_stale_pending() && !attached {
                            stale_pending.push((repo_idx, run_idx));
                        } else {
                            fresh_pending.push((repo_idx, run_idx));
                        }
                    }
                    GroupKind::Running => running.push((repo_idx, run_idx)),
                    GroupKind::Queued => queued.push((repo_idx, run_idx)),
                    GroupKind::Collected => collected.push((repo_idx, run_idx)),
                    GroupKind::MergedUncleaned => merged_uncleaned.push((repo_idx, run_idx)),
                    GroupKind::Stopped => stopped.push((repo_idx, run_idx)),
                }
            }
        }

        // Sort each bucket cross-repo: urgent/newest first.
        fresh_pending.sort_by(|&(ra, ia), &(rb, ib)| {
            let a = &self.snapshot.repos[ra].runs[ia];
            let b = &self.snapshot.repos[rb].runs[ib];
            b.is_urgent_pending()
                .cmp(&a.is_urgent_pending())
                .then_with(|| a.last_activity_seconds().cmp(&b.last_activity_seconds()))
                .then_with(|| b.run.cmp(&a.run))
        });
        stale_pending.sort_by(|&(ra, ia), &(rb, ib)| {
            let a = &self.snapshot.repos[ra].runs[ia];
            let b = &self.snapshot.repos[rb].runs[ib];
            a.last_activity_seconds()
                .cmp(&b.last_activity_seconds())
                .then_with(|| b.run.cmp(&a.run))
        });
        running.sort_by(|&(ra, ia), &(rb, ib)| {
            let a = &self.snapshot.repos[ra].runs[ia];
            let b = &self.snapshot.repos[rb].runs[ib];
            a.idle_seconds
                .unwrap_or(u64::MAX)
                .cmp(&b.idle_seconds.unwrap_or(u64::MAX))
                .then_with(|| b.run.cmp(&a.run))
        });
        queued.sort_by(|&(ra, ia), &(rb, ib)| {
            let a = &self.snapshot.repos[ra].runs[ia];
            let b = &self.snapshot.repos[rb].runs[ib];
            a.age_seconds
                .unwrap_or(u64::MAX)
                .cmp(&b.age_seconds.unwrap_or(u64::MAX))
                .then_with(|| b.run.cmp(&a.run))
        });
        for list in [&mut collected, &mut merged_uncleaned, &mut stopped] {
            list.sort_by(|&(ra, ia), &(rb, ib)| {
                let a = &self.snapshot.repos[ra].runs[ia];
                let b = &self.snapshot.repos[rb].runs[ib];
                a.last_activity_seconds()
                    .cmp(&b.last_activity_seconds())
                    .then_with(|| b.run.cmp(&a.run))
            });
        }

        // 1. 待主控处理
        if !fresh_pending.is_empty() || !stale_pending.is_empty() {
            if !fresh_pending.is_empty() {
                rows.push(ListRow::GroupHeader {
                    repo_idx: usize::MAX,
                    group: GroupKind::Pending,
                    count: fresh_pending.len(),
                });
                for (repo_idx, run_idx) in fresh_pending {
                    rows.push(ListRow::Run { repo_idx, run_idx });
                }
            }
            if !stale_pending.is_empty() {
                let expanded = self
                    .expanded_folds
                    .contains(&(global_path.clone(), CollapseKind::StalePending));
                let newest_ago = stale_pending
                    .iter()
                    .map(|&(ri, rj)| self.snapshot.repos[ri].runs[rj].last_activity_seconds())
                    .filter(|&s| s != u64::MAX)
                    .min();
                rows.push(ListRow::ToggleFold {
                    repo_idx: usize::MAX,
                    kind: CollapseKind::StalePending,
                    count: stale_pending.len(),
                    newest_ago_seconds: newest_ago,
                    expanded,
                    standalone: rows.is_empty(),
                });
                if expanded {
                    for (repo_idx, run_idx) in stale_pending {
                        rows.push(ListRow::Run { repo_idx, run_idx });
                    }
                }
            }
        }

        // 2. 运行与返工
        if !running.is_empty() {
            rows.push(ListRow::GroupHeader {
                repo_idx: usize::MAX,
                group: GroupKind::Running,
                count: running.len(),
            });
            for (repo_idx, run_idx) in running {
                rows.push(ListRow::Run { repo_idx, run_idx });
            }
        }

        // 3. 排队与待续派 (hidden when 0; dispatched items hidden unless filtering / show_all / expanded)
        let queue_total = queued.len() + active_queue.len();
        if queue_total > 0 {
            rows.push(ListRow::GroupHeader {
                repo_idx: usize::MAX,
                group: GroupKind::Queued,
                count: queue_total,
            });
            for (repo_idx, run_idx) in queued {
                rows.push(ListRow::Run { repo_idx, run_idx });
            }
            for &idx in active_queue {
                rows.push(ListRow::DagQueueEntry { idx });
            }
        }
        if !dispatched_queue.is_empty() {
            let show_dispatched_directly = self.show_all || filtering;
            let dispatched_expanded = show_dispatched_directly
                || self
                    .expanded_folds
                    .contains(&(global_path.clone(), CollapseKind::DispatchedQueue));
            if show_dispatched_directly {
                for &idx in dispatched_queue {
                    rows.push(ListRow::DagQueueEntry { idx });
                }
            } else {
                rows.push(ListRow::ToggleFold {
                    repo_idx: usize::MAX,
                    kind: CollapseKind::DispatchedQueue,
                    count: dispatched_queue.len(),
                    newest_ago_seconds: None,
                    expanded: dispatched_expanded,
                    standalone: queue_total == 0,
                });
                if dispatched_expanded {
                    for &idx in dispatched_queue {
                        rows.push(ListRow::DagQueueEntry { idx });
                    }
                }
            }
        }

        // 4. 合入批次 (hidden when 0)
        if !matching_merges.is_empty() {
            let expanded = filtering
                || self
                    .expanded_folds
                    .contains(&(global_path.clone(), CollapseKind::Merges));
            rows.push(ListRow::ToggleFold {
                repo_idx: usize::MAX,
                kind: CollapseKind::Merges,
                count: matching_merges.len(),
                newest_ago_seconds: None,
                expanded,
                standalone: true,
            });
            if expanded {
                for &idx in matching_merges {
                    rows.push(ListRow::MergeEntry { idx });
                }
            }
        }

        // 5. 近期已收取 / 已合未清 / 已停止 (folded at bottom)
        for (kind, list) in [
            (CollapseKind::Collected, collected),
            (CollapseKind::MergedUncleaned, merged_uncleaned),
            (CollapseKind::Stopped, stopped),
        ] {
            if list.is_empty() {
                continue;
            }
            let expanded = filtering
                || self
                    .expanded_folds
                    .contains(&(global_path.clone(), kind));
            let newest_ago = list
                .iter()
                .map(|&(ri, rj)| self.snapshot.repos[ri].runs[rj].last_activity_seconds())
                .filter(|&s| s != u64::MAX)
                .min();
            rows.push(ListRow::ToggleFold {
                repo_idx: usize::MAX,
                kind,
                count: list.len(),
                newest_ago_seconds: newest_ago,
                expanded,
                standalone: true,
            });
            if expanded {
                for (repo_idx, run_idx) in list {
                    rows.push(ListRow::Run { repo_idx, run_idx });
                }
            }
        }
    }

    fn rebuild_rows_inner(&mut self, preserve: Option<SelectionKey>) {
        let query = self.filter.trim().to_lowercase();
        let filtering = !query.is_empty();
        let mut rows = Vec::new();
        let global_path = PathBuf::from(GLOBAL_FOLD_SENTINEL);

        let mut active_queue: Vec<usize> = Vec::new();
        let mut dispatched_queue: Vec<usize> = Vec::new();
        for (i, q) in self.snapshot.dag_queue.iter().enumerate() {
            if q.status == "cancelled" || !q.matches_filter(&query) {
                continue;
            }
            if q.status == "dispatched" {
                dispatched_queue.push(i);
            } else {
                active_queue.push(i);
            }
        }
        let matching_merges: Vec<usize> = self
            .snapshot
            .merges
            .iter()
            .enumerate()
            .filter(|(_, m)| m.matches_filter(&query))
            .map(|(i, _)| i)
            .collect();

        if self.runs_group_mode == RunsGroupMode::ByStatus {
            self.build_runs_rows_by_status(
                &query,
                filtering,
                &mut rows,
                &active_queue,
                &dispatched_queue,
                &matching_merges,
            );
        } else {
            let mut stale_repo_indices: Vec<usize> = Vec::new();
            for (repo_idx, repo) in self.snapshot.repos.iter().enumerate() {
                if !self.show_all && !filtering && self.is_stale_other_repo(repo) {
                    stale_repo_indices.push(repo_idx);
                    continue;
                }
                self.append_repo_rows(repo_idx, repo, &query, filtering, &mut rows);
            }

            if !active_queue.is_empty() || !dispatched_queue.is_empty() {
                let show_dispatched_directly = self.show_all || filtering;
                let dispatched_expanded = show_dispatched_directly
                    || self
                        .expanded_folds
                        .contains(&(global_path.clone(), CollapseKind::DispatchedQueue));
                let header_count = if show_dispatched_directly || dispatched_expanded {
                    active_queue.len() + dispatched_queue.len()
                } else {
                    active_queue.len()
                };
                if !active_queue.is_empty() || show_dispatched_directly {
                    rows.push(ListRow::DagQueueHeader {
                        count: header_count,
                    });
                    for idx in &active_queue {
                        rows.push(ListRow::DagQueueEntry { idx: *idx });
                    }
                }
                if !dispatched_queue.is_empty() {
                    if show_dispatched_directly {
                        for idx in dispatched_queue {
                            rows.push(ListRow::DagQueueEntry { idx });
                        }
                    } else {
                        rows.push(ListRow::ToggleFold {
                            repo_idx: usize::MAX,
                            kind: CollapseKind::DispatchedQueue,
                            count: dispatched_queue.len(),
                            newest_ago_seconds: None,
                            expanded: dispatched_expanded,
                            standalone: active_queue.is_empty(),
                        });
                        if dispatched_expanded {
                            for idx in dispatched_queue {
                                rows.push(ListRow::DagQueueEntry { idx });
                            }
                        }
                    }
                }
            }

            if !matching_merges.is_empty() {
                let expanded = filtering
                    || self
                        .expanded_folds
                        .contains(&(global_path.clone(), CollapseKind::Merges));
                rows.push(ListRow::ToggleFold {
                    repo_idx: usize::MAX,
                    kind: CollapseKind::Merges,
                    count: matching_merges.len(),
                    newest_ago_seconds: None,
                    expanded,
                    standalone: true,
                });
                if expanded {
                    for &idx in &matching_merges {
                        rows.push(ListRow::MergeEntry { idx });
                    }
                }
            }

            if !stale_repo_indices.is_empty() {
                let expanded = self
                    .expanded_folds
                    .contains(&(global_path, CollapseKind::StaleRepos));
                let newest_ago = stale_repo_indices
                    .iter()
                    .flat_map(|&ri| self.snapshot.repos[ri].runs.iter())
                    .map(RunItem::last_activity_seconds)
                    .filter(|&s| s != u64::MAX)
                    .min();
                rows.push(ListRow::ToggleFold {
                    repo_idx: usize::MAX,
                    kind: CollapseKind::StaleRepos,
                    count: stale_repo_indices.len(),
                    newest_ago_seconds: newest_ago,
                    expanded,
                    standalone: true,
                });
                if expanded {
                    for repo_idx in stale_repo_indices {
                        self.append_repo_rows(
                            repo_idx,
                            &self.snapshot.repos[repo_idx],
                            &query,
                            filtering,
                            &mut rows,
                        );
                    }
                }
            }
        }
        self.rows = rows;

        // Rebuild filtered events list and keep follow cursor synced.
        // Hide noisy `lane_acquired`/`lane_released` events by default unless toggled on (`l`)
        // or explicitly searched for in `/`.
        let prev_event_seq = self.selected_event_item().map(|e| e.seq);
        let include_lane = self.show_lane_events
            || query.contains("lane")
            || query.contains("占槽")
            || query.contains("放槽");
        let mut hidden_lane = 0usize;
        self.filtered_events = self
            .snapshot
            .events
            .iter()
            .enumerate()
            .filter(|(_, ev)| {
                let is_lane = matches!(ev.kind.as_str(), "lane_acquired" | "lane_released");
                if is_lane && !include_lane {
                    hidden_lane += 1;
                    return false;
                }
                ev.matches_filter(&query)
            })
            .map(|(i, _)| i)
            .collect();
        self.hidden_lane_events = hidden_lane;
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
                    ) if self
                        .snapshot
                        .repos
                        .get(*repo_idx)
                        .map(|r| r.path.as_path())
                        .unwrap_or_else(|| Path::new(GLOBAL_FOLD_SENTINEL))
                        == prev_repo.as_path()
                        && kind == prev_kind =>
                    {
                        next_sel = Some(row_idx);
                        break;
                    }
                    (SelectionKey::DagQueue(prev_name), ListRow::DagQueueEntry { idx })
                        if &self.snapshot.dag_queue[*idx].name == prev_name =>
                    {
                        next_sel = Some(row_idx);
                        break;
                    }
                    (SelectionKey::Merge(prev_id), ListRow::MergeEntry { idx })
                        if &self.snapshot.merges[*idx].merge_run == prev_id =>
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

    pub fn selected_work_candidate(&self) -> Option<&RadarCandidateItem> {
        let row_idx = self.selected_work_row?;
        match self.work_rows.get(row_idx)? {
            WorkRow::Candidate { cand_idx } => self.snapshot.candidates.get(*cand_idx),
            _ => None,
        }
    }

    fn current_work_selection_id(&self) -> Option<String> {
        let row_idx = self.selected_work_row?;
        match self.work_rows.get(row_idx)? {
            WorkRow::Item { work_idx, .. } => {
                self.snapshot.work.get(*work_idx).map(|w| w.id.clone())
            }
            WorkRow::Candidate { cand_idx } => self
                .snapshot
                .candidates
                .get(*cand_idx)
                .map(|c| format!("__cand__{}", c.id)),
            WorkRow::FoldToggle { kind, .. } => Some(match kind {
                WorkFoldKind::Deferred => "__fold_deferred__".to_string(),
                WorkFoldKind::Candidates => "__fold_candidates__".to_string(),
                WorkFoldKind::Done => "__fold_done__".to_string(),
                WorkFoldKind::Dropped => "__fold_dropped__".to_string(),
            }),
            WorkRow::GroupHeader { .. } => None,
        }
    }

    /// Rebuild the L1 Work View row list with status grouping, owner pinning,
    /// dependency chain indentation (`work_closed` / `run_applied`), own-bucket counts (never
    /// absorbing attached downstream waiting items into parent bucket headers), and folded
    /// deferred/candidates/done/dropped sections.
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
                    own_count: dropped.len(),
                    attached_downstream: 0,
                    ephemeral_count: 0,
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

        // Partition visible indices into Owner, Engineering Active/Timed, Deferred, Done, and Dropped.
        let mut owner_open: Vec<usize> = Vec::new();
        let mut eng_open: Vec<usize> = Vec::new();
        let mut deferred_items: Vec<usize> = Vec::new();
        let mut done_items: Vec<usize> = Vec::new();
        let mut dropped_items: Vec<usize> = Vec::new();

        for idx in visible_indices {
            let w = &self.snapshot.work[idx];
            match w.bucket() {
                WorkBucket::Owner => owner_open.push(idx),
                WorkBucket::Deferred => deferred_items.push(idx),
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
                own_count: owner_open.len(),
                attached_downstream: 0,
                ephemeral_count: 0,
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
                        if let Some(pidx) =
                            find_work_by_run_ref(&self.snapshot.work, &eng_open, run_ref)
                        {
                            if pidx != idx {
                                first_parent = Some(pidx);
                            }
                        }
                    }
                } else if cond.starts_with("owner:")
                    || cond.starts_with("run_delivered:")
                    || cond.starts_with("after_time:")
                {
                    dep_count += 1;
                }
            }
            if let Some(pidx) = first_parent {
                let p_bucket = self.snapshot.work[pidx].bucket();
                let c_bucket = w.bucket();
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
            WorkBucket::TimedWait,
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
            let header_idx = rows.len();
            rows.push(WorkRow::GroupHeader {
                bucket,
                own_count: 0,
                attached_downstream: 0,
                ephemeral_count: 0,
            });
            let start_row = rows.len();
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
            // Count ONLY items belonging to `bucket` in `own_count`, and track attached
            // downstream items from other buckets separately (`阻下游 K`) so header counts
            // never contradict the top bar or `delegate work` CLI.
            let mut own = 0usize;
            let mut downstream = 0usize;
            let mut eph = 0usize;
            for r in &rows[start_row..] {
                if let WorkRow::Item { work_idx, .. } = r {
                    let w = &self.snapshot.work[*work_idx];
                    if w.bucket() == bucket {
                        own += 1;
                        if w.ephemeral {
                            eph += 1;
                        }
                    } else {
                        downstream += 1;
                    }
                }
            }
            if let Some(WorkRow::GroupHeader {
                own_count,
                attached_downstream,
                ephemeral_count,
                ..
            }) = rows.get_mut(header_idx)
            {
                *own_count = own;
                *attached_downstream = downstream;
                *ephemeral_count = eph;
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

        // 3. Folded Deferred (暂缓) section — collapsed by default so it stays out of active chain focus
        deferred_items.sort_by(|&a, &b| {
            let wa = &self.snapshot.work[a];
            let wb = &self.snapshot.work[b];
            wa.defer_until
                .cmp(&wb.defer_until)
                .then_with(|| wa.id.cmp(&wb.id))
        });
        if !deferred_items.is_empty() {
            let expanded = self.work_deferred_expanded || filtering;
            rows.push(WorkRow::FoldToggle {
                kind: WorkFoldKind::Deferred,
                count: deferred_items.len(),
                expanded,
            });
            if expanded {
                for idx in deferred_items {
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

        // 4. Folded Radar Candidates (侦察候选) section — completely hidden when 0!
        if mode == WorkFilterMode::All && !self.filtered_candidates.is_empty() {
            let expanded = self.work_candidates_expanded || filtering;
            rows.push(WorkRow::FoldToggle {
                kind: WorkFoldKind::Candidates,
                count: self.filtered_candidates.len(),
                expanded,
            });
            if expanded {
                for &cand_idx in &self.filtered_candidates {
                    rows.push(WorkRow::Candidate { cand_idx });
                }
            }
        }

        // 5. Folded Done section (newest first; expanded when text-filtering or explicitly toggled)
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

        // 6. Folded Dropped (已否) section
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
                    WorkRow::Candidate { cand_idx }
                        if format!("__cand__{}", self.snapshot.candidates[*cand_idx].id) == *key =>
                    {
                        next_sel = Some(i);
                        break;
                    }
                    WorkRow::FoldToggle {
                        kind: WorkFoldKind::Deferred,
                        ..
                    } if key == "__fold_deferred__" => {
                        next_sel = Some(i);
                        break;
                    }
                    WorkRow::FoldToggle {
                        kind: WorkFoldKind::Candidates,
                        ..
                    } if key == "__fold_candidates__" => {
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
                .position(|r| matches!(r, WorkRow::Item { .. } | WorkRow::Candidate { .. }))
                .or_else(|| self.work_rows.iter().position(WorkRow::is_selectable));
        }
        if next_sel != self.selected_work_row {
            self.selected_work_row = next_sel;
            self.detail_scroll = 0;
        }
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
            WorkFoldKind::Deferred => {
                self.work_deferred_expanded = !self.work_deferred_expanded;
                "__fold_deferred__".to_string()
            }
            WorkFoldKind::Candidates => {
                self.work_candidates_expanded = !self.work_candidates_expanded;
                "__fold_candidates__".to_string()
            }
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

    /// Jump directly to a Run by ID in `3 运行`, expanding its fold if collapsed (in either
    /// `ByStatus` or `ByRepo` mode).
    pub fn jump_to_run_by_id(&mut self, run_id: &str) -> bool {
        let Some((repo_idx, run_idx, run_ref)) = self.snapshot.find_run_by_id(run_id) else {
            return false;
        };
        let target_repo = self.snapshot.repos[repo_idx].path.clone();
        let target_run_id = run_ref.run.clone();
        let target_group = run_ref.group;
        let is_stale = run_ref.is_stale_pending();
        if !self.filter.is_empty() && !run_ref.matches_filter(&self.filter.trim().to_lowercase()) {
            self.filter.clear();
        }
        let fold_scope = if self.runs_group_mode == RunsGroupMode::ByStatus {
            PathBuf::from(GLOBAL_FOLD_SENTINEL)
        } else {
            target_repo.clone()
        };
        match target_group {
            GroupKind::Collected => {
                self.expanded_folds
                    .insert((fold_scope, CollapseKind::Collected));
            }
            GroupKind::MergedUncleaned => {
                self.expanded_folds
                    .insert((fold_scope, CollapseKind::MergedUncleaned));
            }
            GroupKind::Stopped => {
                self.expanded_folds
                    .insert((fold_scope, CollapseKind::Stopped));
            }
            GroupKind::Pending if is_stale => {
                self.expanded_folds
                    .insert((fold_scope, CollapseKind::StalePending));
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
        let Some((_, _, run_ref)) = self.snapshot.find_run_for_work(&item) else {
            if let Some(last_rid) = item.runs.last() {
                self.set_toast(format!("关联 run 已清理（delegate show {last_rid}）"));
            } else {
                self.set_toast("该工作项暂无关联 run");
            }
            return false;
        };
        let target_run_id = run_ref.run.clone();
        self.jump_to_run_by_id(&target_run_id)
    }

    /// Jump to a specific work item ID in the Work tab (expanding folds if needed).
    pub fn jump_to_work_id(&mut self, work_id: &str) -> bool {
        let Some(item) = self.snapshot.work_item_by_id(work_id).cloned() else {
            return false;
        };
        if item.state == "deferred" {
            self.work_deferred_expanded = true;
        } else if item.state == "done" {
            if item.closed_kind == "dropped" {
                self.work_dropped_expanded = true;
            } else {
                self.work_done_expanded = true;
            }
        }
        self.work_filter_mode = WorkFilterMode::All;
        self.rebuild_work_rows(Some(work_id.to_string()));
        self.tab = ViewTab::Work;
        self.focus = FocusPane::List;
        self.detail_scroll = 0;
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
        let path = self
            .snapshot
            .repos
            .get(repo_idx)
            .map(|r| r.path.clone())
            .unwrap_or_else(|| PathBuf::from(GLOBAL_FOLD_SENTINEL));
        let key = (path.clone(), kind);
        if !self.expanded_folds.remove(&key) {
            self.expanded_folds.insert(key);
        }
        self.rebuild_rows_inner(Some(SelectionKey::Fold(path, kind)));
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

    /// Selected run indices `(repo_idx, run_idx)` or fallback to newest run so right pane stays populated.
    pub fn preview_run_indices(&self) -> Option<(usize, usize)> {
        if let Some(row_idx) = self.selected_row {
            match self.rows.get(row_idx) {
                Some(ListRow::Run { repo_idx, run_idx }) => {
                    return Some((*repo_idx, *run_idx));
                }
                Some(ListRow::MergeEntry { .. } | ListRow::DagQueueEntry { .. }) => {
                    return None;
                }
                _ => {}
            }
        }
        // Fallback when no active run is directly selected (e.g. cursor on collapsed fold):
        // pick the newest Collected or MergedUncleaned (or any) run across repos.
        let mut best: Option<(u64, usize, usize)> = None;
        for (ri, repo) in self.snapshot.repos.iter().enumerate() {
            for (rj, r) in repo.runs.iter().enumerate() {
                let ago = r.last_activity_seconds();
                if best.is_none_or(|(b_ago, _, _)| ago < b_ago) {
                    best = Some((ago, ri, rj));
                }
            }
        }
        best.map(|(_, ri, rj)| (ri, rj))
    }

    /// On-demand read + cache of `<runsDir>/<run>/card.json` for a single run (zero snapshot bloat).
    pub fn get_or_load_run_card(&mut self, repo_idx: usize, run_idx: usize) -> Option<&RunCardL1> {
        let repo = self.snapshot.repos.get(repo_idx)?;
        let run = repo.runs.get(run_idx)?;
        let key = run.cache_key();
        if !self.card_cache.contains_key(&key) {
            let runs_dir = repo.effective_runs_dir();
            let loaded = crate::data::read_run_card_json(&runs_dir, &run.run);
            self.card_cache.insert(key.clone(), loaded);
        }
        self.card_cache.get(&key).and_then(|o| o.as_ref())
    }

    pub fn selected_merge_row(&self) -> Option<&MergeRunItem> {
        let row_idx = self.selected_row?;
        match self.rows.get(row_idx)? {
            ListRow::MergeEntry { idx } => self.snapshot.merges.get(*idx),
            _ => None,
        }
    }

    /// Interactive entries for `1 总览` card ① (需要关注).
    pub fn overview_attention_entries(&self) -> Vec<OverviewAttentionEntry> {
        let mut out = Vec::new();
        // Owner open items first
        let mut owners: Vec<usize> = self
            .snapshot
            .work
            .iter()
            .enumerate()
            .filter(|(_, w)| !w.ephemeral && w.state != "done" && w.owner.is_some())
            .map(|(i, _)| i)
            .collect();
        owners.sort_by(|&a, &b| {
            let wa = &self.snapshot.work[a];
            let wb = &self.snapshot.work[b];
            wa.owner
                .as_ref()
                .map(|o| o.kind)
                .cmp(&wb.owner.as_ref().map(|o| o.kind))
                .then_with(|| wa.id.cmp(&wb.id))
        });
        for work_idx in owners {
            out.push(OverviewAttentionEntry::OwnerWork { work_idx });
        }

        // Fresh pending runs across repos (stale >24h without open work folded into StaleFold)
        let open_work_ids: HashSet<&str> = self
            .snapshot
            .work
            .iter()
            .filter(|w| !w.ephemeral && w.state != "done")
            .map(|w| w.id.as_str())
            .collect();
        let open_work_runs: HashSet<&str> = self
            .snapshot
            .work
            .iter()
            .filter(|w| !w.ephemeral && w.state != "done")
            .flat_map(|w| w.runs.iter().map(String::as_str))
            .collect();

        let mut fresh: Vec<(usize, usize)> = Vec::new();
        let mut stale: Vec<(usize, usize)> = Vec::new();
        for (ri, repo) in self.snapshot.repos.iter().enumerate() {
            for (rj, r) in repo.runs.iter().enumerate() {
                if r.group != GroupKind::Pending {
                    continue;
                }
                let attached = open_work_runs.contains(r.run.as_str())
                    || r.work_id
                        .as_deref()
                        .is_some_and(|wid| open_work_ids.contains(wid));
                if r.is_stale_pending() && !attached {
                    stale.push((ri, rj));
                } else {
                    fresh.push((ri, rj));
                }
            }
        }
        fresh.sort_by(|&(ra, ia), &(rb, ib)| {
            let a = &self.snapshot.repos[ra].runs[ia];
            let b = &self.snapshot.repos[rb].runs[ib];
            b.is_urgent_pending()
                .cmp(&a.is_urgent_pending())
                .then_with(|| a.last_activity_seconds().cmp(&b.last_activity_seconds()))
        });
        for (repo_idx, run_idx) in fresh {
            out.push(OverviewAttentionEntry::PendingRun { repo_idx, run_idx });
        }
        if !stale.is_empty() {
            out.push(OverviewAttentionEntry::StaleFold { count: stale.len() });
            if self.overview_stale_expanded {
                stale.sort_by(|&(ra, ia), &(rb, ib)| {
                    self.snapshot.repos[ra].runs[ia]
                        .last_activity_seconds()
                        .cmp(&self.snapshot.repos[rb].runs[ib].last_activity_seconds())
                });
                for (repo_idx, run_idx) in stale {
                    out.push(OverviewAttentionEntry::PendingRun { repo_idx, run_idx });
                }
            }
        }
        out
    }

    /// Interactive entries for `1 总览` card ② (正在运行与波次预估).
    pub fn overview_running_entries(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (ri, repo) in self.snapshot.repos.iter().enumerate() {
            for (rj, r) in repo.runs.iter().enumerate() {
                if matches!(r.group, GroupKind::Running | GroupKind::Queued) {
                    out.push((ri, rj));
                }
            }
        }
        out.sort_by(|&(ra, ia), &(rb, ib)| {
            let a = &self.snapshot.repos[ra].runs[ia];
            let b = &self.snapshot.repos[rb].runs[ib];
            a.group
                .cmp(&b.group)
                .then_with(|| a.idle_seconds.unwrap_or(u64::MAX).cmp(&b.idle_seconds.unwrap_or(u64::MAX)))
                .then_with(|| b.run.cmp(&a.run))
        });
        out
    }

    /// Interactive active chain rows for `1 总览` card ③ (工作表推进链): active pool items
    /// (excluding TimedWait, Deferred, Done, Dropped, and Candidates).
    pub fn overview_work_chain_rows(&self) -> Vec<WorkRow> {
        let mut out = Vec::new();
        for row in &self.work_rows {
            if let WorkRow::Item { work_idx, .. } = row {
                let b = self.snapshot.work[*work_idx].bucket();
                if matches!(
                    b,
                    WorkBucket::Owner
                        | WorkBucket::NeedsAction
                        | WorkBucket::InProgress
                        | WorkBucket::Ready
                        | WorkBucket::Waiting
                ) {
                    out.push(row.clone());
                }
            }
        }
        out
    }

    /// Recent deliveries for `1 总览` card ⑤ (最近交付).
    pub fn overview_delivery_entries(&self) -> Vec<RecentDeliveryItem> {
        self.snapshot.effective_recent_deliveries()
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
        // Unified 5-tab navigation: `1`..=`5` and `[` / `]`. Old `0` & `O/w/A/e/m/b` switches removed.
        let switch_tab = |app: &mut App, next: ViewTab| -> AppAction {
            if app.tab != next {
                app.tab = next;
                app.focus = FocusPane::List;
                app.detail_scroll = 0;
                app.dirty = true;
                if next == ViewTab::Runs && app.needs_selected_result() {
                    return AppAction::LoadSelectedResult;
                }
                return AppAction::Redraw;
            }
            AppAction::None
        };
        match key.code {
            KeyCode::Char('1') => return switch_tab(self, ViewTab::Overview),
            KeyCode::Char('2') => return switch_tab(self, ViewTab::Work),
            KeyCode::Char('3') => return switch_tab(self, ViewTab::Runs),
            KeyCode::Char('4') => return switch_tab(self, ViewTab::Quota),
            KeyCode::Char('5') => return switch_tab(self, ViewTab::Events),
            KeyCode::Char('[') => {
                let prev = self.tab.prev();
                return switch_tab(self, prev);
            }
            KeyCode::Char(']') => {
                let next = self.tab.next();
                return switch_tab(self, next);
            }
            _ => {}
        }

        if self.tab == ViewTab::Overview {
            return self.handle_overview_key(key);
        }

        if self.tab == ViewTab::Quota {
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
                // Enter is pure read-only focus shift to detail; NEVER triggers account switch.
                KeyCode::Enter => {
                    self.focus = match self.focus {
                        FocusPane::List => FocusPane::Detail,
                        FocusPane::Detail => FocusPane::List,
                    };
                    self.dirty = true;
                    AppAction::Redraw
                }
                // `u` is the sole manual account-switch confirmation trigger.
                KeyCode::Char('u') => {
                    self.open_account_switch_confirm();
                    AppAction::Redraw
                }
                KeyCode::Char(' ') => {
                    self.quota_models_expanded = !self.quota_models_expanded;
                    self.dirty = true;
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
                        if self.move_work_selection(1) {
                            self.detail_scroll = 0;
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
                        if self.move_work_selection(-1) {
                            self.detail_scroll = 0;
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
                        if self.move_work_selection(10) {
                            self.detail_scroll = 0;
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
                        if self.move_work_selection(-10) {
                            self.detail_scroll = 0;
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
                        let first = self.work_rows.iter().position(WorkRow::is_selectable);
                        if first != self.selected_work_row {
                            self.selected_work_row = first;
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
                        let last = self.work_rows.iter().rposition(WorkRow::is_selectable);
                        if last != self.selected_work_row {
                            self.selected_work_row = last;
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
                KeyCode::Enter => {
                    if self.toggle_selected_work_fold() {
                        AppAction::Redraw
                    } else if self.last_terminal_width < 110 {
                        if let Some(w) = self.selected_work_item().cloned() {
                            self.overlay = Overlay::WorkCard {
                                work_id: w.id,
                                scroll: 0,
                            };
                            self.dirty = true;
                            AppAction::Redraw
                        } else {
                            AppAction::None
                        }
                    } else {
                        self.focus = match self.focus {
                            FocusPane::List => FocusPane::Detail,
                            FocusPane::Detail => FocusPane::List,
                        };
                        self.dirty = true;
                        AppAction::Redraw
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
                    self.detail_scroll = 0;
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
                    self.detail_scroll = 0;
                    self.rebuild_work_rows(None);
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::Char('p') => {
                    if let Some(c) = self.selected_work_candidate().cloned() {
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
                            note: String::new(),
                        };
                        self.dirty = true;
                        AppAction::Redraw
                    } else {
                        self.set_toast("请先展开并选中一条侦察候选");
                        AppAction::Redraw
                    }
                }
                KeyCode::Char('y') => {
                    if let Some(w) = self.selected_work_item().cloned() {
                        let cmd = build_work_next_command(&self.delegate_bin, &self.cwd, &w);
                        AppAction::CopyClipboard(cmd)
                    } else if let Some(c) = self.selected_work_candidate().cloned() {
                        let cmd = format_repo_cmd(
                            &self.cwd,
                            &c.repo_path,
                            &format!(
                                "{} radar pick {}",
                                shell_quote(&self.delegate_bin.to_string_lossy()),
                                shell_quote(&c.id)
                            ),
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
                    if !self.filter.is_empty() || self.work_filter_mode != WorkFilterMode::All {
                        self.filter.clear();
                        self.work_filter_mode = WorkFilterMode::All;
                        self.rebuild_rows(None);
                        self.dirty = true;
                        AppAction::Redraw
                    } else if self.focus == FocusPane::Detail {
                        self.focus = FocusPane::List;
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
                KeyCode::Char('f') => {
                    self.events_follow = !self.events_follow;
                    if self.events_follow && !self.filtered_events.is_empty() {
                        self.selected_event = Some(self.filtered_events.len() - 1);
                        self.detail_scroll = 0;
                    }
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::Char('l') => {
                    self.show_lane_events = !self.show_lane_events;
                    self.rebuild_rows(None);
                    self.detail_scroll = 0;
                    self.dirty = true;
                    AppAction::Redraw
                }
                KeyCode::Char('t') | KeyCode::Enter => {
                    if let Some(ev) = self.selected_event_item().cloned() {
                        if !ev.run.is_empty() && self.jump_to_run_by_id(&ev.run) {
                            if self.needs_selected_result() {
                                AppAction::LoadSelectedResult
                            } else {
                                AppAction::Redraw
                            }
                        } else if !ev.run.is_empty() {
                            self.set_toast(format!("该运行已清理（delegate show {}）", ev.run));
                            AppAction::Redraw
                        } else {
                            AppAction::None
                        }
                    } else {
                        AppAction::None
                    }
                }
                KeyCode::Char('y') => {
                    if let Some(ev) = self.selected_event_item().cloned() {
                        let cmd = if !ev.run.is_empty() {
                            format!(
                                "{} show {}",
                                shell_quote(&self.delegate_bin.to_string_lossy()),
                                shell_quote(&ev.run)
                            )
                        } else {
                            format!(
                                "{} events --since {}",
                                shell_quote(&self.delegate_bin.to_string_lossy()),
                                ev.seq.saturating_sub(1)
                            )
                        };
                        AppAction::CopyClipboard(cmd)
                    } else {
                        AppAction::None
                    }
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

        // `3 运行` (`ViewTab::Runs`)
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
            KeyCode::Char('v') => {
                let prev = self.current_selection_key();
                self.runs_group_mode = match self.runs_group_mode {
                    RunsGroupMode::ByStatus => RunsGroupMode::ByRepo,
                    RunsGroupMode::ByRepo => RunsGroupMode::ByStatus,
                };
                self.rebuild_rows_inner(prev);
                self.dirty = true;
                AppAction::Redraw
            }
            KeyCode::Char('w') => {
                if let Some(run) = self.selected_run().cloned() {
                    if let Some(wid) = self.snapshot.work_id_for_run(&run) {
                        if self.jump_to_work_id(&wid) {
                            return AppAction::Redraw;
                        }
                    }
                }
                self.set_toast("当前运行未关联工作项");
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
                } else if self.selected_merge_row().is_some() {
                    self.focus = FocusPane::Detail;
                    self.dirty = true;
                    AppAction::Redraw
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
            KeyCode::Char('Q') => AppAction::RefreshQuota,
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
                if let Some(m) = self.selected_merge_row().cloned() {
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
                        "把集成提交链落回源仓库，不推送远端。".to_string()
                    };
                    self.overlay = Overlay::CommandModal {
                        action_label: "合回源仓库 (apply --keep-commits)".to_string(),
                        command: cmd,
                        note,
                    };
                    self.dirty = true;
                } else {
                    self.open_command_modal(WriteAction::Apply);
                }
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
                } else if let Some(m) = self.selected_merge_row().cloned() {
                    let cmd = format_repo_cmd(
                        &self.cwd,
                        &m.repo_path,
                        &format!(
                            "{} apply --keep-commits {}",
                            shell_quote(&self.delegate_bin.to_string_lossy()),
                            shell_quote(&m.merge_run)
                        ),
                    );
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

    fn handle_overview_key(&mut self, key: KeyEvent) -> AppAction {
        let move_sel = |sel: &mut usize, len: usize, delta: isize| -> bool {
            if len == 0 {
                *sel = 0;
                return false;
            }
            let cur = (*sel).min(len - 1);
            let next = if delta >= 0 {
                (cur + delta as usize).min(len - 1)
            } else {
                cur.saturating_sub((-delta) as usize)
            };
            if next != *sel {
                *sel = next;
                true
            } else {
                false
            }
        };
        let is_narrow = self.last_terminal_width < 110;
        match key.code {
            KeyCode::Char('q') => AppAction::Quit,
            KeyCode::Tab => {
                self.overview_focus = self.overview_focus.next();
                self.dirty = true;
                AppAction::Redraw
            }
            KeyCode::BackTab => {
                self.overview_focus = self.overview_focus.prev();
                self.dirty = true;
                AppAction::Redraw
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if is_narrow {
                    self.overview_scroll = self.overview_scroll.saturating_add(1);
                    self.dirty = true;
                    return AppAction::Redraw;
                }
                let changed = match self.overview_focus {
                    OverviewFocus::Attention => {
                        let len = self.overview_attention_entries().len();
                        move_sel(&mut self.overview_attention_sel, len, 1)
                    }
                    OverviewFocus::Running => {
                        let len = self.overview_running_entries().len();
                        move_sel(&mut self.overview_running_sel, len, 1)
                    }
                    OverviewFocus::WorkChains => {
                        let len = self.overview_work_chain_rows().len();
                        move_sel(&mut self.overview_work_sel, len, 1)
                    }
                    OverviewFocus::Deliveries => {
                        let len = self.overview_delivery_entries().len();
                        move_sel(&mut self.overview_deliveries_sel, len, 1)
                    }
                };
                if changed {
                    self.dirty = true;
                    AppAction::Redraw
                } else {
                    AppAction::None
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if is_narrow {
                    self.overview_scroll = self.overview_scroll.saturating_sub(1);
                    self.dirty = true;
                    return AppAction::Redraw;
                }
                let changed = match self.overview_focus {
                    OverviewFocus::Attention => {
                        let len = self.overview_attention_entries().len();
                        move_sel(&mut self.overview_attention_sel, len, -1)
                    }
                    OverviewFocus::Running => {
                        let len = self.overview_running_entries().len();
                        move_sel(&mut self.overview_running_sel, len, -1)
                    }
                    OverviewFocus::WorkChains => {
                        let len = self.overview_work_chain_rows().len();
                        move_sel(&mut self.overview_work_sel, len, -1)
                    }
                    OverviewFocus::Deliveries => {
                        let len = self.overview_delivery_entries().len();
                        move_sel(&mut self.overview_deliveries_sel, len, -1)
                    }
                };
                if changed {
                    self.dirty = true;
                    AppAction::Redraw
                } else {
                    AppAction::None
                }
            }
            KeyCode::PageDown => {
                if is_narrow {
                    self.overview_scroll = self.overview_scroll.saturating_add(8);
                    self.dirty = true;
                    return AppAction::Redraw;
                }
                match self.overview_focus {
                    OverviewFocus::Attention => {
                        let len = self.overview_attention_entries().len();
                        move_sel(&mut self.overview_attention_sel, len, 6);
                    }
                    OverviewFocus::Running => {
                        let len = self.overview_running_entries().len();
                        move_sel(&mut self.overview_running_sel, len, 6);
                    }
                    OverviewFocus::WorkChains => {
                        let len = self.overview_work_chain_rows().len();
                        move_sel(&mut self.overview_work_sel, len, 6);
                    }
                    OverviewFocus::Deliveries => {
                        let len = self.overview_delivery_entries().len();
                        move_sel(&mut self.overview_deliveries_sel, len, 6);
                    }
                }
                self.dirty = true;
                AppAction::Redraw
            }
            KeyCode::PageUp => {
                if is_narrow {
                    self.overview_scroll = self.overview_scroll.saturating_sub(8);
                    self.dirty = true;
                    return AppAction::Redraw;
                }
                match self.overview_focus {
                    OverviewFocus::Attention => {
                        let len = self.overview_attention_entries().len();
                        move_sel(&mut self.overview_attention_sel, len, -6);
                    }
                    OverviewFocus::Running => {
                        let len = self.overview_running_entries().len();
                        move_sel(&mut self.overview_running_sel, len, -6);
                    }
                    OverviewFocus::WorkChains => {
                        let len = self.overview_work_chain_rows().len();
                        move_sel(&mut self.overview_work_sel, len, -6);
                    }
                    OverviewFocus::Deliveries => {
                        let len = self.overview_delivery_entries().len();
                        move_sel(&mut self.overview_deliveries_sel, len, -6);
                    }
                }
                self.dirty = true;
                AppAction::Redraw
            }
            KeyCode::Home | KeyCode::Char('g') => {
                if is_narrow {
                    self.overview_scroll = 0;
                } else {
                    match self.overview_focus {
                        OverviewFocus::Attention => self.overview_attention_sel = 0,
                        OverviewFocus::Running => self.overview_running_sel = 0,
                        OverviewFocus::WorkChains => self.overview_work_sel = 0,
                        OverviewFocus::Deliveries => self.overview_deliveries_sel = 0,
                    }
                }
                self.dirty = true;
                AppAction::Redraw
            }
            KeyCode::End | KeyCode::Char('G') => {
                if is_narrow {
                    self.overview_scroll = usize::MAX / 2;
                } else {
                    match self.overview_focus {
                        OverviewFocus::Attention => {
                            self.overview_attention_sel =
                                self.overview_attention_entries().len().saturating_sub(1);
                        }
                        OverviewFocus::Running => {
                            self.overview_running_sel =
                                self.overview_running_entries().len().saturating_sub(1);
                        }
                        OverviewFocus::WorkChains => {
                            self.overview_work_sel =
                                self.overview_work_chain_rows().len().saturating_sub(1);
                        }
                        OverviewFocus::Deliveries => {
                            self.overview_deliveries_sel =
                                self.overview_delivery_entries().len().saturating_sub(1);
                        }
                    }
                }
                self.dirty = true;
                AppAction::Redraw
            }
            KeyCode::Char(' ') => {
                if self.overview_focus == OverviewFocus::Attention {
                    let entries = self.overview_attention_entries();
                    if let Some(OverviewAttentionEntry::StaleFold { .. }) =
                        entries.get(self.overview_attention_sel)
                    {
                        self.overview_stale_expanded = !self.overview_stale_expanded;
                        self.dirty = true;
                        return AppAction::Redraw;
                    }
                }
                AppAction::None
            }
            KeyCode::Enter => match self.overview_focus {
                OverviewFocus::Attention => {
                    let entries = self.overview_attention_entries();
                    match entries.get(self.overview_attention_sel) {
                        Some(OverviewAttentionEntry::OwnerWork { work_idx }) => {
                            if let Some(w) = self.snapshot.work.get(*work_idx) {
                                let wid = w.id.clone();
                                self.jump_to_work_id(&wid);
                                return AppAction::Redraw;
                            }
                            AppAction::None
                        }
                        Some(OverviewAttentionEntry::PendingRun { repo_idx, run_idx }) => {
                            if let Some(r) = self
                                .snapshot
                                .repos
                                .get(*repo_idx)
                                .and_then(|rp| rp.runs.get(*run_idx))
                            {
                                let rid = r.run.clone();
                                if self.jump_to_run_by_id(&rid) {
                                    return if self.needs_selected_result() {
                                        AppAction::LoadSelectedResult
                                    } else {
                                        AppAction::Redraw
                                    };
                                }
                            }
                            AppAction::None
                        }
                        Some(OverviewAttentionEntry::StaleFold { .. }) => {
                            self.overview_stale_expanded = !self.overview_stale_expanded;
                            self.dirty = true;
                            AppAction::Redraw
                        }
                        None => AppAction::None,
                    }
                }
                OverviewFocus::Running => {
                    let entries = self.overview_running_entries();
                    if let Some(&(ri, rj)) = entries.get(self.overview_running_sel) {
                        if let Some(r) = self.snapshot.repos.get(ri).and_then(|rp| rp.runs.get(rj))
                        {
                            let rid = r.run.clone();
                            if self.jump_to_run_by_id(&rid) {
                                return if self.needs_selected_result() {
                                    AppAction::LoadSelectedResult
                                } else {
                                    AppAction::Redraw
                                };
                            }
                        }
                    }
                    AppAction::None
                }
                OverviewFocus::WorkChains => {
                    let rows = self.overview_work_chain_rows();
                    if let Some(WorkRow::Item { work_idx, .. }) = rows.get(self.overview_work_sel) {
                        if let Some(w) = self.snapshot.work.get(*work_idx) {
                            let wid = w.id.clone();
                            self.jump_to_work_id(&wid);
                            return AppAction::Redraw;
                        }
                    }
                    AppAction::None
                }
                OverviewFocus::Deliveries => {
                    let deliveries = self.overview_delivery_entries();
                    if let Some(d) = deliveries.get(self.overview_deliveries_sel) {
                        let when = format_iso_local_short(&d.applied_at, &self.snapshot.today_ymd)
                            .unwrap_or_else(|| d.applied_at.clone());
                        let mut body = format!(
                            "运行: {}\n仓库: {}\n合入时刻: {}{}\n改动: {} 文件 (+{}/-{}) · 耗时 {} · {} 轮\n\n",
                            d.run,
                            d.repo,
                            when,
                            if d.applied_head.is_empty() {
                                String::new()
                            } else {
                                format!(" · 提交 {}", d.applied_head)
                            },
                            d.changes_files,
                            d.added,
                            d.deleted,
                            format_duration(Some(d.elapsed_seconds.max(0) as u64)),
                            d.rounds.max(1)
                        );
                        if d.conclusion.trim().is_empty() {
                            body.push_str(&format!(
                                "（历史记录无摘要，完整全文请运行：delegate show {}）",
                                d.run
                            ));
                        } else {
                            body.push_str(&d.conclusion);
                        }
                        self.overlay = Overlay::FullResult {
                            run_id: d.run.clone(),
                            title: format!("最近交付 · {}", d.name),
                            content: body,
                            scroll: 0,
                        };
                        self.dirty = true;
                        AppAction::Redraw
                    } else {
                        AppAction::None
                    }
                }
            },
            KeyCode::Char('y') => match self.overview_focus {
                OverviewFocus::Deliveries => {
                    let deliveries = self.overview_delivery_entries();
                    if let Some(d) = deliveries.get(self.overview_deliveries_sel) {
                        let cmd = format!(
                            "{} show {}",
                            shell_quote(&self.delegate_bin.to_string_lossy()),
                            shell_quote(&d.run)
                        );
                        AppAction::CopyClipboard(cmd)
                    } else {
                        AppAction::None
                    }
                }
                OverviewFocus::Attention => {
                    let entries = self.overview_attention_entries();
                    match entries.get(self.overview_attention_sel) {
                        Some(OverviewAttentionEntry::OwnerWork { work_idx }) => {
                            if let Some(w) = self.snapshot.work.get(*work_idx) {
                                let cmd = build_work_next_command(&self.delegate_bin, &self.cwd, w);
                                AppAction::CopyClipboard(cmd)
                            } else {
                                AppAction::None
                            }
                        }
                        Some(OverviewAttentionEntry::PendingRun { repo_idx, run_idx }) => {
                            if let Some(r) = self
                                .snapshot
                                .repos
                                .get(*repo_idx)
                                .and_then(|rp| rp.runs.get(*run_idx))
                            {
                                let action = recommended_action(r);
                                let (cmd, _) =
                                    build_write_command(&self.delegate_bin, &self.cwd, r, action);
                                AppAction::CopyClipboard(cmd)
                            } else {
                                AppAction::None
                            }
                        }
                        _ => AppAction::None,
                    }
                }
                OverviewFocus::WorkChains => {
                    let rows = self.overview_work_chain_rows();
                    if let Some(WorkRow::Item { work_idx, .. }) = rows.get(self.overview_work_sel) {
                        if let Some(w) = self.snapshot.work.get(*work_idx) {
                            let cmd = build_work_next_command(&self.delegate_bin, &self.cwd, w);
                            return AppAction::CopyClipboard(cmd);
                        }
                    }
                    AppAction::None
                }
                OverviewFocus::Running => {
                    let entries = self.overview_running_entries();
                    if let Some(&(ri, rj)) = entries.get(self.overview_running_sel) {
                        if let Some(r) = self.snapshot.repos.get(ri).and_then(|rp| rp.runs.get(rj))
                        {
                            let cmd = format_repo_cmd(
                                &self.cwd,
                                &r.repo_path,
                                &format!(
                                    "{} status {}",
                                    shell_quote(&self.delegate_bin.to_string_lossy()),
                                    shell_quote(&r.run)
                                ),
                            );
                            return AppAction::CopyClipboard(cmd);
                        }
                    }
                    AppAction::None
                }
            },
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
            KeyCode::Char('Q') => AppAction::RefreshQuota,
            KeyCode::Char('r') => AppAction::Refresh,
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
                    format!("{bin} reply {id} '<追加问题>'"),
                    "运行中直接追加，当前轮结束后在同一会话接着答。".to_string(),
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

pub(crate) fn normalize_wid_for_match(raw: &str) -> String {
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
