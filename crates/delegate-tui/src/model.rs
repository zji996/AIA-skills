use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

/// Pending runs inactive for more than 24 hours are folded by default.
pub const STALE_PENDING_SECS: u64 = 24 * 3600;
/// Other repositories with no activity for more than 7 days are folded by default.
pub const STALE_REPO_SECS: u64 = 7 * 24 * 3600;
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
pub struct RemoteExecInfo {
    pub host: String,
    pub active: bool,
    pub running: bool,
    pub queue: Option<String>,
    pub cmd: Option<String>,
    pub elapsed_seconds: Option<u64>,
    pub output_tail: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChainRoundInfo {
    pub round: usize,
    pub run: String,
    pub kind: String,
    pub state: String,
    pub question: bool,
    pub elapsed_seconds: Option<u64>,
    pub path: Option<String>,
    pub total_lines: Option<usize>,
    pub total_chars: Option<usize>,
    pub current: bool,
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
            // Rework round is rendered once in the Chinese status column (`返工第 N 轮`),
            // never duplicated in English `[REWORK 第N轮]` next to the task name.
            String::new()
        } else {
            match self.verdict.as_deref() {
                Some("PASS") => "PASS".to_string(),
                Some("REWORK") => {
                    let r = self.rework_rounds.max(self.round).max(1);
                    format!("返工第 {r} 轮")
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
            format!("返工第 {r} 轮（返工中）")
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
                    format!("返工第 {r} 轮")
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
    pub deferred_count: usize,
    pub earliest_deferred_clock: Option<String>,
    pub date_wait_count: usize,
    pub earliest_date_wait_clock: Option<String>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CardL0Info {
    pub risk_count: usize,
    pub max_severity: Option<String>,
    pub fast_forward: Option<bool>,
    pub tests_added: u64,
    pub summary: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CardRiskEntry {
    pub severity: String,
    pub category: String,
    pub path: String,
    pub line: Option<u64>,
    pub added: u64,
    pub deleted: u64,
    pub reason: String,
    pub source: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunCardL1 {
    pub risks: Vec<CardRiskEntry>,
    pub rule_hits: usize,
    pub reviewer_hints: usize,
    pub tests_added: u64,
    pub tests_modified: u64,
    pub tests_deleted: u64,
    pub review_verdict: String,
    pub review_summary: String,
    pub review_round: u64,
    pub fast_forward: Option<bool>,
    pub accept_ok: Option<bool>,
    pub accept_label: String,
    pub accept_layers_passed: usize,
    pub accept_layers_total: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct RecentDeliveryItem {
    pub run: String,
    pub name: String,
    pub repo: String,
    pub source_root: String,
    pub applied_at: String,
    pub applied_epoch: i64,
    pub applied_head: String,
    pub changes_files: usize,
    pub added: i64,
    pub deleted: i64,
    pub elapsed_seconds: i64,
    pub rounds: usize,
    pub conclusion: String,
    pub work_id: Option<String>,
    pub cleaned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AlertLevel {
    P0,
    P1,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActiveAlertItem {
    pub level: AlertLevel,
    pub kind: String,
    pub label: String,
    pub message: String,
    pub target_tab: u8,
    pub target_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct SnapshotPulse {
    pub owner_open: usize,
    pub controller_pending: usize,
    pub stale_pending: usize,
    pub running_runs: usize,
    pub queued_runs: usize,
    pub work_active_pool: usize,
    pub work_deferred: usize,
    pub work_timed: usize,
    pub merged_today: usize,
    pub pending_has_urgent: bool,
    pub quota_alert: bool,
    pub p0_alert: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct DiskVolumeSnapshot {
    pub root: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub avail_bytes: u64,
    /// Used percentage in tenths (e.g. 852 => 85.2%).
    pub used_pct_tenths: u32,
    pub level: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct StaleSlotRunItem {
    pub run: String,
    pub name: String,
    pub slot: String,
    pub idle_hours: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct DiskHealthSnapshot {
    pub configured: bool,
    pub level: String,
    pub primary: Option<DiskVolumeSnapshot>,
    pub overflow: Option<DiskVolumeSnapshot>,
    pub total_slots: usize,
    pub overflow_slots: Vec<String>,
    pub stale_runs: Vec<StaleSlotRunItem>,
}

impl DiskHealthSnapshot {
    pub fn should_display_in_overview(&self) -> bool {
        self.configured
            || !self.overflow_slots.is_empty()
            || !self.stale_runs.is_empty()
            || matches!(self.level.as_str(), "warn" | "high" | "critical")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgyAccountSwitchInfo {
    pub from: String,
    pub to: String,
    pub reason: String,
}

impl AgyAccountSwitchInfo {
    pub fn format_line(&self, is_reviewer: bool) -> String {
        let detail = match (is_reviewer, self.reason.is_empty()) {
            (true, true) => "审查员".to_string(),
            (true, false) => format!("审查员：{}", self.reason),
            (false, true) => "原槽位不可用".to_string(),
            (false, false) => self.reason.clone(),
        };
        format!("本 run 已从 {} 换到 {}（{detail}）", self.from, self.to)
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
    pub appended_total: u64,
    pub appended_pending: u64,
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
    pub card_l0: Option<CardL0Info>,
    pub remote_exec: Option<RemoteExecInfo>,
    pub chain_rounds: Vec<ChainRoundInfo>,
    pub agy_account_switch: Option<AgyAccountSwitchInfo>,
    pub review_agy_account_switch: Option<AgyAccountSwitchInfo>,
}

impl RunItem {
    /// Original task name plus `（第 N 轮）` when part of a multi-round reply chain.
    pub fn display_name_with_round(&self) -> String {
        if self.chain_round > 1 {
            format!("{}（第 {} 轮）", self.display_name, self.chain_round)
        } else {
            self.display_name.clone()
        }
    }

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

    pub fn is_urgent_pending(&self) -> bool {
        if self.group != GroupKind::Pending {
            return false;
        }
        let pm_bad = self
            .premerge
            .as_ref()
            .is_some_and(|p| matches!(p.status.as_str(), "conflict" | "failed"));
        let state_bad = matches!(
            self.state.as_str(),
            "failed" | "crashed" | "timeout" | "stalled" | "malformed" | "rejected"
        ) && self.result_read_at.is_none();
        let needs_ctrl = !self.applied
            && self.review.as_ref().is_some_and(|r| {
                r.verdict.as_deref() == Some("NEEDS_CONTROLLER") || r.needs_attention
            });
        if pm_bad
            || self.has_protect_violation()
            || !self.conflict_markers.is_empty()
            || state_bad
            || needs_ctrl
        {
            return true;
        }
        let age = self.last_activity_seconds();
        if age != u64::MAX {
            return (self.question && age >= 600) || (!self.question && age >= 1800);
        }
        false
    }

    pub fn state_badge(&self) -> Cow<'static, str> {
        if self.question && !self.applied {
            return Cow::Borrowed("等决定");
        }
        if matches!(self.state.as_str(), "running" | "starting") {
            if let Some(rev) = &self.review {
                if rev.status == "reviewing" {
                    return Cow::Borrowed("审查中");
                }
                if rev.status == "reworking" {
                    let r = rev.rework_rounds.max(rev.round).max(1);
                    return Cow::Owned(format!("返工{r}轮"));
                }
            }
            if self.stall_warning.is_some() {
                return Cow::Borrowed("卡住警");
            }
            if let Some(act) = &self.activity {
                return Cow::Borrowed(match act.kind.as_str() {
                    "running_command" => "跑命令",
                    "accepting" | "running_accept" => "跑验收",
                    "thinking" => "思考中",
                    "waiting" => "排队中",
                    "quota" => "限流中",
                    "error" => "报错中",
                    _ => "运行中",
                });
            }
        }
        if !self.applied {
            if let Some(pm) = &self.premerge {
                if pm.status == "conflict" {
                    return Cow::Borrowed("预冲突");
                }
                if pm.status == "failed" {
                    return Cow::Borrowed("预失败");
                }
            }
            if let Some(rev) = &self.review {
                if rev.verdict.as_deref() == Some("NEEDS_CONTROLLER") {
                    return Cow::Borrowed("需主审");
                }
            }
        }
        Cow::Borrowed(match self.state.as_str() {
            "running" => "运行中",
            "starting" => "启动中",
            "waiting" | "queued" => "排队中",
            "delivered" if self.applied => "已合入",
            "delivered" => "待合入",
            "answered" if self.applied => "已合入",
            "answered" if self.pending_files > 0 => "待合入",
            "answered" if self.group == GroupKind::Collected => "已收取",
            "answered" => "已答复",
            "blocked_mainline" => "主干阻",
            "rejected" => "未通过",
            "failed" => "失败",
            "timeout" => "超时",
            "stalled" => "卡住",
            "malformed" => "异常",
            "crashed" => "崩溃",
            "killed" => "强停",
            "stopped" => "已停止",
            "needs-redispatch" => "待重派",
            "cleaned" => "待办",
            _ => "未知",
        })
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
            &self.state_badge(),
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
    pub runs_dir: PathBuf,
    pub label: String,
    pub runs: Vec<RunItem>,
}

impl RepoGroup {
    pub fn effective_runs_dir(&self) -> PathBuf {
        if !self.runs_dir.as_os_str().is_empty() {
            self.runs_dir.clone()
        } else {
            self.path.join(".local/run/delegate")
        }
    }

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
    TimedWait,
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
            Self::Waiting => "依赖等待",
            Self::TimedWait => "定时等待",
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
    pub ephemeral: bool,
    pub eta: Option<WorkEtaInfo>,
}

impl WorkItem {
    /// True when this `blocked` item is waiting exclusively on `after_time:...` conditions.
    pub fn is_pure_timed_wait(&self) -> bool {
        if self.state != "blocked" {
            return false;
        }
        let unsat: Vec<&str> = self
            .wait_on_status
            .iter()
            .filter(|w| !w.satisfied)
            .map(|w| w.condition.as_str())
            .filter(|c| !c.is_empty())
            .collect();
        if !unsat.is_empty() {
            return unsat.iter().all(|c| c.starts_with("after_time:"));
        }
        !self.wait_on.is_empty() && self.wait_on.iter().all(|c| c.starts_with("after_time:"))
    }

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
            "blocked" if self.is_pure_timed_wait() => WorkBucket::TimedWait,
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
            "blocked" if self.is_pure_timed_wait() => "定时",
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
        !self.ephemeral && self.owner.is_none() && self.state == "ready" && self.brief.trim().is_empty()
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
    pub timed: usize,
    pub deferred: usize,
    pub stale: usize,
    pub done_today: usize,
    pub done_total: usize,
    pub dropped_total: usize,
}

impl WorkCounts {
    pub fn active_pool(&self) -> usize {
        self.owner + self.needs_action + self.in_progress + self.ready + self.waiting
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunningAgyProc {
    pub pid: u32,
    pub run: Option<String>,
    pub run_name: Option<String>,
    pub role: Option<String>,
    pub round: Option<String>,
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
    pub identity_mismatch: bool,
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
    pub token_input_7d: u64,
    pub token_output_7d: u64,
    pub token_cache_read_7d: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppSnapshot {
    pub repos: Vec<RepoGroup>,
    pub work: Vec<WorkItem>,
    pub lane: LaneStatus,
    pub events: Vec<EventItem>,
    pub events_has_more: bool,
    pub events_earliest_seq: Option<u64>,
    pub dag_queue: Vec<DagQueueItem>,
    pub merges: Vec<MergeRunItem>,
    pub candidates: Vec<RadarCandidateItem>,
    pub agy_accounts: AgyAccountsState,
    pub analytics: AnalyticsSnapshot,
    pub recent_deliveries: Vec<RecentDeliveryItem>,
    pub pulse: Option<SnapshotPulse>,
    pub alerts: Vec<ActiveAlertItem>,
    pub alerts_from_snapshot: bool,
    pub eta: SnapshotEta,
    pub disk_health: Option<DiskHealthSnapshot>,
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
        self.events_has_more.hash(&mut h);
        self.events_earliest_seq.hash(&mut h);
        self.dag_queue.hash(&mut h);
        self.merges.hash(&mut h);
        self.candidates.hash(&mut h);
        self.agy_accounts.hash(&mut h);
        self.analytics.hash(&mut h);
        self.recent_deliveries.hash(&mut h);
        self.pulse.hash(&mut h);
        self.alerts.hash(&mut h);
        self.eta.hash(&mut h);
        self.disk_health.hash(&mut h);
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
                WorkBucket::TimedWait => c.timed += 1,
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

    /// Single-source-of-truth pulse counters: reads `snapshot["pulse"]` emitted by `crates/delegate`
    /// when present, and falls back to the exact same definitions for synthetic in-memory fixtures
    /// or older `delegate` binaries.
    pub fn effective_pulse(&self) -> SnapshotPulse {
        if let Some(p) = &self.pulse {
            return p.clone();
        }
        let wc = self.work_counts();
        let open_work_ids: std::collections::HashSet<&str> = self
            .work
            .iter()
            .filter(|w| !w.ephemeral && w.state != "done")
            .map(|w| w.id.as_str())
            .collect();
        let open_work_runs: std::collections::HashSet<&str> = self
            .work
            .iter()
            .filter(|w| !w.ephemeral && w.state != "done")
            .flat_map(|w| w.runs.iter().map(String::as_str))
            .collect();

        let mut controller_pending = 0usize;
        let mut stale_pending = 0usize;
        let mut running_runs = 0usize;
        let mut queued_runs = 0usize;
        let mut pending_has_urgent = false;

        for repo in &self.repos {
            for r in &repo.runs {
                match r.group {
                    GroupKind::Running => running_runs += 1,
                    GroupKind::Queued => queued_runs += 1,
                    GroupKind::Pending => {
                        let attached = open_work_runs.contains(r.run.as_str())
                            || r.work_id
                                .as_deref()
                                .is_some_and(|wid| open_work_ids.contains(wid));
                        if r.is_stale_pending() && !attached {
                            stale_pending += 1;
                        } else {
                            controller_pending += 1;
                            if r.is_urgent_pending() {
                                pending_has_urgent = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        let merged_today = if !self.today_ymd.is_empty() {
            let from_daily = self
                .analytics
                .daily_7d
                .iter()
                .find(|d| d.ymd == self.today_ymd)
                .map(|d| d.merged_count as usize);
            from_daily.unwrap_or_else(|| {
                self.recent_deliveries
                    .iter()
                    .filter(|d| {
                        iso_to_local_ymd(&d.applied_at)
                            .is_some_and(|ymd| ymd == self.today_ymd)
                    })
                    .count()
            })
        } else {
            0
        };

        let alerts = self.effective_alerts();
        let p0_alert = alerts.iter().any(|a| a.level == AlertLevel::P0);
        let quota_alert = self.compute_fallback_quota_alert();

        SnapshotPulse {
            owner_open: wc.owner,
            controller_pending,
            stale_pending,
            running_runs,
            queued_runs,
            work_active_pool: wc.active_pool(),
            work_deferred: wc.deferred,
            work_timed: wc.timed,
            merged_today,
            pending_has_urgent,
            quota_alert,
            p0_alert,
        }
    }

    fn compute_fallback_quota_alert(&self) -> bool {
        if self.agy_accounts.live_drifted {
            return true;
        }
        for acct in &self.agy_accounts.accounts {
            if let Some(q) = &acct.quota {
                if q.identity_mismatch {
                    return true;
                }
                if q.ok {
                    if q.entries.iter().any(|e| e.remaining_pct_x100 < 1500) {
                        return true;
                    }
                    if let Some(est) = &acct.quota_estimate {
                        let check = |win_key: &str, w: &QuotaWindowEstimate| -> bool {
                            let Some(mins) = w.estimated_remaining_minutes else {
                                return false;
                            };
                            q.entries.iter().any(|e| {
                                e.window == win_key
                                    && w.pool
                                        .as_deref()
                                        .is_none_or(|p| p.is_empty() || e.pool == p)
                                    && e.reset_in_seconds
                                        .is_some_and(|rs| rs > 0 && mins.saturating_mul(60) < rs)
                            })
                        };
                        if est.weekly.as_ref().is_some_and(|w| check("weekly", w))
                            || est.five_hour.as_ref().is_some_and(|w| check("fiveHour", w))
                            || est.pools.iter().any(|p| {
                                p.weekly.as_ref().is_some_and(|w| check("weekly", w))
                                    || p.five_hour.as_ref().is_some_and(|w| check("fiveHour", w))
                            })
                        {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// Active P0/P1 alerts: reads `snapshot["alerts"]` from `crates/delegate/src/pulse.rs` when
    /// emitted, or mirrors the exact same P0/P1 rules for synthetic test fixtures.
    pub fn effective_alerts(&self) -> Vec<ActiveAlertItem> {
        if self.alerts_from_snapshot {
            return self.alerts.clone();
        }
        let mut p0: Vec<ActiveAlertItem> = Vec::new();
        let mut p1: Vec<ActiveAlertItem> = Vec::new();
        let now = self.generated_epoch.unwrap_or_else(current_utc_epoch);

        let open_work_ids: std::collections::HashSet<&str> = self
            .work
            .iter()
            .filter(|w| !w.ephemeral && w.state != "done")
            .map(|w| w.id.as_str())
            .collect();
        let open_work_runs: std::collections::HashSet<&str> = self
            .work
            .iter()
            .filter(|w| !w.ephemeral && w.state != "done")
            .flat_map(|w| w.runs.iter().map(String::as_str))
            .collect();

        // P0: unhandled chain-head failure / violation / premerge conflict.
        let mut stalled: Vec<(&RunItem, u64)> = Vec::new();
        for repo in &self.repos {
            for r in &repo.runs {
                if r.group != GroupKind::Pending {
                    continue;
                }
                let attached = open_work_runs.contains(r.run.as_str())
                    || r.work_id
                        .as_deref()
                        .is_some_and(|wid| open_work_ids.contains(wid));
                if r.is_stale_pending() && !attached {
                    continue;
                }
                let pm_bad = r
                    .premerge
                    .as_ref()
                    .is_some_and(|p| matches!(p.status.as_str(), "conflict" | "failed"));
                let state_bad = matches!(
                    r.state.as_str(),
                    "failed" | "crashed" | "timeout" | "stalled" | "malformed" | "rejected"
                ) && r.result_read_at.is_none();
                if pm_bad || r.has_protect_violation() || !r.conflict_markers.is_empty() || state_bad
                {
                    let reason = if let Some(pm) = &r.premerge {
                        if pm.status == "conflict" {
                            pm.full_label()
                        } else if pm.status == "failed" {
                            "预合并失败".to_string()
                        } else {
                            r.state_badge().into_owned()
                        }
                    } else if r.has_protect_violation() {
                        "受控路径违规".to_string()
                    } else {
                        r.state_badge().into_owned()
                    };
                    p0.push(ActiveAlertItem {
                        level: AlertLevel::P0,
                        kind: if pm_bad {
                            "premerge_conflict".to_string()
                        } else {
                            "run_failure".to_string()
                        },
                        label: "失败/冲突".to_string(),
                        message: format!("{} {reason} → 按 3 查审查卡处理", r.display_name),
                        target_tab: 3,
                        target_id: Some(r.run.clone()),
                    });
                } else {
                    let age = r.last_activity_seconds();
                    if age != u64::MAX
                        && ((r.question && age >= 600) || (!r.question && age >= 1800))
                    {
                        stalled.push((r, age));
                    }
                }
            }
        }

        // P0: active agy account drift / identity mismatch only (advisory).
        // Quota depletion and <15% low quota stay out of the global top alert strip to avoid layout jitter;
        // they remain visible on the `4 额度` tab, the Overview quota card, and the `4 额度(!)` badge.
        if let Some(active) = self.agy_accounts.accounts.iter().find(|a| a.active) {
            let short = mask_account_name(&active.id);
            if self.agy_accounts.live_drifted {
                p0.push(ActiveAlertItem {
                    level: AlertLevel::P0,
                    kind: "quota_drift".to_string(),
                    label: "额度·登录态漂移".to_string(),
                    message: format!("★{short} 当前本机登录态与记录不一致 → 按 4 核对"),
                    target_tab: 4,
                    target_id: Some(active.id.clone()),
                });
            } else if active.quota.as_ref().is_some_and(|q| q.identity_mismatch) {
                p0.push(ActiveAlertItem {
                    level: AlertLevel::P0,
                    kind: "quota_drift".to_string(),
                    label: "额度·身份不匹配".to_string(),
                    message: format!("★{short} 额度缓存账号身份不匹配 → 按 4 核对"),
                    target_tab: 4,
                    target_id: Some(active.id.clone()),
                });
            }
        }

        // P1: owner open blocking downstream or > 2h.
        let mut owner_downstream: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for w in &self.work {
            if w.ephemeral || w.state == "done" {
                continue;
            }
            for c in &w.wait_on {
                if let Some(t) = c
                    .strip_prefix("owner:")
                    .or_else(|| c.strip_prefix("work:"))
                    .or_else(|| c.strip_prefix("work_done:"))
                {
                    *owner_downstream.entry(t).or_insert(0) += 1;
                }
            }
        }
        let mut urgent_owners: Vec<(usize, i64, &WorkItem)> = Vec::new();
        for w in &self.work {
            if w.ephemeral || w.state == "done" || w.owner.is_none() {
                continue;
            }
            let ds = owner_downstream.get(w.id.as_str()).copied().unwrap_or(0);
            let since_raw = if !w.state_since.is_empty() {
                w.state_since.as_str()
            } else {
                w.created_at.as_str()
            };
            let waited = parse_iso_utc_epoch(since_raw)
                .map(|ep| (now - ep).max(0))
                .unwrap_or(0);
            if ds > 0 || waited > 7200 {
                urgent_owners.push((ds, waited, w));
            }
        }
        urgent_owners.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        if let Some((ds, waited, top)) = urgent_owners.first() {
            let detail = if *ds > 0 {
                format!("阻下游 {ds}")
            } else {
                format!("已等 {}", format_duration(Some(*waited as u64)))
            };
            p1.push(ActiveAlertItem {
                level: AlertLevel::P1,
                kind: "owner_blocked".to_string(),
                label: "等你拍板".to_string(),
                message: format!("[{}] {}（{detail}）→ 按 2 处理", top.id, top.display_title()),
                target_tab: 2,
                target_id: Some(top.id.clone()),
            });
        }

        // P1: controller stalled runs.
        stalled.sort_by(|a, b| b.0.question.cmp(&a.0.question).then_with(|| b.1.cmp(&a.1)));
        if let Some((top, age)) = stalled.first() {
            let dur = format_duration(Some(*age));
            let extra = if stalled.len() > 1 {
                format!(" 等{}单", stalled.len())
            } else {
                String::new()
            };
            let note = if top.question {
                "提问等决定"
            } else if top
                .premerge
                .as_ref()
                .is_some_and(|p| p.status == "ready")
            {
                "已PASS且快进✓待合入"
            } else {
                "待主控处理"
            };
            p1.push(ActiveAlertItem {
                level: AlertLevel::P1,
                kind: "controller_stalled".to_string(),
                label: format!("卡主控 {dur}"),
                message: format!("{}{extra} {note} → 按 3", top.display_name),
                target_tab: 3,
                target_id: Some(top.run.clone()),
            });
        }

        if let Some(ref dh) = self.disk_health {
            if dh.level == "critical" {
                p0.insert(
                    0,
                    ActiveAlertItem {
                        level: AlertLevel::P0,
                        kind: "disk_critical".to_string(),
                        label: "磁盘临界".to_string(),
                        message: "磁盘水位临界 → 运行 delegate disk prune".to_string(),
                        target_tab: 1,
                        target_id: None,
                    },
                );
            } else if dh.level == "high" {
                p1.push(ActiveAlertItem {
                    level: AlertLevel::P1,
                    kind: "disk_high".to_string(),
                    label: "磁盘高水位".to_string(),
                    message: "磁盘超过高水位 → 可运行 delegate disk prune".to_string(),
                    target_tab: 1,
                    target_id: None,
                });
            }
            if let Some(top) = dh.stale_runs.first() {
                p1.push(ActiveAlertItem {
                    level: AlertLevel::P1,
                    kind: "disk_stale_slot".to_string(),
                    label: "长期占槽".to_string(),
                    message: format!(
                        "{}({}) 已 {}h 无进展占 {:.1} GiB → 检查 apply/clean（不自动删）",
                        top.name,
                        top.slot,
                        top.idle_hours,
                        top.bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                    ),
                    target_tab: 3,
                    target_id: Some(top.run.clone()),
                });
            }
        }

        p0.extend(p1);
        p0
    }

    /// Recent deliveries from `snapshot["recentDeliveries"]`, falling back to applied runs in `repos`
    /// when reading older snapshot versions.
    pub fn effective_recent_deliveries(&self) -> Vec<RecentDeliveryItem> {
        if !self.recent_deliveries.is_empty() {
            return self.recent_deliveries.clone();
        }
        let mut out = Vec::new();
        for repo in &self.repos {
            let short_repo = repo
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&repo.label)
                .to_string();
            for r in &repo.runs {
                if r.applied {
                    let (f, add, del) = r.display_changes();
                    let applied_at = r.finished_at.clone().unwrap_or_default();
                    let applied_epoch = parse_iso_utc_epoch(&applied_at).unwrap_or(0);
                    out.push(RecentDeliveryItem {
                        run: r.run.clone(),
                        name: r.display_name.clone(),
                        repo: short_repo.clone(),
                        source_root: repo.path.to_string_lossy().into_owned(),
                        applied_at,
                        applied_epoch,
                        applied_head: String::new(),
                        changes_files: f as usize,
                        added: add as i64,
                        deleted: del as i64,
                        elapsed_seconds: r.elapsed_seconds.unwrap_or(0) as i64,
                        rounds: r.chain_round.max(1),
                        conclusion: String::new(),
                        work_id: r.work_id.clone(),
                        cleaned: false,
                    });
                }
            }
        }
        out.sort_by_key(|b| std::cmp::Reverse(b.applied_epoch));
        out
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

    /// Resolve a run ID (e.g. `20261125-115613-phys2-t02-throughput` or a reply run
    /// `20261125-115816-7ddc`) to a human-readable run/chain display name.
    pub fn resolve_run_display_name(&self, run_id: &str) -> String {
        for repo in &self.repos {
            for r in &repo.runs {
                if r.run == run_id || r.chain_ids.iter().any(|c| c == run_id) {
                    if !r.display_name.is_empty() {
                        return r.display_name.clone();
                    }
                    if !r.name.is_empty() {
                        return r.name.clone();
                    }
                }
            }
        }
        strip_run_id_timestamp(run_id).to_string()
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

        let raw = match item.state.as_str() {
            "blocked" => {
                if let Some(ws) = item.wait_on_status.iter().find(|w| !w.satisfied) {
                    self.replace_run_ids_in_text(&clean_wait_detail(&ws.detail))
                } else if let Some(raw) = item.wait_on.first() {
                    self.format_raw_wait(raw)
                } else if !item.focus.is_empty() {
                    self.replace_run_ids_in_text(&item.focus)
                } else {
                    "等前置".to_string()
                }
            }
            "verify" => {
                if let Some(art) = item.artifacts.first() {
                    format!("待接线 {}", art.path)
                } else {
                    let pending = item.pending_followups();
                    if pending > 0 {
                        format!("待办 {pending} 条")
                    } else if !item.focus.is_empty() {
                        item.focus.clone()
                    } else {
                        "待闭环".to_string()
                    }
                }
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
                    self.replace_run_ids_in_text(&item.focus)
                } else {
                    item.state_badge().to_string()
                }
            }
        };
        localize_inline_timestamps(&raw, &self.today_ymd)
    }

    fn format_raw_wait(&self, raw: &str) -> String {
        if let Some(wid) = raw.strip_prefix("work_closed:") {
            format!("等 {wid} 关闭")
        } else if let Some(wid) = raw.strip_prefix("work_applied:") {
            format!("等 {wid} 合入")
        } else if let Some(oref) = raw.strip_prefix("owner:") {
            format!("等所有者 {oref}")
        } else if let Some(run_ref) = raw.strip_prefix("run_applied:") {
            format!("等 {} 合入", self.resolve_run_display_name(run_ref))
        } else if let Some(run_ref) = raw.strip_prefix("run_delivered:") {
            format!("等 {} 交付", self.resolve_run_display_name(run_ref))
        } else if let Some(ts) = raw.strip_prefix("after_time:") {
            if let Some(loc) = format_iso_local_short(ts, &self.today_ymd) {
                format!("等到 {loc}")
            } else {
                format!("等到 {}", &ts[..ts.len().min(10)])
            }
        } else {
            raw.to_string()
        }
    }

    fn replace_run_ids_in_text(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut cur = text;
        while let Some(pos) = find_run_id_start(cur) {
            out.push_str(&cur[..pos]);
            let rest = &cur[pos..];
            let end = rest
                .find(|c: char| {
                    c.is_whitespace()
                        || matches!(
                            c,
                            '(' | ')' | '（' | '）' | '[' | ']' | ',' | '，' | '·' | '；' | '：'
                        )
                })
                .unwrap_or(rest.len());
            let token = &rest[..end];
            out.push_str(&self.resolve_run_display_name(token));
            cur = &rest[end..];
        }
        out.push_str(cur);
        out
    }
}

fn find_run_id_start(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.len() < 16 {
        return None;
    }
    for i in 0..=bytes.len() - 16 {
        if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'-' || bytes[i - 1] == b'_') {
            continue;
        }
        if is_run_id_prefix(&bytes[i..i + 16]) {
            return Some(i);
        }
    }
    None
}

fn is_run_id_prefix(b: &[u8]) -> bool {
    b.len() >= 16
        && b[0..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'-'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'-'
}

/// Strip leading `YYYYMMDD-HHMMSS-` timestamp prefix from a run ID if followed by a non-empty name.
pub fn strip_run_id_timestamp(id: &str) -> &str {
    let b = id.as_bytes();
    if is_run_id_prefix(b) && id.len() > 16 {
        &id[16..]
    } else {
        id
    }
}

fn clean_wait_detail(detail: &str) -> String {
    if let Some((head, _)) = detail.split_once("（当前 ") {
        head.to_string()
    } else {
        detail.to_string()
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
                        let short = shorten_running_cmd(cmd);
                        if short.is_empty() {
                            Some("在跑命令".to_string())
                        } else {
                            Some(format!("在跑 {short}"))
                        }
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

/// Parse an ISO-8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`, `YYYY-MM-DDTHH:MMZ`,
/// `YYYY-MM-DDTHH:MM`, or date `YYYY-MM-DD`) to UTC epoch seconds.
pub fn parse_iso_utc_epoch(iso: &str) -> Option<i64> {
    let s = iso.trim();
    if !s.is_ascii() {
        return None;
    }
    let b = s.as_bytes();
    let (y, mo, d, h, mi, sec): (i32, i32, i32, i32, i32, i32) =
        if s.len() >= 19 && b[4] == b'-' && b[7] == b'-' && b[10] == b'T' && b[13] == b':' && b[16] == b':' {
            (
                s[0..4].parse().ok()?,
                s[5..7].parse().ok()?,
                s[8..10].parse().ok()?,
                s[11..13].parse().ok()?,
                s[14..16].parse().ok()?,
                s[17..19].parse().ok()?,
            )
        } else if s.len() >= 16 && b[4] == b'-' && b[7] == b'-' && b[10] == b'T' && b[13] == b':' {
            (
                s[0..4].parse().ok()?,
                s[5..7].parse().ok()?,
                s[8..10].parse().ok()?,
                s[11..13].parse().ok()?,
                s[14..16].parse().ok()?,
                0,
            )
        } else if s.len() == 10 && b[4] == b'-' && b[7] == b'-' {
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

fn utc_epoch_to_local_tm(utc_secs: i64) -> Option<libc::tm> {
    let t = utc_secs as libc::time_t;
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            None
        } else {
            Some(tm)
        }
    }
}

/// Format an ISO-8601 UTC timestamp in local time:
/// - today (`YYYY-MM-DD == today_ymd`): `HH:MM`
/// - other day: `MM-DD HH:MM`
pub fn format_iso_local_short(iso: &str, today_ymd: &str) -> Option<String> {
    let s = iso.trim();
    if s.len() < 16 || !s.contains('T') {
        return None;
    }
    let ep = parse_iso_utc_epoch(s)?;
    let tm = utc_epoch_to_local_tm(ep)?;
    let ymd = format!(
        "{:04}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday
    );
    if !today_ymd.is_empty() && ymd == today_ymd {
        Some(format!("{:02}:{:02}", tm.tm_hour, tm.tm_min))
    } else {
        Some(format!(
            "{:02}-{:02} {:02}:{:02}",
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min
        ))
    }
}

/// Format an ISO-8601 UTC timestamp in local time with seconds:
/// - today (`YYYY-MM-DD == today_ymd`): `HH:MM:SS`
/// - other day: `MM-DD HH:MM:SS`
pub fn format_iso_local_hms(iso: &str, today_ymd: &str) -> Option<String> {
    let s = iso.trim();
    if s.len() < 16 || !s.contains('T') {
        return None;
    }
    let ep = parse_iso_utc_epoch(s)?;
    let tm = utc_epoch_to_local_tm(ep)?;
    let ymd = format!(
        "{:04}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday
    );
    if !today_ymd.is_empty() && ymd == today_ymd {
        Some(format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec))
    } else {
        Some(format!(
            "{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        ))
    }
}

/// Replace any embedded UTC ISO timestamps (`YYYY-MM-DDTHH:MM[:SS][Z]`) inside `text`
/// with local short timestamps (`HH:MM` for today, `MM-DD HH:MM` for other days).
pub fn localize_inline_timestamps(text: &str, today_ymd: &str) -> String {
    let bytes = text.as_bytes();
    if bytes.len() < 16 {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 16 <= bytes.len()
            && bytes[i..i + 4].iter().all(u8::is_ascii_digit)
            && bytes[i + 4] == b'-'
            && bytes[i + 5..i + 7].iter().all(u8::is_ascii_digit)
            && bytes[i + 7] == b'-'
            && bytes[i + 8..i + 10].iter().all(u8::is_ascii_digit)
            && bytes[i + 10] == b'T'
            && bytes[i + 11..i + 13].iter().all(u8::is_ascii_digit)
            && bytes[i + 13] == b':'
            && bytes[i + 14..i + 16].iter().all(u8::is_ascii_digit)
        {
            let mut end = i + 16;
            if end + 3 <= bytes.len()
                && bytes[end] == b':'
                && bytes[end + 1..end + 3].iter().all(u8::is_ascii_digit)
            {
                end += 3;
                // Optional fractional seconds `.123`
                if end < bytes.len() && bytes[end] == b'.' {
                    let mut f = end + 1;
                    while f < bytes.len() && bytes[f].is_ascii_digit() {
                        f += 1;
                    }
                    end = f;
                }
            }
            if end < bytes.len() && bytes[end] == b'Z' {
                end += 1;
            }
            if let Some(loc) = format_iso_local_short(&text[i..end], today_ymd) {
                out.push_str(&loc);
                i = end;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Shorten a raw shell command for compact running-row badges:
/// - strips leading `KEY=VAL` env assignments (when followed by a command)
/// - strips `delegate lane` / `/path/to/delegate lane` wrapper (and its flags)
/// - strips leading `cd <dir> &&` or `bash -lc` wrapper
/// - replaces absolute paths on the executable with their basename, and strips `/home/<user>/` prefix.
pub fn shorten_running_cmd(raw: &str) -> String {
    let mut s = raw.trim();
    if s.is_empty() {
        return String::new();
    }
    // Strip leading `cd ... && ` if present
    if let Some(rest) = s.strip_prefix("cd ") {
        if let Some((_, after)) = rest.split_once("&&") {
            s = after.trim();
        }
    }
    // Strip outer `bash -lc '...'` / `sh -c '...'` (even if closing quote was truncated)
    for prefix in ["bash -lc ", "bash -c ", "sh -lc ", "sh -c "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest
                .trim()
                .trim_start_matches(['\'', '"'])
                .trim_end_matches(['\'', '"'])
                .trim();
            break;
        }
    }
    s = s.trim_start_matches(['\'', '"']).trim();

    let tokens: Vec<&str> = s.split_whitespace().collect();
    if tokens.is_empty() {
        return String::new();
    }
    let mut idx = 0;
    // Skip leading `env` and `FOO=bar` env assignments.
    if tokens.get(idx).copied() == Some("env") {
        idx += 1;
    }
    while idx < tokens.len()
        && tokens[idx].contains('=')
        && !tokens[idx].starts_with('-')
        && !tokens[idx].starts_with('/')
    {
        idx += 1;
    }
    if idx >= tokens.len() {
        return String::new();
    }
    // Skip `delegate lane [flags] [--]` wrapper
    let exe_base = tokens[idx].rsplit('/').next().unwrap_or(tokens[idx]);
    if exe_base == "delegate" && tokens.get(idx + 1).copied() == Some("lane") {
        idx += 2;
        while idx < tokens.len() {
            let tok = tokens[idx];
            if tok == "--" {
                idx += 1;
                break;
            } else if tok == "--label" || tok == "--timeout" || tok == "--repo" {
                idx += 2;
            } else if tok.starts_with("--") {
                idx += 1;
            } else {
                break;
            }
        }
    }
    // Skip `bash -lc` / `sh -c` if it appeared after `delegate lane`
    if matches!(
        tokens.get(idx).copied(),
        Some("bash" | "sh")
    ) && matches!(tokens.get(idx + 1).copied(), Some("-lc" | "-c"))
    {
        idx += 2;
    }
    if idx >= tokens.len() {
        return "delegate lane".to_string();
    }

    // Build compact 3-token command summary with executable basename and stripped HOME prefixes.
    let mut parts: Vec<String> = Vec::new();
    for (j, &raw_tok) in tokens[idx..].iter().take(4).enumerate() {
        let tok = raw_tok.trim_matches(['\'', '"']);
        if matches!(tok, "&&" | "||" | ";" | "|") {
            break;
        }
        let cleaned = if j == 0 {
            tok.rsplit('/').next().unwrap_or(tok).to_string()
        } else if let Some(after_home) = strip_home_prefix(tok) {
            after_home
        } else {
            tok.to_string()
        };
        if !cleaned.is_empty() {
            parts.push(cleaned);
        }
        if parts.len() >= 3 {
            break;
        }
    }
    parts.join(" ")
}

fn strip_home_prefix(tok: &str) -> Option<String> {
    if let Some(rest) = tok.strip_prefix("/home/") {
        if let Some((_, after_user)) = rest.split_once('/') {
            // If it's a binary in ~/.local/bin/foo, just return foo; otherwise ~/...
            if let Some(bin) = after_user.strip_prefix(".local/bin/") {
                return Some(bin.to_string());
            }
            return Some(format!("~/{after_user}"));
        }
    }
    None
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
