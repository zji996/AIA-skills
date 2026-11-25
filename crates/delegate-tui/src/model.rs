use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

/// Pending runs inactive for more than 24 hours are folded by default.
pub const STALE_PENDING_SECS: u64 = 24 * 3600;
/// Default stale thresholds for work items (aligned with `delegate` work.rs).
pub const WORK_STALE_VERIFY_SECS: u64 = 4 * 3600;
pub const WORK_STALE_READY_SECS: u64 = 8 * 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GroupKind {
    Pending,
    Running,
    Queued,
    Collected,
    MergedUncleaned,
    Stopped,
}

impl GroupKind {
    pub const ALL: [GroupKind; 6] = [
        GroupKind::Pending,
        GroupKind::Running,
        GroupKind::Queued,
        GroupKind::Collected,
        GroupKind::MergedUncleaned,
        GroupKind::Stopped,
    ];

    pub fn label(self) -> &'static str {
        match self {
            GroupKind::Pending => "待处理",
            GroupKind::Running => "运行中",
            GroupKind::Queued => "排队",
            GroupKind::Collected => "已收取",
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
pub struct ReviewInfo {
    pub status: String,
    pub verdict: Option<String>,
    pub round: u64,
    pub rework_rounds: u64,
    pub needs_attention: bool,
    pub upgraded_by: Option<String>,
    pub verdict_file: Option<String>,
    pub controller_paths: Vec<String>,
    pub sign_hash: Option<String>,
    pub sign_required: bool,
    pub signed: bool,
    pub sign_valid: bool,
    pub sign_invalidated: bool,
}

impl ReviewInfo {
    pub fn row_snippet(&self) -> String {
        if self.status == "reviewing" {
            if self.round > 1 {
                format!("审查中 第{}轮", self.round)
            } else {
                "审查中".to_string()
            }
        } else if self.status == "reworking" {
            let r = self.rework_rounds.max(self.round).max(1);
            format!("REWORK 第{r}轮")
        } else {
            match self.verdict.as_deref() {
                Some("PASS") => "PASS".to_string(),
                Some("REWORK") => {
                    let r = self.rework_rounds.max(self.round).max(1);
                    format!("REWORK 第{r}轮")
                }
                Some("NEEDS_CONTROLLER") => "NEEDS_CONTROLLER".to_string(),
                Some(other) if !other.is_empty() => other.to_string(),
                _ => String::new(),
            }
        }
    }

    pub fn detail_label(&self) -> String {
        if self.status == "reviewing" {
            if self.round > 1 {
                format!("审查中（第 {} 轮）", self.round)
            } else {
                "审查中".to_string()
            }
        } else if self.status == "reworking" {
            let r = self.rework_rounds.max(self.round).max(1);
            format!("REWORK 第 {r} 轮（返工中）")
        } else {
            match self.verdict.as_deref() {
                Some("PASS") => {
                    if self.rework_rounds > 0 {
                        format!("PASS（第 {} 轮）", self.round.max(1))
                    } else {
                        "PASS".to_string()
                    }
                }
                Some("REWORK") => {
                    let r = self.rework_rounds.max(self.round).max(1);
                    format!("REWORK 第 {r} 轮")
                }
                Some("NEEDS_CONTROLLER") => {
                    if self.sign_required {
                        if self.sign_valid {
                            "NEEDS_CONTROLLER（已签字）".to_string()
                        } else if self.sign_invalidated {
                            "NEEDS_CONTROLLER（签字已失效）".to_string()
                        } else {
                            "NEEDS_CONTROLLER（待签字）".to_string()
                        }
                    } else {
                        "NEEDS_CONTROLLER".to_string()
                    }
                }
                Some(other) => other.to_string(),
                None => String::new(),
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PremergeInfo {
    pub status: String, // "ready" | "running" | "stale" | "conflict" | "failed"
    pub base_short: String,
    pub conflicts: Vec<String>,
    pub conflict_count: usize,
    pub failed_layer: Option<String>,
    pub log: Option<String>,
}

impl PremergeInfo {
    pub fn row_snippet(&self) -> String {
        match self.status.as_str() {
            "ready" => {
                if self.base_short.is_empty() {
                    "快进✓".to_string()
                } else {
                    format!("快进✓ {}", self.base_short)
                }
            }
            "stale" => "快进✗ 主干已前进".to_string(),
            "running" | "pending" => "预合并中".to_string(),
            "conflict" => {
                if let Some(first) = self.conflicts.first() {
                    if self.conflict_count > 1 {
                        format!("预合并冲突:{first} 等{}", self.conflict_count)
                    } else {
                        format!("预合并冲突:{first}")
                    }
                } else {
                    "预合并冲突".to_string()
                }
            }
            "failed" => {
                if let Some(layer) = &self.failed_layer {
                    format!("预合并失败:{layer}")
                } else {
                    "预合并失败".to_string()
                }
            }
            _ => String::new(),
        }
    }

    pub fn full_label(&self) -> String {
        match self.status.as_str() {
            "ready" => {
                if self.base_short.is_empty() {
                    "快进 ✓".to_string()
                } else {
                    format!("快进 ✓（基于 {}）", self.base_short)
                }
            }
            "stale" => "快进 ✗ 主干已前进".to_string(),
            "running" | "pending" => "预合并中".to_string(),
            "conflict" => {
                if self.conflicts.is_empty() {
                    "预合并冲突".to_string()
                } else if self.conflict_count == 1 {
                    format!("预合并冲突：{}", self.conflicts[0])
                } else if self.conflict_count == 2 && self.conflicts.len() >= 2 {
                    format!("预合并冲突：{}、{}", self.conflicts[0], self.conflicts[1])
                } else {
                    format!("预合并冲突：{} 等 {} 个", self.conflicts[0], self.conflict_count)
                }
            }
            "failed" => {
                if let Some(layer) = &self.failed_layer {
                    format!("预合并失败（{layer}）")
                } else {
                    "预合并失败".to_string()
                }
            }
            _ => String::new(),
        }
    }
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
            "blocked_mainline" => "主阻",
            "accept_blocked_mainline" => "主阻",
            "accept_rerun" => "重验",
            "rejected" => "未过",
            "failed" => "失败",
            "stopped" => "已停",
            "applied" => "合入",
            "cleaned" => "清理",
            "stall_warning" => "卡警",
            "loop_review" => "自检",
            "queue_dispatched" => "续派",
            "queue_paused" => "暂停",
            "review_started" => "开审",
            "review_verdict" => "审结",
            "rework_sent" => "返工",
            "needs_attention" => "关注",
            "merge_started" => "合始",
            "merge_item" => "合项",
            "merge_done" => "合结",
            "premerge_ready" => "快进",
            "premerge_conflict" => "预冲",
            "premerge_failed" => "预败",
            "radar_unchanged" => "侦静",
            "radar_done" => "侦察",
            _ => "事件",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MergeItemEntry {
    pub run: String,
    pub name: String,
    pub status: String,
    pub commit: String,
    pub title: String,
    pub accept_ok: Option<bool>,
    pub auto_merge: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MergeRunItem {
    pub merge_run: String,
    pub name: String,
    pub repo_path: PathBuf,
    pub repo_label: String,
    pub base_head: String,
    pub head: String,
    pub state: String,
    pub started_at: String,
    pub finished_at: String,
    pub merged_count: u64,
    pub skipped_count: u64,
    pub auto_merge_count: u64,
    pub all_auto_merge: bool,
    pub merge_accept_ok: bool,
    pub merge_accept_skipped: bool,
    pub merge_accept_cmd: String,
    pub applied: bool,
    pub report_path: String,
    pub items: Vec<MergeItemEntry>,
}

impl MergeRunItem {
    pub fn matches_filter(&self, query_lower: &str) -> bool {
        if query_lower.is_empty() {
            return true;
        }
        if [
            self.merge_run.as_str(),
            self.name.as_str(),
            self.repo_label.as_str(),
            self.state.as_str(),
        ]
        .iter()
        .any(|h| h.to_lowercase().contains(query_lower))
        {
            return true;
        }
        self.items.iter().any(|it| {
            it.run.to_lowercase().contains(query_lower)
                || it.title.to_lowercase().contains(query_lower)
                || it.reason.to_lowercase().contains(query_lower)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RadarCandidateItem {
    pub id: String,
    pub repo_path: PathBuf,
    pub repo_label: String,
    pub title: String,
    pub evidence: Vec<String>,
    pub suggestion: String,
    pub cost: String,
    pub brief_ready: bool,
    pub brief_note: String,
    pub fingerprint: String,
    pub first_seen_at: String,
    pub last_seen_at: String,
    pub picked_at: String,
    pub board_path: String,
}

impl RadarCandidateItem {
    pub fn matches_filter(&self, query_lower: &str) -> bool {
        if query_lower.is_empty() {
            return true;
        }
        if [
            self.id.as_str(),
            self.repo_label.as_str(),
            self.title.as_str(),
            self.suggestion.as_str(),
            self.cost.as_str(),
        ]
        .iter()
        .any(|h| h.to_lowercase().contains(query_lower))
        {
            return true;
        }
        self.evidence
            .iter()
            .any(|e| e.to_lowercase().contains(query_lower))
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct RunEtaInfo {
    pub stage: String,
    pub stage_label: String,
    pub stopped_clock: bool,
    pub short_text: String,
    pub compact_badge: String,
    pub detail_text: String,
    pub blocker_kind: Option<String>,
    pub blocker_who: Option<String>,
    pub blocker_label: Option<String>,
    pub p50_clock: Option<String>,
    pub p75_clock: Option<String>,
    pub anomaly: bool,
    pub is_prior: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct WorkEtaInfo {
    pub stopped_clock: bool,
    pub short_text: String,
    pub focus_badge: String,
    pub compact_badge: String,
    pub blocker_kind: Option<String>,
    pub blocker_label: Option<String>,
    pub p50_clock: Option<String>,
    pub p75_clock: Option<String>,
    pub is_prior: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct WaveBlockerBrief {
    pub run: String,
    pub name: String,
    pub kind: String,
    pub compact: String,
    pub label: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct WaveEtaSummary {
    pub active_count: usize,
    pub running_count: usize,
    pub queued_count: usize,
    pub blocked_count: usize,
    pub earliest_p50_clock: Option<String>,
    pub wave_p50_clock: Option<String>,
    pub wave_p75_clock: Option<String>,
    pub is_prior: bool,
    pub blockers: Vec<WaveBlockerBrief>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct WorkDagEtaSummary {
    pub write_chains: usize,
    pub depth: usize,
    pub active_count: usize,
    pub dev_wall_range: String,
    pub earliest_finish_p50_clock: Option<String>,
    pub earliest_finish_p75_clock: Option<String>,
    pub external_wait_count: usize,
    pub owner_blocked_count: usize,
    pub blocker_count: usize,
    pub headline: String,
    pub is_prior: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct SnapshotEta {
    pub wave: WaveEtaSummary,
    pub work: WorkDagEtaSummary,
    pub queue: WorkDagEtaSummary,
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
    pub thinking: Option<String>,
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
    pub review: Option<ReviewInfo>,
    pub premerge: Option<PremergeInfo>,
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
    pub result_read_at: Option<String>,
    pub finished_at: Option<String>,
    pub tokens_input: Option<u64>,
    pub tokens_output: Option<u64>,
    pub tokens_cache_read: Option<u64>,
    pub work_id: Option<String>,
    pub eta: Option<RunEtaInfo>,
}

impl RunItem {
    /// Short <= 6-col ETA badge for Runs rows (`~03:40`, `<5m`, `卡主控`, ...).
    pub fn eta_badge(&self) -> Option<(&str, bool)> {
        if self.applied || matches!(self.group, GroupKind::Collected | GroupKind::MergedUncleaned) {
            return None;
        }
        let eta = self.eta.as_ref()?;
        if eta.stage == "done" {
            return None;
        }
        let b = eta.compact_badge.trim();
        if b.is_empty() || b == "完成" || b == "-" {
            return None;
        }
        Some((b, eta.stopped_clock))
    }
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
            GroupKind::Pending
            | GroupKind::Collected
            | GroupKind::MergedUncleaned
            | GroupKind::Stopped => self
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
            if let Some(rev) = &self.review {
                if rev.status == "reviewing" {
                    return "审中";
                }
                if rev.status == "reworking" {
                    return "返工";
                }
            }
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
        if !self.applied {
            if let Some(rev) = &self.review {
                if rev.verdict.as_deref() == Some("NEEDS_CONTROLLER") {
                    return "主审";
                }
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
            "blocked_mainline" => "主阻",
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
        let review_snippet = self
            .review
            .as_ref()
            .map(ReviewInfo::detail_label)
            .unwrap_or_default();
        let premerge_snippet = self
            .premerge
            .as_ref()
            .map(PremergeInfo::full_label)
            .unwrap_or_default();
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
            review_snippet.as_str(),
            premerge_snippet.as_str(),
            self.next_step.as_deref().unwrap_or(""),
            self.work_id.as_deref().unwrap_or(""),
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
pub struct LaneLease {
    pub rank: usize,
    pub used: usize,
    pub share: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LaneEntry {
    pub label: String,
    pub since: String,
    pub running: bool,
    pub remote: bool,
    pub rank: Option<usize>,
    pub used: Option<usize>,
    pub share: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct LaneStatus {
    pub max_heavy: Option<usize>,
    pub heavy_jobs: Option<usize>,
    pub threads: Option<usize>,
    pub budget: Option<usize>,
    pub heavy_jobs_explicit: bool,
    pub pool_total: Option<usize>,
    pub pool_target: Option<usize>,
    pub pool_free: Option<usize>,
    pub leases: Vec<LaneLease>,
    pub concurrency_summary: Option<String>,
    pub remote_max_heavy: Option<usize>,
    pub local: Vec<LaneEntry>,
    pub remote: Vec<LaneEntry>,
}

impl LaneStatus {
    pub fn pool_summary(&self, compact: bool) -> Option<String> {
        let (total, target, free) = (self.pool_total?, self.pool_target?, self.pool_free?);
        let exp = if self.heavy_jobs_explicit {
            self.heavy_jobs
                .map(|j| format!("（显式×{j}）"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        if compact {
            Some(format!("令牌 {total}/{target} 闲{free}{exp}"))
        } else {
            Some(format!("令牌 {total}/{target} 空闲 {free}{exp}"))
        }
    }
    pub fn leases_summary(&self) -> Option<String> {
        if !self.leases.is_empty() {
            let parts: Vec<String> = self
                .leases
                .iter()
                .map(|l| format!("#{} {}/{}", l.rank, l.used, l.share))
                .collect();
            return Some(parts.join(" · "));
        }
        let mut from_local: Vec<String> = self
            .local
            .iter()
            .filter(|e| e.running)
            .filter_map(|e| Some(format!("#{} {}/{}", e.rank?, e.used?, e.share?)))
            .collect();
        if from_local.is_empty() {
            None
        } else {
            from_local.truncate(6);
            Some(from_local.join(" · "))
        }
    }
    pub fn compact_concurrency(&self) -> Option<String> {
        if let Some(pool) = self.pool_summary(true) {
            return Some(pool);
        }
        let (t, b, j) = (self.threads?, self.budget?, self.heavy_jobs?);
        let mh = match self.max_heavy {
            Some(0) => "∞".to_string(),
            Some(n) => n.to_string(),
            None => "?".to_string(),
        };
        let tag = if self.heavy_jobs_explicit { "显" } else { "" };
        Some(format!("{t}−2={b}→{mh}×{j}{tag}"))
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OwnerKind {
    Act,
    Nod,
    Card,
    Direction,
}

impl OwnerKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "act" => Some(Self::Act),
            "nod" => Some(Self::Nod),
            "card" => Some(Self::Card),
            "direction" => Some(Self::Direction),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Act => "动手",
            Self::Nod => "点头",
            Self::Card => "看卡",
            Self::Direction => "方向",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkOwnerInfo {
    pub kind: OwnerKind,
    pub ask: String,
    pub recommendation: String,
    pub if_undecided: String,
    pub cost: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkWaitStatus {
    pub condition: String,
    pub satisfied: bool,
    pub detail: String,
    pub pointer: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkFollowup {
    pub id: usize,
    pub kind: String,
    pub text: String,
    pub anchor: String,
    pub source_run: String,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkArtifact {
    pub path: String,
    pub source_run: String,
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkBucket {
    Owner,
    NeedsAction,
    InProgress,
    Ready,
    Waiting,
    Deferred,
    Done,
    Dropped,
}

impl WorkBucket {
    pub fn label(self) -> &'static str {
        match self {
            Self::Owner => "等你",
            Self::NeedsAction => "需处理",
            Self::InProgress => "推进中",
            Self::Ready => "就绪",
            Self::Waiting => "等待",
            Self::Deferred => "暂缓",
            Self::Done => "已完成",
            Self::Dropped => "已否",
        }
    }

    pub fn is_open(self) -> bool {
        !matches!(self, Self::Done | Self::Dropped)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct WorkItem {
    pub id: String,
    pub title: String,
    pub repo: String,
    pub repo_label: String,
    pub state: String,
    pub closed_kind: String,
    pub done_when: String,
    pub wait_on: Vec<String>,
    pub verify_on: Vec<String>,
    pub wait_on_status: Vec<WorkWaitStatus>,
    pub runs: Vec<String>,
    pub artifacts: Vec<WorkArtifact>,
    pub brief: String,
    pub followups: Vec<WorkFollowup>,
    pub owner: Option<WorkOwnerInfo>,
    pub owner_ref: String,
    pub auto_dispatch_prompt: String,
    pub auto_dispatch_run: String,
    pub defer_reason: String,
    pub defer_until: String,
    pub defer_streak: u32,
    pub created_at: String,
    pub updated_at: String,
    pub state_since: String,
    pub close_reason: String,
    pub close_evidence: String,
    pub focus: String,
    pub next: String,
    pub applied: bool,
    pub eta: Option<WorkEtaInfo>,
}

impl WorkItem {
    pub fn bucket(&self) -> WorkBucket {
        if self.state == "done" {
            if self.closed_kind == "dropped" {
                return WorkBucket::Dropped;
            }
            return WorkBucket::Done;
        }
        if self.owner.is_some() {
            return WorkBucket::Owner;
        }
        match self.state.as_str() {
            "attention" | "verify" => WorkBucket::NeedsAction,
            "active" => WorkBucket::InProgress,
            "ready" => WorkBucket::Ready,
            "blocked" => WorkBucket::Waiting,
            "deferred" => WorkBucket::Deferred,
            _ => WorkBucket::Ready,
        }
    }

    pub fn state_badge(&self) -> &'static str {
        if self.state == "done" {
            return if self.closed_kind == "dropped" {
                "已否"
            } else {
                "完成"
            };
        }
        if let Some(ow) = &self.owner {
            return ow.kind.label();
        }
        match self.state.as_str() {
            "attention" => "需处理",
            "verify" => "待闭环",
            "active" => "推进中",
            "ready" => "就绪",
            "blocked" => "等待",
            "deferred" => "暂缓",
            _ => "工作",
        }
    }

    pub fn display_title(&self) -> &str {
        if let Some(ow) = &self.owner {
            if !ow.ask.is_empty() {
                return &ow.ask;
            }
        }
        &self.title
    }

    pub fn pending_followups(&self) -> usize {
        self.followups.iter().filter(|f| !f.done).count()
    }

    pub fn is_missing_brief(&self) -> bool {
        self.owner.is_none() && self.state == "ready" && self.brief.trim().is_empty()
    }

    pub fn is_missing_owner_advice(&self) -> bool {
        let Some(ow) = &self.owner else {
            return false;
        };
        if self.state == "done" || ow.kind == OwnerKind::Direction {
            return false;
        }
        ow.ask.trim().is_empty()
            || ow.recommendation.trim().is_empty()
            || ow.if_undecided.trim().is_empty()
            || ow.cost.trim().is_empty()
    }

    pub fn matches_filter(&self, query_lower: &str) -> bool {
        if query_lower.is_empty() {
            return true;
        }
        let ow_kind = self
            .owner
            .as_ref()
            .map(|o| o.kind.label())
            .unwrap_or("");
        let ow_ask = self
            .owner
            .as_ref()
            .map(|o| o.ask.as_str())
            .unwrap_or("");
        let ow_rec = self
            .owner
            .as_ref()
            .map(|o| o.recommendation.as_str())
            .unwrap_or("");
        let haystacks = [
            self.id.as_str(),
            self.title.as_str(),
            self.repo_label.as_str(),
            self.state.as_str(),
            self.state_badge(),
            self.bucket().label(),
            self.focus.as_str(),
            self.done_when.as_str(),
            self.close_reason.as_str(),
            self.close_evidence.as_str(),
            self.brief.as_str(),
            ow_kind,
            ow_ask,
            ow_rec,
        ];
        if haystacks
            .iter()
            .any(|h| h.to_lowercase().contains(query_lower))
        {
            return true;
        }
        self.wait_on
            .iter()
            .any(|w| w.to_lowercase().contains(query_lower))
            || self
                .runs
                .iter()
                .any(|r| r.to_lowercase().contains(query_lower))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkCounts {
    pub owner: usize,
    pub needs_action: usize,
    pub ready: usize,
    pub in_progress: usize,
    pub waiting: usize,
    pub deferred: usize,
    pub stale: usize,
    pub done_today: usize,
    pub done_total: usize,
    pub dropped_total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunningAgyProc {
    pub pid: u32,
    pub run: Option<String>,
    pub cmdline: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct AccountUsageWindow {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub total: u64,
    pub author_total: u64,
    pub reviewer_total: u64,
    pub rounds: usize,
    pub runs: usize,
    pub live_counted: usize,
    pub uncounted_running: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct AccountUsageStats {
    pub one_hour: AccountUsageWindow,
    pub five_hour: AccountUsageWindow,
    pub seven_day: AccountUsageWindow,
    pub hourly_24h: Vec<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct QuotaPoolEntry {
    pub pool: String,
    pub window: String,       // "weekly" | "fiveHour" | "other"
    pub window_label: String, // e.g. "Weekly Limit Remaining"
    pub remaining_pct_x100: u32,
    pub reset_at: String,
    pub reset_in_seconds: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct AccountRealQuota {
    pub fetched_at: String,
    pub fetched_epoch: i64,
    pub age_seconds: u64,
    pub ok: bool,
    pub error: Option<String>,
    pub entries: Vec<QuotaPoolEntry>,
    pub unknown_lines: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct QuotaNoteSnapshot {
    pub weekly_pct_x100: Option<u32>,
    pub weekly_at: String,
    pub five_hour_pct_x100: Option<u32>,
    pub five_hour_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct QuotaWindowEstimate {
    pub remaining_pct_x100: u32,
    pub tokens_per_pct: u64,
    pub estimated_remaining_tokens: u64,
    pub estimated_remaining_minutes: Option<u64>,
    pub burn_rate_per_hour: u64,
    pub method: String,
    pub source: String,
    pub burn_rate_source: String,
    pub pool: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct QuotaPoolEstimate {
    pub pool: String,
    pub weekly: Option<QuotaWindowEstimate>,
    pub five_hour: Option<QuotaWindowEstimate>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct QuotaEstimate {
    pub weekly: Option<QuotaWindowEstimate>,
    pub five_hour: Option<QuotaWindowEstimate>,
    pub pools: Vec<QuotaPoolEstimate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgyAccountItem {
    pub id: String,
    pub email: String,
    pub label: String,
    pub active: bool,
    pub added_at: String,
    pub last_used_at: String,
    pub last_synced_at: String,
    pub token_expires_at: String,
    pub file_count: usize,
    pub files: Vec<String>,
    pub source: String,
    pub usage: AccountUsageStats,
    pub quota: Option<AccountRealQuota>,
    pub quota_note: Option<QuotaNoteSnapshot>,
    pub quota_estimate: Option<QuotaEstimate>,
}

pub fn mask_account_name(name: &str) -> String {
    let mut chars = name.chars();
    let prefix: String = chars.by_ref().take(3).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

pub fn thinking_short_zh(level: &str) -> Option<&'static str> {
    match level.trim().to_ascii_lowercase().as_str() {
        "off" => Some("关"),
        "minimal" | "min" => Some("微"),
        "low" => Some("低"),
        "medium" | "med" => Some("中"),
        "high" => Some("高"),
        "xhigh" => Some("极"),
        _ => None,
    }
}

impl AgyAccountItem {
    /// Masked account name by default (first 3 chars + `…`); full id (+ email) when revealed.
    pub fn display_name(&self, revealed: bool) -> String {
        if revealed {
            if self.email.is_empty() {
                self.id.clone()
            } else {
                format!("{} ({})", self.id, self.email)
            }
        } else {
            mask_account_name(&self.id)
        }
    }

    pub fn matches_filter(&self, query_lower: &str) -> bool {
        if query_lower.is_empty() {
            return true;
        }
        [
            self.id.as_str(),
            self.email.as_str(),
            self.label.as_str(),
            self.source.as_str(),
            if self.active { "活跃 active" } else { "备用" },
        ]
        .iter()
        .any(|h| h.to_lowercase().contains(query_lower))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct AgyAccountsState {
    pub root: String,
    pub active: Option<String>,
    pub live_email: String,
    pub live_drifted: bool,
    pub running_agy: Vec<RunningAgyProc>,
    pub accounts: Vec<AgyAccountItem>,
    pub unassigned_usage: AccountUsageStats,
    pub quota_updated_at: String,
    pub quota_age_seconds: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct PurposeBreakdown {
    pub write: u64,
    pub review: u64,
    pub rework: u64,
    pub read_only: u64,
}

impl PurposeBreakdown {
    pub fn total(&self) -> u64 {
        self.write
            .saturating_add(self.review)
            .saturating_add(self.rework)
            .saturating_add(self.read_only)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ModelTokens {
    pub model: String,
    pub tokens: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct DailyTrendPoint {
    pub ymd: String,
    pub date: String,
    pub merged_count: u64,
    pub median_merge_secs: Option<u64>,
    pub avg_merge_secs: Option<u64>,
    pub total_chains: u64,
    pub first_round_pass: u64,
    pub first_round_pass_pct: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct AnalyticsSnapshot {
    pub hourly_24h_total: Vec<u64>,
    pub hourly_24h_by_account: Vec<(String, Vec<u64>)>,
    pub by_purpose: PurposeBreakdown,
    pub by_model: Vec<ModelTokens>,
    pub daily_7d: Vec<DailyTrendPoint>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppSnapshot {
    pub repos: Vec<RepoGroup>,
    pub work: Vec<WorkItem>,
    pub lane: LaneStatus,
    pub events: Vec<EventItem>,
    pub dag_queue: Vec<DagQueueItem>,
    pub merges: Vec<MergeRunItem>,
    pub candidates: Vec<RadarCandidateItem>,
    pub agy_accounts: AgyAccountsState,
    pub analytics: AnalyticsSnapshot,
    pub eta: SnapshotEta,
    pub host_load: Option<HostLoad>,
    pub generated_epoch: Option<i64>,
    pub today_ymd: String,
    pub refreshed_at: String,
    pub warnings: Vec<String>,
}

impl AppSnapshot {
    /// Fingerprint of visible data excluding `refreshed_at`, used to skip redundant redraws.
    pub fn data_fingerprint(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.repos.hash(&mut h);
        self.work.hash(&mut h);
        for w in &self.work {
            self.work_item_stale_info(w).hash(&mut h);
        }
        self.lane.hash(&mut h);
        self.events.hash(&mut h);
        self.dag_queue.hash(&mut h);
        self.merges.hash(&mut h);
        self.candidates.hash(&mut h);
        self.agy_accounts.hash(&mut h);
        self.analytics.hash(&mut h);
        self.eta.hash(&mut h);
        self.host_load.hash(&mut h);
        self.warnings.hash(&mut h);
        h.finish()
    }

    pub fn work_item_stale_info(&self, item: &WorkItem) -> Option<(u64, bool)> {
        if item.owner.is_some() {
            return None;
        }
        let threshold_secs = match item.state.as_str() {
            "verify" => std::env::var("DELEGATE_WORK_STALE_VERIFY_HOURS")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| *v > 0.0)
                .map(|h| (h * 3600.0).round() as u64)
                .unwrap_or(WORK_STALE_VERIFY_SECS),
            "ready" => std::env::var("DELEGATE_WORK_STALE_READY_HOURS")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| *v > 0.0)
                .map(|h| (h * 3600.0).round() as u64)
                .unwrap_or(WORK_STALE_READY_SECS),
            _ => return None,
        };
        let since_raw = if !item.state_since.is_empty() {
            item.state_since.as_str()
        } else {
            item.created_at.as_str()
        };
        let since = parse_iso_utc_epoch(since_raw)?;
        let now = self.generated_epoch.unwrap_or_else(current_utc_epoch);
        let elapsed = (now - since).max(0) as u64;
        if elapsed >= threshold_secs {
            Some((elapsed / 3600, elapsed >= threshold_secs.saturating_mul(2)))
        } else {
            None
        }
    }

    pub fn total_count(&self, group: GroupKind) -> usize {
        self.repos.iter().map(|r| r.count_group(group)).sum()
    }

    pub fn is_done_today(&self, item: &WorkItem) -> bool {
        if item.state != "done" || item.closed_kind == "dropped" {
            return false;
        }
        if self.today_ymd.is_empty() {
            return false;
        }
        let ts = if !item.state_since.is_empty() {
            item.state_since.as_str()
        } else {
            item.updated_at.as_str()
        };
        iso_to_local_ymd(ts).is_some_and(|d| d == self.today_ymd)
            || ts.starts_with(&self.today_ymd)
    }

    pub fn work_counts(&self) -> WorkCounts {
        let mut c = WorkCounts::default();
        for w in &self.work {
            if self.work_item_stale_info(w).is_some() {
                c.stale += 1;
            }
            match w.bucket() {
                WorkBucket::Owner => c.owner += 1,
                WorkBucket::NeedsAction => c.needs_action += 1,
                WorkBucket::InProgress => c.in_progress += 1,
                WorkBucket::Ready => c.ready += 1,
                WorkBucket::Waiting => c.waiting += 1,
                WorkBucket::Deferred => c.deferred += 1,
                WorkBucket::Done => {
                    c.done_total += 1;
                    if self.is_done_today(w) {
                        c.done_today += 1;
                    }
                }
                WorkBucket::Dropped => c.dropped_total += 1,
            }
        }
        c
    }

    pub fn find_run_by_id(&self, run_id: &str) -> Option<(usize, usize, &RunItem)> {
        if run_id.is_empty() {
            return None;
        }
        for (ri, repo) in self.repos.iter().enumerate() {
            for (run_i, run) in repo.runs.iter().enumerate() {
                if run.run == run_id || run.chain_ids.iter().any(|c| c == run_id) {
                    return Some((ri, run_i, run));
                }
            }
        }
        None
    }

    /// Find the most relevant `RunItem` linked to a work item (preferring active runs, then latest).
    pub fn find_run_for_work(&self, item: &WorkItem) -> Option<(usize, usize, &RunItem)> {
        let mut fallback: Option<(usize, usize, &RunItem)> = None;
        for rid in item.runs.iter().rev() {
            if let Some((ri, run_i, run)) = self.find_run_by_id(rid) {
                if matches!(run.group, GroupKind::Running | GroupKind::Queued) {
                    return Some((ri, run_i, run));
                }
                if fallback.is_none() {
                    fallback = Some((ri, run_i, run));
                }
            }
        }
        if fallback.is_none() {
            // Also check runs carrying `work_id == Some(item.id)`
            for (ri, repo) in self.repos.iter().enumerate() {
                for (run_i, run) in repo.runs.iter().enumerate() {
                    if run.work_id.as_deref() == Some(item.id.as_str()) {
                        if matches!(run.group, GroupKind::Running | GroupKind::Queued) {
                            return Some((ri, run_i, run));
                        }
                        if fallback.is_none() {
                            fallback = Some((ri, run_i, run));
                        }
                    }
                }
            }
        }
        fallback
    }

    pub fn work_id_for_run(&self, run: &RunItem) -> Option<String> {
        if let Some(wid) = &run.work_id {
            if !wid.is_empty() {
                return Some(wid.clone());
            }
        }
        for w in &self.work {
            if w.runs.iter().any(|r| {
                r == &run.run || run.chain_ids.iter().any(|cid| cid == r)
            }) {
                return Some(w.id.clone());
            }
        }
        None
    }

    pub fn work_item_by_id(&self, id: &str) -> Option<&WorkItem> {
        self.work.iter().find(|w| w.id == id)
    }

    /// Synthesize a live one-line status ("此刻在干什么") for a work item, enriched with
    /// live RunItem details when an active or pending run is attached.
    pub fn work_live_status(&self, item: &WorkItem) -> String {
        if let Some(ow) = &item.owner {
            if item.state == "done" {
                return if item.closed_kind == "dropped" {
                    format!("已否：{}", item.close_reason)
                } else if !item.close_evidence.is_empty() {
                    format!("已定：{}", item.close_evidence)
                } else {
                    "已定".to_string()
                };
            }
            if !ow.recommendation.is_empty() {
                return format!("建议：{}", ow.recommendation);
            }
            if !ow.cost.is_empty() {
                return format!("成本：{}", ow.cost);
            }
            if !item.brief.is_empty() {
                return "见卡片".to_string();
            }
            return format!("等所有者（{}）", ow.kind.label());
        }

        if item.state != "done" {
            if let Some((_, _, run)) = self.find_run_for_work(item) {
                let use_run = matches!(run.group, GroupKind::Running | GroupKind::Queued)
                    || (!run.applied
                        && matches!(item.state.as_str(), "attention" | "active" | "verify"));
                if use_run {
                    return live_run_one_liner(run);
                }
            }
        }

        match item.state.as_str() {
            "blocked" => {
                if let Some(ws) = item.wait_on_status.iter().find(|w| !w.satisfied) {
                    return clean_wait_detail(&ws.detail);
                }
                if let Some(raw) = item.wait_on.first() {
                    return format_raw_wait(raw);
                }
                if !item.focus.is_empty() {
                    return item.focus.clone();
                }
                "等前置".to_string()
            }
            "verify" => {
                if let Some(art) = item.artifacts.first() {
                    return format!("待接线 {}", art.path);
                }
                let pending = item.pending_followups();
                if pending > 0 {
                    return format!("待办 {pending} 条");
                }
                if !item.focus.is_empty() {
                    return item.focus.clone();
                }
                "待闭环".to_string()
            }
            "done" => {
                if item.closed_kind == "dropped" {
                    format!("已否：{}", item.close_reason)
                } else if !item.close_evidence.is_empty() {
                    format!("已完成：{}", item.close_evidence)
                } else {
                    "已完成".to_string()
                }
            }
            _ => {
                if !item.focus.is_empty() {
                    item.focus.clone()
                } else {
                    item.state_badge().to_string()
                }
            }
        }
    }
}

fn clean_wait_detail(detail: &str) -> String {
    if let Some((head, _)) = detail.split_once("（当前 ") {
        head.to_string()
    } else {
        detail.to_string()
    }
}

fn format_raw_wait(raw: &str) -> String {
    if let Some(wid) = raw.strip_prefix("work_closed:") {
        format!("等 {wid} 关闭")
    } else if let Some(oref) = raw.strip_prefix("owner:") {
        format!("等所有者 {oref}")
    } else if let Some(run_ref) = raw.strip_prefix("run_applied:") {
        format!("等 {run_ref} 合入")
    } else if let Some(run_ref) = raw.strip_prefix("run_delivered:") {
        format!("等 {run_ref} 交付")
    } else if let Some(ts) = raw.strip_prefix("after_time:") {
        format!("等到 {}", &ts[..ts.len().min(10)])
    } else {
        raw.to_string()
    }
}

pub fn live_run_one_liner(run: &RunItem) -> String {
    if run.question && !run.applied {
        return "提问等决定".to_string();
    }
    match run.group {
        GroupKind::Running => {
            if let Some(rev) = &run.review {
                if rev.status == "reviewing" {
                    return if rev.round > 1 {
                        format!("审查中 第{}轮", rev.round)
                    } else {
                        "审查中".to_string()
                    };
                }
                if rev.status == "reworking" {
                    let r = rev.rework_rounds.max(rev.round).max(1);
                    return format!("返工中 第{r}轮");
                }
            }
            let dur = format_duration_zh(run.elapsed_seconds.or(run.age_seconds));
            let base = if run.stall_warning.is_some() {
                format!("卡住提醒 {dur}")
            } else {
                format!("运行中 {dur}")
            };
            let act_phrase = run.activity.as_ref().and_then(|act| match act.kind.as_str() {
                "running_command" => {
                    let cmd = act.cmd.as_deref().unwrap_or("");
                    let low = cmd.to_ascii_lowercase();
                    if low.contains("verify")
                        || low.contains("accept")
                        || low.contains("cargo test")
                        || low.contains("clippy")
                        || low.contains("pytest")
                        || low.contains("pnpm test")
                    {
                        Some("在跑验收".to_string())
                    } else if !cmd.is_empty() {
                        let first = cmd.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
                        Some(format!("在跑 {first}"))
                    } else {
                        Some("在跑命令".to_string())
                    }
                }
                "thinking" => Some("在思考".to_string()),
                "waiting" => Some("等待中".to_string()),
                "quota" => Some("等配额".to_string()),
                "error" => Some("报错".to_string()),
                _ => None,
            });
            match act_phrase {
                Some(p) => format!("{base} · {p}"),
                None => base,
            }
        }
        GroupKind::Queued => {
            if let Some(ahead) = run.queue_ahead.or(run.measure_waiting_ahead) {
                format!("排队中 · 前方 {ahead} 位")
            } else if let Some(reason) = &run.queue_reason {
                format!("排队中 · {reason}")
            } else {
                "排队中".to_string()
            }
        }
        GroupKind::Pending => {
            if !run.applied
                && (run.state == "delivered"
                    || (run.state == "answered" && run.pending_files > 0))
            {
                if let Some(pm) = &run.premerge {
                    let lbl = pm.full_label();
                    if !lbl.is_empty() {
                        return lbl;
                    }
                }
                let mut tags = Vec::new();
                match &run.accept {
                    AcceptStatus::Passed { .. } => tags.push("验收✓".to_string()),
                    AcceptStatus::Failed { .. } => tags.push("验收✗".to_string()),
                    AcceptStatus::NotExecuted { .. } => tags.push("验收待".to_string()),
                    AcceptStatus::None => {}
                }
                if let Some(rev) = &run.review {
                    match rev.verdict.as_deref() {
                        Some("PASS") => tags.push("审查 PASS".to_string()),
                        Some("NEEDS_CONTROLLER") => tags.push("需主控审查".to_string()),
                        Some("REWORK") => tags.push("退回返工".to_string()),
                        _ => {}
                    }
                }
                if tags.is_empty() {
                    return "待合入".to_string();
                }
                return format!("待合入 · {}", tags.join(" "));
            }
            if run.state == "answered" && !run.applied {
                return "待回答/有新答复".to_string();
            }
            format!("需处理 · {}", run.state_badge())
        }
        GroupKind::Collected => "已收取".to_string(),
        GroupKind::MergedUncleaned => "已合入待清理".to_string(),
        GroupKind::Stopped => "已停止".to_string(),
    }
}

pub fn current_utc_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse an ISO-8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) or date (`YYYY-MM-DD`) to UTC epoch seconds.
pub fn parse_iso_utc_epoch(iso: &str) -> Option<i64> {
    let s = iso.trim();
    let (y, mo, d, h, mi, sec): (i32, i32, i32, i32, i32, i32) =
        if s.len() >= 19 && s.ends_with('Z') {
            (
                s[0..4].parse().ok()?,
                s[5..7].parse().ok()?,
                s[8..10].parse().ok()?,
                s[11..13].parse().ok()?,
                s[14..16].parse().ok()?,
                s[17..19].parse().ok()?,
            )
        } else if s.len() == 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' {
            (
                s[0..4].parse().ok()?,
                s[5..7].parse().ok()?,
                s[8..10].parse().ok()?,
                0,
                0,
                0,
            )
        } else {
            return None;
        };
    // Howard Hinnant civil-from-days inverse for UTC epoch seconds (avoids mutating TZ)
    let y_adj = y - i32::from(mo <= 2);
    let era = y_adj.div_euclid(400);
    let yoe = (y_adj - era * 400) as i64;
    let m_adj = (mo + if mo > 2 { -3 } else { 9 }) as i64;
    let doy = (153 * m_adj + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era as i64 * 146097 + doe - 719468;
    Some(days * 86400 + h as i64 * 3600 + mi as i64 * 60 + sec as i64)
}

/// Parse an ISO-8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) and convert to local `YYYY-MM-DD`.
pub fn iso_to_local_ymd(iso: &str) -> Option<String> {
    let s = iso.trim();
    if s.len() < 19 || !s.ends_with('Z') {
        if s.len() >= 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' {
            return Some(s[..10].to_string());
        }
        return None;
    }
    let utc_secs = parse_iso_utc_epoch(s)? as libc::time_t;
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&utc_secs, &mut tm).is_null() {
            return Some(s[..10].to_string());
        }
        Some(format!(
            "{:04}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        ))
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
