use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

/// Pending runs inactive for more than 24 hours are folded by default.
pub const STALE_PENDING_SECS: u64 = 24 * 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GroupKind {
    Pending,
    Running,
    Queued,
    MergedUncleaned,
    Stopped,
}

impl GroupKind {
    pub const ALL: [GroupKind; 5] = [
        GroupKind::Pending,
        GroupKind::Running,
        GroupKind::Queued,
        GroupKind::MergedUncleaned,
        GroupKind::Stopped,
    ];

    pub fn label(self) -> &'static str {
        match self {
            GroupKind::Pending => "待处理",
            GroupKind::Running => "运行中",
            GroupKind::Queued => "排队",
            GroupKind::MergedUncleaned => "已合未清",
            GroupKind::Stopped => "已停",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AcceptStatus {
    None,
    Passed { command: String },
    Failed { command: String, exit_code: Option<i64> },
    NotExecuted { command: String, infra: bool },
}

impl AcceptStatus {
    pub fn short_badge(&self) -> &'static str {
        match self {
            AcceptStatus::None => "无验",
            AcceptStatus::Passed { .. } => "验✓",
            AcceptStatus::Failed { .. } => "验✗",
            AcceptStatus::NotExecuted { .. } => "验待",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DirChange {
    pub path: String,
    pub added: u64,
    pub deleted: u64,
    pub files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NoteItem {
    pub index: usize,
    pub text: String,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActivityInfo {
    pub kind: String,
    pub label: String,
    pub cmd: Option<String>,
    pub elapsed_seconds: Option<u64>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StallWarningInfo {
    pub kind: String,
    pub cmd: Option<String>,
    pub seconds: Option<u64>,
    pub count: Option<u64>,
    pub detail: Option<String>,
    pub self_check: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EventItem {
    pub seq: u64,
    pub at: String,
    pub run: String,
    pub repo: String,
    pub kind: String,
    pub summary: String,
    pub payload_pretty: String,
}

impl EventItem {
    pub fn matches_filter(&self, query_lower: &str) -> bool {
        if query_lower.is_empty() {
            return true;
        }
        [
            self.kind.as_str(),
            self.run.as_str(),
            self.repo.as_str(),
            self.summary.as_str(),
        ]
        .iter()
        .any(|h| h.to_lowercase().contains(query_lower))
    }

    pub fn kind_badge(&self) -> &'static str {
        match self.kind.as_str() {
            "started" => "启动",
            "queued" => "排队",
            "lane_acquired" => "占槽",
            "lane_released" => "放槽",
            "accept" => "验收",
            "delivered" => "交付",
            "question" => "提问",
            "rejected" => "未过",
            "failed" => "失败",
            "stopped" => "已停",
            "applied" => "合入",
            "cleaned" => "清理",
            "stall_warning" => "卡警",
            "loop_review" => "自检",
            "queue_dispatched" => "续派",
            "queue_paused" => "暂停",
            _ => "事件",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DagQueueItem {
    pub name: String,
    pub status: String,
    pub after: Vec<String>,
    pub repo: String,
    pub prompt_file: String,
    pub run_id: Option<String>,
    pub pause_reason: Option<String>,
    pub created_at: String,
}

impl DagQueueItem {
    pub fn status_badge(&self) -> &'static str {
        match self.status.as_str() {
            "pending" => "待续",
            "dispatching" => "派中",
            "dispatched" => "已派",
            "paused" => "暂停",
            "cancelled" => "取消",
            _ => "队列",
        }
    }

    pub fn matches_filter(&self, query_lower: &str) -> bool {
        if query_lower.is_empty() {
            return true;
        }
        let after_joined = self.after.join(",");
        [
            self.name.as_str(),
            self.status.as_str(),
            self.status_badge(),
            after_joined.as_str(),
            self.repo.as_str(),
            self.pause_reason.as_deref().unwrap_or(""),
        ]
        .iter()
        .any(|h| h.to_lowercase().contains(query_lower))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunItem {
    pub run: String,
    pub name: String,
    pub display_name: String,
    pub chain_round: usize,
    pub chain_ids: Vec<String>,
    pub parent: Option<String>,
    pub repo_path: PathBuf,
    pub repo_label: String,
    pub state: String,
    pub group: GroupKind,
    pub model: String,
    pub tier: Option<String>,
    pub mode: String,
    pub elapsed_seconds: Option<u64>,
    pub age_seconds: Option<u64>,
    pub finished_ago_seconds: Option<u64>,
    pub idle_seconds: Option<u64>,
    pub queued_seconds: Option<u64>,
    pub turns: Option<u64>,
    pub changes_files: u64,
    pub changes_added: u64,
    pub changes_deleted: u64,
    pub pending_files: u64,
    pub pending_added: u64,
    pub pending_deleted: u64,
    pub dir_changes: Vec<DirChange>,
    pub dirs_more: usize,
    pub files: Vec<String>,
    pub accept: AcceptStatus,
    pub protect_violation: Vec<String>,
    pub prompt_protected_paths: Vec<String>,
    pub source_write_violation: bool,
    pub conflict_markers: Vec<String>,
    pub last_command: Option<String>,
    pub activity: Option<ActivityInfo>,
    pub stall_warning: Option<StallWarningInfo>,
    pub next_step: Option<String>,
    pub recovery: Option<String>,
    pub error: Option<String>,
    pub transient_error: Option<String>,
    pub resumes: u64,
    pub question: bool,
    pub applied: bool,
    pub applied_by: Option<String>,
    pub pending_reply: bool,
    pub queue_reason: Option<String>,
    pub queue_ahead: Option<u64>,
    pub measure_waiting_ahead: Option<u64>,
    pub pending_notes: usize,
    pub notes: Vec<NoteItem>,
    pub result_chars: usize,
    pub has_result: bool,
    pub has_worktree: bool,
    pub finished_at: Option<String>,
    pub tokens_input: Option<u64>,
    pub tokens_output: Option<u64>,
    pub tokens_cache_read: Option<u64>,
}

impl RunItem {
    /// Effective file count / +/- lines to display in the list row.
    pub fn display_changes(&self) -> (u64, u64, u64) {
        if self.pending_files > 0 {
            (self.pending_files, self.pending_added, self.pending_deleted)
        } else if self.changes_files > 0 {
            (self.changes_files, self.changes_added, self.changes_deleted)
        } else if !self.files.is_empty() {
            (self.files.len() as u64, 0, 0)
        } else {
            (0, 0, 0)
        }
    }

    pub fn has_protect_violation(&self) -> bool {
        !self.protect_violation.is_empty() || self.source_write_violation
    }

    pub fn has_protect_warning(&self) -> bool {
        !self.prompt_protected_paths.is_empty()
    }

    /// Seconds elapsed since this run was last active (smaller = more recent).
    pub fn last_activity_seconds(&self) -> u64 {
        match self.group {
            GroupKind::Running => self.idle_seconds.unwrap_or(0),
            GroupKind::Queued => 0,
            GroupKind::Pending | GroupKind::MergedUncleaned | GroupKind::Stopped => self
                .finished_ago_seconds
                .or(self.age_seconds)
                .unwrap_or(u64::MAX),
        }
    }

    pub fn is_stale_pending(&self) -> bool {
        self.group == GroupKind::Pending && self.last_activity_seconds() > STALE_PENDING_SECS
    }

    pub fn state_badge(&self) -> &'static str {
        if self.question && !self.applied {
            return "提问";
        }
        if matches!(self.state.as_str(), "running" | "starting") {
            if self.stall_warning.is_some() {
                return "卡警";
            }
            if let Some(act) = &self.activity {
                return match act.kind.as_str() {
                    "running_command" => "跑命",
                    "thinking" => "思考",
                    "waiting" => "等待",
                    "quota" => "配额",
                    "error" => "报错",
                    _ => "运行",
                };
            }
        }
        match self.state.as_str() {
            "running" => "运行",
            "starting" => "启动",
            "waiting" | "queued" => "排队",
            "delivered" if self.applied => "已合",
            "delivered" => "待合",
            "answered" if self.applied => "已合",
            "answered" if self.pending_files > 0 => "待合",
            "answered" => "已答",
            "rejected" => "没过",
            "failed" => "失败",
            "timeout" => "超时",
            "stalled" => "卡住",
            "malformed" => "异常",
            "crashed" => "崩溃",
            "killed" => "强停",
            "stopped" => "已停",
            "needs-redispatch" => "重派",
            "cleaned" => "待办",
            _ => "未知",
        }
    }

    pub fn cache_key(&self) -> String {
        format!(
            "{}::{}::{}::{}",
            self.repo_path.display(),
            self.run,
            self.finished_at.as_deref().unwrap_or(""),
            self.result_chars
        )
    }

    pub fn matches_filter(&self, query_lower: &str) -> bool {
        if query_lower.is_empty() {
            return true;
        }
        let haystacks = [
            self.display_name.as_str(),
            self.name.as_str(),
            self.run.as_str(),
            self.repo_label.as_str(),
            self.model.as_str(),
            self.state.as_str(),
            self.state_badge(),
            self.group.label(),
            self.last_command.as_deref().unwrap_or(""),
            self.activity.as_ref().map(|a| a.label.as_str()).unwrap_or(""),
            self.next_step.as_deref().unwrap_or(""),
        ];
        if haystacks
            .iter()
            .any(|h| h.to_lowercase().contains(query_lower))
        {
            return true;
        }
        self.files
            .iter()
            .any(|f| f.to_lowercase().contains(query_lower))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoGroup {
    pub path: PathBuf,
    pub label: String,
    pub runs: Vec<RunItem>,
}

impl RepoGroup {
    pub fn count_group(&self, group: GroupKind) -> usize {
        self.runs.iter().filter(|r| r.group == group).count()
    }

    /// Most recent activity across all runs in this repository (smaller = more recent).
    pub fn recent_activity_seconds(&self) -> u64 {
        self.runs
            .iter()
            .map(RunItem::last_activity_seconds)
            .min()
            .unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LaneEntry {
    pub label: String,
    pub since: String,
    pub running: bool,
    pub remote: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct LaneStatus {
    pub max_heavy: Option<usize>,
    pub remote_max_heavy: Option<usize>,
    pub local: Vec<LaneEntry>,
    pub remote: Vec<LaneEntry>,
}

impl LaneStatus {
    pub fn local_running(&self) -> usize {
        self.local.iter().filter(|e| e.running).count()
    }
    pub fn local_queued(&self) -> usize {
        self.local.iter().filter(|e| !e.running).count()
    }
    pub fn remote_running(&self) -> usize {
        self.remote.iter().filter(|e| e.running).count()
    }
    pub fn remote_queued(&self) -> usize {
        self.remote.iter().filter(|e| !e.running).count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostLoad {
    /// Stored as tenths (e.g. 23 -> "2.3") so hashing ignores tiny noise and stays Eq/Hash.
    pub load1_tenths: u32,
    pub load5_tenths: u32,
    pub load15_tenths: u32,
    pub mem_used_tenths_gib: Option<u32>,
    pub mem_total_tenths_gib: Option<u32>,
}

impl HostLoad {
    pub fn format_short(&self) -> String {
        let load = format!(
            "负载 {}.{} {}.{}",
            self.load1_tenths / 10,
            self.load1_tenths % 10,
            self.load5_tenths / 10,
            self.load5_tenths % 10
        );
        if let (Some(used), Some(total)) = (self.mem_used_tenths_gib, self.mem_total_tenths_gib) {
            format!(
                "{load} · 内存 {}.{}/{}.{}G",
                used / 10,
                used % 10,
                total / 10,
                total % 10
            )
        } else {
            load
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppSnapshot {
    pub repos: Vec<RepoGroup>,
    pub lane: LaneStatus,
    pub events: Vec<EventItem>,
    pub dag_queue: Vec<DagQueueItem>,
    pub host_load: Option<HostLoad>,
    pub refreshed_at: String,
    pub warnings: Vec<String>,
}

impl AppSnapshot {
    /// Fingerprint of visible data excluding `refreshed_at`, used to skip redundant redraws.
    pub fn data_fingerprint(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.repos.hash(&mut h);
        self.lane.hash(&mut h);
        self.events.hash(&mut h);
        self.dag_queue.hash(&mut h);
        self.host_load.hash(&mut h);
        self.warnings.hash(&mut h);
        h.finish()
    }

    pub fn total_count(&self, group: GroupKind) -> usize {
        self.repos.iter().map(|r| r.count_group(group)).sum()
    }
}

pub fn format_duration(seconds: Option<u64>) -> String {
    let Some(sec) = seconds else {
        return "-".to_string();
    };
    if sec < 60 {
        format!("{sec}s")
    } else if sec < 3600 {
        format!("{}m", sec / 60)
    } else if sec < 86400 {
        let h = sec / 3600;
        let m = (sec % 3600) / 60;
        format!("{h}h{m:02}m")
    } else {
        let d = sec / 86400;
        let h = (sec % 86400) / 3600;
        format!("{d}d{h}h")
    }
}

pub fn format_duration_zh(seconds: Option<u64>) -> String {
    let Some(sec) = seconds else {
        return "-".to_string();
    };
    if sec < 60 {
        format!("{sec} 秒")
    } else if sec < 3600 {
        format!("{} 分钟", sec / 60)
    } else if sec < 86400 {
        let h = sec / 3600;
        let m = (sec % 3600) / 60;
        if m == 0 {
            format!("{h} 小时")
        } else {
            format!("{h} 小时 {m:02} 分")
        }
    } else {
        let d = sec / 86400;
        let h = (sec % 86400) / 3600;
        if h == 0 {
            format!("{d} 天")
        } else {
            format!("{d} 天 {h} 时")
        }
    }
}

/// Natural Chinese relative time, e.g. `刚刚`, `3 分钟前`, `2 小时前`, `5 天前`.
pub fn format_relative_time(seconds: Option<u64>) -> String {
    let Some(sec) = seconds else {
        return "-".to_string();
    };
    if sec < 45 {
        "刚刚".to_string()
    } else if sec < 3600 {
        format!("{} 分钟前", (sec / 60).max(1))
    } else if sec < 86400 {
        format!("{} 小时前", sec / 3600)
    } else {
        format!("{} 天前", sec / 86400)
    }
}

/// Proportional mini-bar breakdown `(added_cells, deleted_cells, neutral_cells)` for diff scale.
/// Total filled cells scale logarithmically with total line edits across `slots` cells.
pub fn diff_bar_segments(added: u64, deleted: u64, files: u64, slots: usize) -> (usize, usize, usize) {
    if slots == 0 {
        return (0, 0, 0);
    }
    let total_lines = added.saturating_add(deleted);
    if total_lines == 0 {
        if files == 0 {
            return (0, 0, 0);
        }
        let filled = match files {
            1..=2 => 1,
            3..=6 => 2,
            7..=15 => 3,
            16..=30 => 4,
            _ => slots,
        }
        .min(slots);
        return (0, 0, filled);
    }
    let filled = match total_lines {
        1..=30 => 1,
        31..=150 => 2,
        151..=600 => 3,
        601..=2500 => 4,
        _ => slots,
    }
    .min(slots);

    if deleted == 0 {
        return (filled, 0, 0);
    }
    if added == 0 {
        return (0, filled, 0);
    }
    if filled == 1 {
        return if added >= deleted { (1, 0, 0) } else { (0, 1, 0) };
    }
    let mut del_cells = ((deleted as f64 / total_lines as f64) * filled as f64).round() as usize;
    if del_cells == 0 {
        del_cells = 1;
    }
    if del_cells >= filled {
        del_cells = filled - 1;
    }
    (filled - del_cells, del_cells, 0)
}

/// Compute `(filled_cells, total_cells)` for the compact lane meter.
pub fn lane_meter_cells(running: usize, max_heavy: Option<usize>) -> (usize, usize) {
    match max_heavy {
        Some(0) | None => {
            let total = running.clamp(4, 6);
            (running.min(total), total)
        }
        Some(lim) if lim <= 8 => (running.min(lim), lim),
        Some(lim) => {
            let total = 8usize;
            let filled = if running == 0 {
                0
            } else {
                (((running as f64 / lim as f64) * total as f64).ceil() as usize).clamp(1, total)
            };
            (filled, total)
        }
    }
}
