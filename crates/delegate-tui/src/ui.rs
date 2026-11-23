use crate::app::{
    App, CollapseKind, FocusPane, ListRow, Overlay, ViewTab, WorkFilterMode, WorkFoldKind, WorkRow,
};
use crate::data::read_work_brief_markdown;
use crate::markdown::render_markdown;
use crate::model::*;
use crate::width::{display_width, pad_left, pad_right, truncate, wrap_text};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

const NARROW_BREAKPOINT: u16 = 100;
const COMPACT_BREAKPOINT: u16 = 70;

// Strict semantic palette: 4 status colors + adaptive grayscale (Color::Reset / Color::DarkGray).
// Works cleanly on both dark and light terminals without hard-coded RGB backgrounds.
const STATUS_PENDING: Color = Color::Yellow;
const STATUS_RUNNING: Color = Color::Green;
const STATUS_QUEUED: Color = Color::Cyan;
const STATUS_FAILED: Color = Color::Red;

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < 20 || area.height < 8 {
        frame.render_widget(Paragraph::new("窗口过小"), area);
        return;
    }

    let top_h: u16 = if app.tab == ViewTab::Work && area.width < COMPACT_BREAKPOINT {
        2
    } else {
        1
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top_h),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(area);

    render_top_bar(frame, app, chunks[0]);

    if app.tab == ViewTab::Work {
        render_work_pane(frame, app, chunks[1]);
    } else if area.width >= NARROW_BREAKPOINT {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
            .split(chunks[1]);
        match app.tab {
            ViewTab::Runs => {
                render_list_pane(frame, app, body[0]);
                render_detail_pane(frame, app, body[1]);
            }
            ViewTab::Events => {
                render_events_list_pane(frame, app, body[0]);
                render_event_detail_pane(frame, app, body[1]);
            }
            ViewTab::Merges => {
                render_merges_list_pane(frame, app, body[0]);
                render_merge_detail_pane(frame, app, body[1]);
            }
            ViewTab::Candidates => {
                render_candidates_list_pane(frame, app, body[0]);
                render_candidate_detail_pane(frame, app, body[1]);
            }
            ViewTab::Accounts => {
                render_accounts_list_pane(frame, app, body[0]);
                render_account_detail_pane(frame, app, body[1]);
            }
            ViewTab::Work => unreachable!(),
        }
    } else {
        let body = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
            .split(chunks[1]);
        match app.tab {
            ViewTab::Runs => {
                render_list_pane(frame, app, body[0]);
                render_detail_pane(frame, app, body[1]);
            }
            ViewTab::Events => {
                render_events_list_pane(frame, app, body[0]);
                render_event_detail_pane(frame, app, body[1]);
            }
            ViewTab::Merges => {
                render_merges_list_pane(frame, app, body[0]);
                render_merge_detail_pane(frame, app, body[1]);
            }
            ViewTab::Candidates => {
                render_candidates_list_pane(frame, app, body[0]);
                render_candidate_detail_pane(frame, app, body[1]);
            }
            ViewTab::Accounts => {
                render_accounts_list_pane(frame, app, body[0]);
                render_account_detail_pane(frame, app, body[1]);
            }
            ViewTab::Work => unreachable!(),
        }
    }

    render_bottom_bar(frame, app, chunks[2]);

    match &app.overlay {
        Overlay::None => {}
        Overlay::FullResult { .. } => render_text_overlay(frame, app, chunks[1], true),
        Overlay::DiffStat { .. } => render_text_overlay(frame, app, chunks[1], false),
        Overlay::WorkCard { .. } => render_work_card_overlay(frame, app, chunks[1]),
        Overlay::ConfirmAccountSwitch {
            id,
            email,
            current_active,
            is_already_active,
            running_count,
        } => render_account_switch_modal(
            frame,
            area,
            id,
            email,
            current_active.as_deref(),
            *is_already_active,
            *running_count,
        ),
        Overlay::CommandModal {
            action_label,
            command,
            note,
        } => render_command_modal(frame, area, action_label, command, note),
        Overlay::Help => render_help_modal(frame, area),
    }
}

fn muted() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn primary() -> Style {
    Style::default().fg(Color::Reset)
}

fn bold_primary() -> Style {
    Style::default()
        .fg(Color::Reset)
        .add_modifier(Modifier::BOLD)
}

fn rounded_block<'a>(title: Line<'a>, focused: bool) -> Block<'a> {
    let border_style = if focused { primary() } else { muted() };
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
}

fn render_top_bar(frame: &mut Frame, app: &App, area: Rect) {
    if app.tab == ViewTab::Work {
        render_work_top_bar(frame, app, area);
        return;
    }

    let pending = app.snapshot.total_count(GroupKind::Pending);
    let running = app.snapshot.total_count(GroupKind::Running);
    let queued = app.snapshot.total_count(GroupKind::Queued);
    let merged = app.snapshot.total_count(GroupKind::MergedUncleaned);
    let stopped = app.snapshot.total_count(GroupKind::Stopped);
    let dag_pending = app
        .snapshot
        .dag_queue
        .iter()
        .filter(|q| matches!(q.status.as_str(), "pending" | "paused"))
        .count();
    let wc = app.snapshot.work_counts();
    let work_open = wc.owner + wc.needs_action + wc.ready + wc.in_progress + wc.waiting + wc.deferred;

    let tab_runs_style = if app.tab == ViewTab::Runs {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        muted()
    };
    let tab_events_style = if app.tab == ViewTab::Events {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        muted()
    };
    let tab_merges_style = if app.tab == ViewTab::Merges {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        muted()
    };
    let tab_candidates_style = if app.tab == ViewTab::Candidates {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        muted()
    };
    let tab_work_style = if wc.owner > 0 {
        Style::default()
            .fg(STATUS_PENDING)
            .add_modifier(Modifier::BOLD)
    } else {
        muted()
    };
    let tab_accounts_style = if app.tab == ViewTab::Accounts {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else if app.snapshot.agy_accounts.live_drifted {
        Style::default()
            .fg(STATUS_PENDING)
            .add_modifier(Modifier::BOLD)
    } else {
        muted()
    };
    let accounts_count = app.snapshot.agy_accounts.accounts.len();

    let mut spans = vec![
        Span::styled(
            " delegate-tui ",
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(" 1任务 ", tab_runs_style),
        Span::raw(" "),
        Span::styled(
            format!(" 2事件({}) ", app.snapshot.events.len()),
            tab_events_style,
        ),
        Span::raw(" "),
        Span::styled(
            format!(" 3合入({}) ", app.snapshot.merges.len()),
            tab_merges_style,
        ),
        Span::raw(" "),
        Span::styled(
            format!(" 4候选({}) ", app.snapshot.candidates.len()),
            tab_candidates_style,
        ),
        Span::raw(" "),
        Span::styled(
            if work_open > 0 {
                format!(" 5工作({work_open}) ")
            } else {
                " 5工作 ".to_string()
            },
            tab_work_style,
        ),
        Span::raw(" "),
        Span::styled(
            if accounts_count > 0 {
                format!(" 6账号({accounts_count}) ")
            } else {
                " 6账号 ".to_string()
            },
            tab_accounts_style,
        ),
        Span::raw("  "),
        Span::styled(
            format!("待处理 {pending}"),
            if pending > 0 {
                Style::default()
                    .fg(STATUS_PENDING)
                    .add_modifier(Modifier::BOLD)
            } else {
                muted()
            },
        ),
        Span::styled(" · ", muted()),
        Span::styled(
            format!("运行 {running}"),
            if running > 0 {
                Style::default()
                    .fg(STATUS_RUNNING)
                    .add_modifier(Modifier::BOLD)
            } else {
                muted()
            },
        ),
        Span::styled(" · ", muted()),
        Span::styled(
            format!("排队 {queued}"),
            if queued > 0 {
                Style::default()
                    .fg(STATUS_QUEUED)
                    .add_modifier(Modifier::BOLD)
            } else {
                muted()
            },
        ),
        Span::styled(" · ", muted()),
        Span::styled(format!("已合 {merged}"), muted()),
    ];
    if dag_pending > 0 {
        spans.push(Span::styled(" · ", muted()));
        spans.push(Span::styled(
            format!("续派 {dag_pending}"),
            Style::default()
                .fg(STATUS_QUEUED)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if stopped > 0 && area.width >= 100 {
        spans.push(Span::styled(" · ", muted()));
        spans.push(Span::styled(format!("已停 {stopped}"), muted()));
    }
    if !app.filter.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!(" 滤:{} ", truncate(&app.filter, 16)),
            Style::default()
                .fg(STATUS_PENDING)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[derive(Clone, Copy)]
struct ColWidths {
    show_state: bool,
    name_w: usize,
    model_w: usize,
    time_w: usize,
    files_w: usize,
    bar_w: usize,
    protect_w: usize,
    accept_w: usize,
}

impl ColWidths {
    fn compute(width: usize) -> Self {
        let show_state = width >= 62;
        let model_w = if width >= 66 { 6 } else { 5 };
        let time_w = if width >= 68 { 9 } else { 6 };
        let files_w = 4;
        let bar_w = if width >= 54 { 5 } else { 0 };
        let protect_w = 3;
        let accept_w = 4;

        let left = 2 + 2 + if show_state { 5 } else { 0 };
        let bar_total = if bar_w > 0 { 1 + bar_w } else { 0 };
        let right = 1 + model_w + 1 + time_w + 1 + files_w + bar_total + 1 + protect_w + 1 + accept_w;
        let name_w = width.saturating_sub(left + right).max(6);
        Self {
            show_state,
            name_w,
            model_w,
            time_w,
            files_w,
            bar_w,
            protect_w,
            accept_w,
        }
    }
}

fn render_list_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::List && matches!(app.overlay, Overlay::None);
    let run_count: usize = if app.filter.is_empty() {
        app.snapshot.repos.iter().map(|r| r.runs.len()).sum()
    } else {
        app.rows
            .iter()
            .filter(|r| matches!(r, ListRow::Run { .. }))
            .count()
    };
    let title_text = if app.filter.is_empty() {
        format!(" 任务 ({run_count}) ")
    } else {
        format!(" 任务 ({run_count}) · 过滤 ")
    };
    let title = Line::from(Span::styled(
        title_text,
        if focused { bold_primary() } else { muted() },
    ));
    let block = rounded_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if app.rows.is_empty() {
        let msg = if app.loading {
            "加载中…".to_string()
        } else if !app.filter.is_empty() {
            format!("无匹配“{}”的任务", app.filter)
        } else if !app.snapshot.warnings.is_empty() {
            truncate(&app.snapshot.warnings[0], inner.width as usize)
        } else {
            "暂无委派任务".to_string()
        };
        let line = Line::from(Span::styled(
            format!(" {}", truncate(&msg, inner.width.saturating_sub(1) as usize)),
            muted(),
        ));
        frame.render_widget(Paragraph::new(line), inner);
        return;
    }

    let inner_w = inner.width as usize;
    let cols = ColWidths::compute(inner_w);
    let show_col_header = inner.height >= 4;
    let header_h: usize = if show_col_header { 1 } else { 0 };
    let view_h = (inner.height as usize).saturating_sub(header_h);

    if let Some(sel) = app.selected_row {
        if sel < app.list_scroll {
            app.list_scroll = sel.saturating_sub(1);
        } else if view_h > 0 && sel >= app.list_scroll + view_h {
            app.list_scroll = sel + 1 - view_h;
        }
    }
    let max_scroll = app.rows.len().saturating_sub(view_h);
    if app.list_scroll > max_scroll {
        app.list_scroll = max_scroll;
    }

    let mut lines: Vec<Line> = Vec::with_capacity(inner.height as usize);
    if show_col_header {
        lines.push(render_col_header(cols, inner_w));
    }
    for (row_idx, row) in app
        .rows
        .iter()
        .enumerate()
        .skip(app.list_scroll)
        .take(view_h)
    {
        let selected = app.selected_row == Some(row_idx);
        let line = match *row {
            ListRow::RepoHeader { repo_idx } => {
                render_repo_header(&app.snapshot.repos[repo_idx], inner_w)
            }
            ListRow::GroupHeader { group, count, .. } => render_group_header(group, count, inner_w),
            ListRow::ToggleFold {
                kind,
                count,
                newest_ago_seconds,
                expanded,
                standalone,
                ..
            } => render_toggle_fold_row(
                kind,
                count,
                newest_ago_seconds,
                expanded,
                standalone,
                selected,
                inner_w,
            ),
            ListRow::Run { repo_idx, run_idx } => {
                render_run_row(&app.snapshot.repos[repo_idx].runs[run_idx], selected, cols, inner_w)
            }
            ListRow::DagQueueHeader { count } => {
                let text = format!("▌ 确定性续派队列 ({count})");
                Line::from(Span::styled(
                    truncate(&text, inner_w),
                    Style::default()
                        .fg(STATUS_QUEUED)
                        .add_modifier(Modifier::BOLD),
                ))
            }
            ListRow::DagQueueEntry { idx } => {
                render_dag_queue_row(&app.snapshot.dag_queue[idx], selected, inner_w)
            }
        };
        lines.push(line);
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_col_header(cols: ColWidths, width: usize) -> Line<'static> {
    let mut s = String::new();
    s.push_str("    ");
    if cols.show_state {
        s.push_str(&pad_right("状态", 4));
        s.push(' ');
    }
    s.push_str(&pad_right("任务", cols.name_w));
    s.push(' ');
    s.push_str(&pad_right("模型", cols.model_w));
    s.push(' ');
    s.push_str(&pad_left("时间", cols.time_w));
    s.push(' ');
    let diff_w = cols.files_w + if cols.bar_w > 0 { 1 + cols.bar_w } else { 0 };
    s.push_str(&pad_left("改动", diff_w));
    s.push(' ');
    s.push_str(&pad_right("护", cols.protect_w));
    s.push(' ');
    s.push_str(&pad_right("验收", cols.accept_w));
    Line::from(Span::styled(truncate(&s, width), muted()))
}

fn render_repo_header(repo: &RepoGroup, width: usize) -> Line<'static> {
    let counts = format!(
        "待{} 运{} 排{} 合{}",
        repo.count_group(GroupKind::Pending),
        repo.count_group(GroupKind::Running),
        repo.count_group(GroupKind::Queued),
        repo.count_group(GroupKind::MergedUncleaned)
    );
    let counts_w = display_width(&counts);
    let prefix = "▌ ";
    let avail_label = width.saturating_sub(display_width(prefix) + counts_w + 2);
    let label = truncate(&repo.label, avail_label);
    let gap = width.saturating_sub(display_width(prefix) + display_width(&label) + counts_w);
    Line::from(vec![
        Span::styled(prefix, muted()),
        Span::styled(label, bold_primary()),
        Span::raw(" ".repeat(gap)),
        Span::styled(counts, muted()),
    ])
}

fn render_group_header(group: GroupKind, count: usize, width: usize) -> Line<'static> {
    let style = match group_status_color(group) {
        Some(c) => Style::default().fg(c).add_modifier(Modifier::BOLD),
        None => muted().add_modifier(Modifier::BOLD),
    };
    let text = format!(" ▾ {} ({count})", group.label());
    Line::from(Span::styled(truncate(&text, width), style))
}

fn render_toggle_fold_row(
    kind: CollapseKind,
    count: usize,
    newest_ago_seconds: Option<u64>,
    expanded: bool,
    standalone: bool,
    selected: bool,
    width: usize,
) -> Line<'static> {
    let arrow = if expanded { "▾" } else { "▸" };
    let ago = newest_ago_seconds
        .map(|s| format!("（{}）", format_relative_time(Some(s))))
        .unwrap_or_default();
    let label = match kind {
        CollapseKind::StalePending => {
            if standalone {
                format!("{arrow} 待处理 · 更早 {count} 条{ago}")
            } else {
                format!("  {arrow} 更早 {count} 条{ago}")
            }
        }
        CollapseKind::MergedUncleaned => format!("{arrow} 已合未清 {count} 条{ago}"),
        CollapseKind::Stopped => format!("{arrow} 已停 {count} 条{ago}"),
    };

    if selected {
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        let hint = if expanded { "Enter 折叠 " } else { "Enter 展开 " };
        let hint_w = display_width(hint);
        let accent_style = match kind {
            CollapseKind::StalePending => Style::default()
                .fg(STATUS_PENDING)
                .add_modifier(Modifier::BOLD),
            CollapseKind::MergedUncleaned | CollapseKind::Stopped => bold_primary(),
        };
        let avail = width.saturating_sub(2 + hint_w + 1);
        let clipped = truncate(&label, avail);
        let gap = width.saturating_sub(2 + display_width(&clipped) + hint_w);
        return Line::from(vec![
            Span::styled("▎", accent_style),
            Span::styled(" ", rev),
            Span::styled(clipped, rev.add_modifier(Modifier::BOLD)),
            Span::styled(" ".repeat(gap), rev),
            Span::styled(hint, rev),
        ]);
    }

    let style = match kind {
        CollapseKind::StalePending if standalone => Style::default().fg(STATUS_PENDING),
        _ => muted(),
    };
    let prefix = if standalone { " " } else { "  " };
    Line::from(Span::styled(
        truncate(&format!("{prefix}{label}"), width),
        style,
    ))
}

fn format_row_time(run: &RunItem, width: usize) -> String {
    match run.group {
        GroupKind::Running => {
            let s = format_duration(run.elapsed_seconds.or(run.age_seconds));
            if width >= 8 {
                format!("跑 {s}")
            } else {
                s
            }
        }
        GroupKind::Queued => {
            if let Some(ahead) = run.queue_ahead.or(run.measure_waiting_ahead) {
                format!("前 {ahead} 位")
            } else {
                format_relative_time(run.age_seconds)
            }
        }
        _ => {
            let rel = format_relative_time(run.finished_ago_seconds.or(run.age_seconds));
            if width < 8 {
                rel.replace(" 分钟前", "分前").replace(" 小时前", "时前").replace(" 天前", "天前")
            } else {
                rel
            }
        }
    }
}

fn render_run_row(run: &RunItem, selected: bool, cols: ColWidths, width: usize) -> Line<'static> {
    let (glyph, status_color) = run_glyph_color(run);
    let (files, added, deleted) = run.display_changes();
    let files_text = if files == 0 {
        "-".to_string()
    } else {
        format!("{files}f")
    };
    let time_text = format_row_time(run, cols.time_w);

    let protect_info = if run.source_write_violation {
        Some(("源!", STATUS_FAILED, true))
    } else if !run.protect_violation.is_empty() {
        Some(("保!", STATUS_FAILED, true))
    } else if !run.prompt_protected_paths.is_empty() {
        Some(("保?", STATUS_PENDING, false))
    } else {
        None
    };

    let (accept_badge, accept_color) = match run.accept {
        AcceptStatus::Passed { .. } => ("验✓", Some(STATUS_RUNNING)),
        AcceptStatus::Failed { .. } => ("验✗", Some(STATUS_FAILED)),
        AcceptStatus::NotExecuted { .. } => ("验待", Some(STATUS_PENDING)),
        AcceptStatus::None => (" -", None),
    };

    let review_snippet = run
        .review
        .as_ref()
        .map(ReviewInfo::row_snippet)
        .filter(|s| !s.is_empty())
        .map(|s| format!(" [{s}]"))
        .unwrap_or_default();
    let premerge_snippet = if !run.applied {
        run.premerge
            .as_ref()
            .map(PremergeInfo::row_snippet)
            .filter(|s| !s.is_empty())
            .map(|s| format!(" [{s}]"))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let row_tag_snippet = format!("{review_snippet}{premerge_snippet}");

    let full_name = if let (GroupKind::Running, Some(act)) = (run.group, run.activity.as_ref()) {
        let snippet = if !review_snippet.is_empty() {
            review_snippet
        } else {
            match act.kind.as_str() {
                "running_command" => {
                    if let Some(cmd) = &act.cmd {
                        format!(" [{}]", cmd)
                    } else {
                        String::new()
                    }
                }
                "quota" | "error" => format!(" [{}]", act.label),
                _ if run.stall_warning.is_some() => " [卡住提醒]".to_string(),
                _ => String::new(),
            }
        };
        if !snippet.is_empty() && cols.name_w > 10 {
            let base = if run.chain_round > 1 {
                format!("{} [{}轮]", run.display_name, run.chain_round)
            } else {
                run.display_name.clone()
            };
            truncate(&format!("{base}{snippet}"), cols.name_w)
        } else if run.chain_round > 1 {
            let suffix = format!(" [{}轮]", run.chain_round);
            let sw = display_width(&suffix);
            if cols.name_w > sw + 2 {
                format!("{}{suffix}", truncate(&run.display_name, cols.name_w - sw))
            } else {
                truncate(&run.display_name, cols.name_w)
            }
        } else {
            truncate(&run.display_name, cols.name_w)
        }
    } else if !row_tag_snippet.is_empty() && cols.name_w > 10 {
        let base = if run.chain_round > 1 {
            format!("{} [{}轮]", run.display_name, run.chain_round)
        } else {
            run.display_name.clone()
        };
        let tag_w = display_width(&row_tag_snippet);
        if cols.name_w >= tag_w + 8 {
            let name_avail = cols.name_w - tag_w;
            format!("{}{row_tag_snippet}", truncate(&base, name_avail))
        } else {
            truncate(&format!("{base}{row_tag_snippet}"), cols.name_w)
        }
    } else if run.chain_round > 1 {
        let suffix = format!(" [{}轮]", run.chain_round);
        let sw = display_width(&suffix);
        if cols.name_w > sw + 2 {
            format!("{}{suffix}", truncate(&run.display_name, cols.name_w - sw))
        } else {
            truncate(&run.display_name, cols.name_w)
        }
    } else {
        truncate(&run.display_name, cols.name_w)
    };
    let padded_name = pad_right(&full_name, cols.name_w);
    let (add_n, del_n, neu_n) = diff_bar_segments(added, deleted, files, cols.bar_w);
    let empty_n = cols.bar_w.saturating_sub(add_n + del_n + neu_n);

    let mut spans = Vec::with_capacity(18);

    if selected {
        // Left status accent stripe + uniform reversed bar: high contrast on both dark & light terminals.
        let accent_style = match status_color {
            Some(c) => Style::default().fg(c).add_modifier(Modifier::BOLD),
            None => bold_primary(),
        };
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        let rev_bold = rev.add_modifier(Modifier::BOLD);

        spans.push(Span::styled("▎", accent_style));
        spans.push(Span::styled(" ", rev));
        spans.push(Span::styled(format!("{glyph} "), rev_bold));
        if cols.show_state {
            spans.push(Span::styled(format!("{} ", pad_right(run.state_badge(), 4)), rev));
        }
        spans.push(Span::styled(padded_name, rev_bold));
        spans.push(Span::styled(" ", rev));
        spans.push(Span::styled(pad_right(&run.model, cols.model_w), rev));
        spans.push(Span::styled(" ", rev));
        spans.push(Span::styled(pad_left(&time_text, cols.time_w), rev));
        spans.push(Span::styled(" ", rev));
        spans.push(Span::styled(pad_left(&files_text, cols.files_w), rev));
        if cols.bar_w > 0 {
            spans.push(Span::styled(" ", rev));
            let filled = add_n + del_n + neu_n;
            let bar = format!("{}{}", "▪".repeat(filled), "·".repeat(empty_n));
            spans.push(Span::styled(bar, rev));
        }
        spans.push(Span::styled(" ", rev));
        if let Some((p_text, _, _)) = protect_info {
            spans.push(Span::styled(pad_right(p_text, cols.protect_w), rev_bold));
        } else {
            spans.push(Span::styled(" ".repeat(cols.protect_w), rev));
        }
        spans.push(Span::styled(" ", rev));
        spans.push(Span::styled(pad_right(accept_badge, cols.accept_w), rev));

        let used: usize = spans.iter().map(|s| display_width(&s.content)).sum();
        if used < width {
            spans.push(Span::styled(" ".repeat(width - used), rev));
        }
        return Line::from(spans);
    }

    let state_style = match status_color {
        Some(c) => Style::default().fg(c),
        None => muted(),
    };
    let name_style = if matches!(run.group, GroupKind::MergedUncleaned | GroupKind::Stopped) {
        muted()
    } else {
        primary()
    };

    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        format!("{glyph} "),
        state_style.add_modifier(Modifier::BOLD),
    ));
    if cols.show_state {
        spans.push(Span::styled(
            format!("{} ", pad_right(run.state_badge(), 4)),
            state_style,
        ));
    }
    spans.push(Span::styled(padded_name, name_style));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(pad_right(&run.model, cols.model_w), muted()));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(pad_left(&time_text, cols.time_w), muted()));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        pad_left(&files_text, cols.files_w),
        if files > 0 { primary() } else { muted() },
    ));
    if cols.bar_w > 0 {
        spans.push(Span::raw(" "));
        if add_n > 0 {
            spans.push(Span::styled("▪".repeat(add_n), Style::default().fg(STATUS_RUNNING)));
        }
        if del_n > 0 {
            spans.push(Span::styled("▪".repeat(del_n), Style::default().fg(STATUS_FAILED)));
        }
        if neu_n > 0 {
            spans.push(Span::styled("▪".repeat(neu_n), muted()));
        }
        if empty_n > 0 {
            spans.push(Span::styled("·".repeat(empty_n), muted()));
        }
    }
    spans.push(Span::raw(" "));
    if let Some((p_text, p_color, bold)) = protect_info {
        spans.push(Span::styled(
            pad_right(p_text, cols.protect_w),
            Style::default()
                .fg(p_color)
                .add_modifier(if bold { Modifier::BOLD } else { Modifier::empty() }),
        ));
    } else {
        spans.push(Span::raw(" ".repeat(cols.protect_w)));
    }
    spans.push(Span::raw(" "));
    let acc_style = match accept_color {
        Some(c) => Style::default().fg(c),
        None => muted(),
    };
    spans.push(Span::styled(pad_right(accept_badge, cols.accept_w), acc_style));

    Line::from(spans)
}

fn render_detail_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::Detail && matches!(app.overlay, Overlay::None);
    if let Some(q) = app.selected_dag_queue().cloned() {
        render_dag_queue_detail_pane(frame, app, &q, area, focused);
        return;
    }
    let Some(run) = app.selected_run().cloned() else {
        let block = rounded_block(Line::from(Span::styled(" 详情 ", muted())), focused);
        frame.render_widget(block, area);
        return;
    };

    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let lines = build_detail_lines(app, &run, inner_w);
    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_h);
    if app.detail_scroll > max_scroll {
        app.detail_scroll = max_scroll;
    }

    let scroll_hint = if total > inner_h {
        format!(" {}/{} ", app.detail_scroll + 1, total)
    } else {
        String::new()
    };
    let round_tag = if run.chain_round > 1 {
        format!(" · 第{}轮", run.chain_round)
    } else {
        String::new()
    };
    let max_title_w = inner_w.saturating_sub(display_width(&scroll_hint) + 8);
    let title_core = truncate(&format!("{}{round_tag}", run.display_name), max_title_w);

    let title = Line::from(Span::styled(
        format!(" {title_core} "),
        if focused { bold_primary() } else { muted() },
    ));
    let mut block = rounded_block(title, focused);
    if !scroll_hint.is_empty() {
        block = block.title_bottom(Line::from(Span::styled(scroll_hint, muted())));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible: Vec<Line> = lines
        .into_iter()
        .skip(app.detail_scroll)
        .take(inner_h)
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn build_detail_lines(app: &App, run: &RunItem, width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let (_, status_color) = run_glyph_color(run);

    // 1. Metadata header
    let badge_style = match status_color {
        Some(c) => Style::default()
            .fg(c)
            .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        None => Style::default().add_modifier(Modifier::REVERSED),
    };
    let mut head_spans = vec![
        Span::styled(format!(" {} ", run.state_badge()), badge_style),
        Span::raw(" "),
        Span::styled(truncate(&run.run, width.saturating_sub(10)), bold_primary()),
    ];
    if run.applied {
        head_spans.push(Span::styled(
            format!(" ({})", run.applied_by.as_deref().unwrap_or("已合入")),
            muted(),
        ));
    }
    let linked_wid = app.snapshot.work_id_for_run(run);
    if let Some(ref wid) = linked_wid {
        head_spans.push(Span::raw(" "));
        head_spans.push(Span::styled(
            format!("[{wid}]"),
            Style::default()
                .fg(STATUS_QUEUED)
                .add_modifier(Modifier::BOLD),
        ));
    }
    out.push(Line::from(head_spans));
    if let Some(ref wid) = linked_wid {
        let w_title = app
            .snapshot
            .work_item_by_id(wid)
            .map(|w| format!(" · {}", w.display_title()))
            .unwrap_or_default();
        push_wrapped(
            &mut out,
            &format!("工作项: {wid}{w_title}（按 w 看工作表）"),
            width,
            Style::default().fg(STATUS_QUEUED),
        );
    }

    let tier_part = run
        .tier
        .as_deref()
        .map(|t| format!(" · {t}"))
        .unwrap_or_default();
    let mode_part = if run.mode.is_empty() {
        String::new()
    } else {
        format!(" · {}", run.mode)
    };
    push_wrapped(
        &mut out,
        &format!("{}  ·  {}{tier_part}{mode_part}", run.repo_label, run.model),
        width,
        muted(),
    );

    let mut timing_parts = Vec::new();
    if let Some(fin_ago) = run.finished_ago_seconds {
        timing_parts.push(format!("{}完成", format_relative_time(Some(fin_ago))));
    } else if let Some(age) = run.age_seconds {
        timing_parts.push(format!("{}发起", format_relative_time(Some(age))));
    }
    if run.elapsed_seconds.is_some() {
        timing_parts.push(format!("用时 {}", format_duration_zh(run.elapsed_seconds)));
    }
    if let Some(idle) = run.idle_seconds {
        timing_parts.push(format!("静默 {}", format_duration_zh(Some(idle))));
    }
    if let Some(q) = run.queued_seconds.filter(|&s| s > 0) {
        timing_parts.push(format!("排队 {}", format_duration_zh(Some(q))));
    }
    if let Some(turns) = run.turns {
        timing_parts.push(format!("{turns} 轮"));
    }
    if run.chain_round > 1 {
        timing_parts.push(format!("续做第 {} 轮", run.chain_round));
    }
    if run.pending_reply {
        timing_parts.push("有待续追问".to_string());
    }
    if !timing_parts.is_empty() {
        push_wrapped(&mut out, &timing_parts.join(" · "), width, muted());
    }

    // Liveness activity ("正在做什么") & stall warning
    if let Some(act) = &run.activity {
        if !act.label.is_empty() {
            let act_color = match act.kind.as_str() {
                "running_command" | "thinking" => STATUS_RUNNING,
                "waiting" => STATUS_QUEUED,
                "quota" => STATUS_PENDING,
                "error" => STATUS_FAILED,
                _ => STATUS_RUNNING,
            };
            push_wrapped(
                &mut out,
                &format!("正在做什么: {}", act.label),
                width,
                Style::default().fg(act_color).add_modifier(Modifier::BOLD),
            );
        }
    }
    if let Some(sw) = &run.stall_warning {
        let det = sw.detail.as_deref().unwrap_or(&sw.kind);
        push_wrapped(
            &mut out,
            &format!("⚠ 卡住提醒 ({}): {det}", sw.kind),
            width,
            Style::default()
                .fg(STATUS_PENDING)
                .add_modifier(Modifier::BOLD),
        );
        if let Some(sc) = &sw.self_check {
            push_wrapped(&mut out, &format!("  自检: {sc}"), width, muted());
        }
    }

    // Queue info
    if run.queue_reason.is_some()
        || run.queue_ahead.is_some()
        || run.measure_waiting_ahead.is_some()
    {
        let mut q_parts = Vec::new();
        if let Some(reason) = &run.queue_reason {
            q_parts.push(reason.clone());
        }
        if let Some(ahead) = run.queue_ahead.or(run.measure_waiting_ahead) {
            q_parts.push(format!("前方 {ahead} 个"));
        }
        push_wrapped(
            &mut out,
            &format!("排队: {}", q_parts.join(" · ")),
            width,
            Style::default().fg(STATUS_QUEUED),
        );
    }

    // Acceptance (only shown when configured)
    match &run.accept {
        AcceptStatus::Passed { command } => {
            let text = if command.is_empty() {
                "验收 ✓ 通过".to_string()
            } else {
                format!("验收 ✓ {command}")
            };
            push_wrapped(&mut out, &text, width, Style::default().fg(STATUS_RUNNING));
        }
        AcceptStatus::Failed { command, exit_code } => {
            let code_s = exit_code.map(|c| format!(" exit={c}")).unwrap_or_default();
            push_wrapped(
                &mut out,
                &format!("验收 ✗ 未通过{code_s} ({command})"),
                width,
                Style::default()
                    .fg(STATUS_FAILED)
                    .add_modifier(Modifier::BOLD),
            );
        }
        AcceptStatus::NotExecuted { command, infra } => {
            let reason = if *infra { "基础设施/排队" } else { "未执行" };
            push_wrapped(
                &mut out,
                &format!("验收 ! {reason} ({command})"),
                width,
                Style::default().fg(STATUS_PENDING),
            );
        }
        AcceptStatus::None => {}
    }

    // Review status (6.29)
    if let Some(rev) = &run.review {
        let label = rev.detail_label();
        if !label.is_empty() {
            let (rev_color, bold) = if rev.status == "reviewing" {
                (STATUS_QUEUED, true)
            } else if rev.status == "reworking" || rev.verdict.as_deref() == Some("REWORK") {
                (STATUS_PENDING, true)
            } else if rev.verdict.as_deref() == Some("PASS") {
                (STATUS_RUNNING, false)
            } else if rev.verdict.as_deref() == Some("NEEDS_CONTROLLER") {
                (STATUS_FAILED, true)
            } else {
                (STATUS_PENDING, false)
            };
            let mut style = Style::default().fg(rev_color);
            if bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            let attn = if rev.needs_attention && rev.verdict.as_deref() != Some("PASS") {
                " · ⚠ 需主控关注"
            } else {
                ""
            };
            push_wrapped(&mut out, &format!("审查: {label}{attn}"), width, style);
            if let Some(up) = &rev.upgraded_by {
                push_wrapped(&mut out, &format!("  升级原因: {up}"), width, muted());
            }
            if !rev.controller_paths.is_empty() {
                push_wrapped(
                    &mut out,
                    &format!("  controllerReviewPaths: {}", rev.controller_paths.join("、")),
                    width,
                    Style::default().fg(STATUS_PENDING),
                );
            }
            if let Some(hash) = &rev.sign_hash {
                let sign_note = if rev.sign_valid {
                    "已签字有效"
                } else if rev.sign_invalidated {
                    "签字已失效"
                } else {
                    "待签字"
                };
                push_wrapped(
                    &mut out,
                    &format!("  签字哈希 ({sign_note}): {hash}"),
                    width,
                    muted(),
                );
            }
        }
    }

    // Premerge / fast-forward status (6.35)
    if !run.applied {
        if let Some(pm) = &run.premerge {
            let lbl = pm.full_label();
            if !lbl.is_empty() {
                let (pm_color, bold) = match pm.status.as_str() {
                    "ready" => (STATUS_RUNNING, false),
                    "running" | "pending" => (STATUS_QUEUED, true),
                    "stale" => (STATUS_PENDING, true),
                    "conflict" | "failed" => (STATUS_FAILED, true),
                    _ => (STATUS_PENDING, false),
                };
                let mut style = Style::default().fg(pm_color);
                if bold {
                    style = style.add_modifier(Modifier::BOLD);
                }
                push_wrapped(&mut out, &format!("预合并: {lbl}"), width, style);
                if matches!(pm.status.as_str(), "conflict" | "failed") {
                    if let Some(log) = &pm.log {
                        push_wrapped(&mut out, &format!("  日志: {log}"), width, muted());
                    }
                }
            }
        }
    }

    // Alerts (only when present)
    if !run.protect_violation.is_empty() {
        push_wrapped(
            &mut out,
            &format!("! 受保护路径改动: {}", run.protect_violation.join(", ")),
            width,
            Style::default()
                .fg(STATUS_FAILED)
                .add_modifier(Modifier::BOLD),
        );
    }
    if run.source_write_violation {
        push_wrapped(
            &mut out,
            "! 源仓库写入违规",
            width,
            Style::default()
                .fg(STATUS_FAILED)
                .add_modifier(Modifier::BOLD),
        );
    }
    if !run.prompt_protected_paths.is_empty() {
        push_wrapped(
            &mut out,
            &format!(
                "受保护路径提示: {}",
                run.prompt_protected_paths.join(", ")
            ),
            width,
            Style::default().fg(STATUS_PENDING),
        );
    }
    if !run.conflict_markers.is_empty() {
        push_wrapped(
            &mut out,
            &format!("! 冲突标记未解: {}", run.conflict_markers.join(", ")),
            width,
            Style::default()
                .fg(STATUS_FAILED)
                .add_modifier(Modifier::BOLD),
        );
    }
    if let Some(err) = &run.error {
        push_wrapped(
            &mut out,
            &format!("错误: {err}"),
            width,
            Style::default().fg(STATUS_FAILED),
        );
    } else if let Some(terr) = &run.transient_error {
        push_wrapped(
            &mut out,
            &format!("上游中断 (续做 {} 次): {terr}", run.resumes),
            width,
            Style::default().fg(STATUS_PENDING),
        );
    }

    // 2. 结论摘要 (Markdown rendered cleanly)
    if let Some((excerpt, truncated, total_chars)) = app.selected_result_excerpt() {
        if !excerpt.trim().is_empty() {
            out.push(Line::raw(""));
            out.push(section_header("结论摘要"));
            out.extend(render_markdown(&excerpt, "  ", width));
            if truncated {
                out.push(Line::from(Span::styled(
                    format!("  … 共 {total_chars} 字 · Enter 看全文"),
                    muted(),
                )));
            }
        }
    } else if let Some(err) = app.selected_result_error() {
        out.push(Line::raw(""));
        out.push(section_header("结论摘要"));
        push_indented_wrapped(&mut out, "  ", err, width, muted());
    } else if run.has_result {
        out.push(Line::raw(""));
        out.push(section_header("结论摘要"));
        out.push(Line::from(Span::styled("  加载中…", muted())));
    }

    // 3. 最近命令 (only when present)
    let accept_cmd = match &run.accept {
        AcceptStatus::Passed { command }
        | AcceptStatus::Failed { command, .. }
        | AcceptStatus::NotExecuted { command, .. }
            if !command.is_empty() =>
        {
            Some(command.as_str())
        }
        _ => None,
    };
    if run.last_command.is_some() || accept_cmd.is_some() {
        out.push(Line::raw(""));
        out.push(section_header("最近命令"));
        if let Some(last) = &run.last_command {
            let idle_suffix = run
                .idle_seconds
                .map(|s| format!("  [静 {}]", format_duration(Some(s))))
                .unwrap_or_default();
            push_indented_wrapped(
                &mut out,
                "  ",
                &format!("{last}{idle_suffix}"),
                width,
                primary(),
            );
        }
        if let Some(cmd) = accept_cmd {
            push_indented_wrapped(&mut out, "  ", &format!("$ {cmd}"), width, muted());
        }
    }

    // 4. 改动文件（按目录汇总 + 小条形图，仅在有改动时展示）
    let (files, added, deleted) = run.display_changes();
    if files > 0 || !run.dir_changes.is_empty() {
        out.push(Line::raw(""));
        let summary = if added > 0 || deleted > 0 {
            format!("改动分布 ({files} 文件 +{added}/-{deleted})")
        } else {
            format!("改动分布 ({files} 文件)")
        };
        out.push(section_header(&summary));
        let max_dir_lines = run
            .dir_changes
            .iter()
            .map(|d| d.added.saturating_add(d.deleted))
            .max()
            .unwrap_or(0);
        let bar_slots: usize = if width >= 48 { 8 } else { 5 };
        let stats_w: usize = 20 + bar_slots;
        let dir_w = width.saturating_sub(stats_w + 3).max(8);

        for d in &run.dir_changes {
            let dir_col = pad_right(&d.path, dir_w);
            let files_part = if d.files > 0 {
                format!("{:>2}f ", d.files)
            } else {
                "    ".to_string()
            };
            let delta_part = if d.added > 0 || d.deleted > 0 {
                pad_left(&format!("+{}/-{}", d.added, d.deleted), 12)
            } else {
                " ".repeat(12)
            };

            let mut row_spans = vec![
                Span::raw("  "),
                Span::styled(dir_col, primary()),
                Span::raw(" "),
                Span::styled(files_part, muted()),
                Span::styled(delta_part, muted()),
                Span::raw(" "),
            ];

            let (a_c, d_c, n_c) = if max_dir_lines > 0 {
                let tot = d.added.saturating_add(d.deleted);
                let filled = (((tot as f64 / max_dir_lines as f64) * bar_slots as f64).ceil()
                    as usize)
                    .clamp(usize::from(tot > 0), bar_slots);
                if d.deleted == 0 {
                    (filled, 0, 0)
                } else if d.added == 0 {
                    (0, filled, 0)
                } else if filled == 1 {
                    if d.added >= d.deleted {
                        (1, 0, 0)
                    } else {
                        (0, 1, 0)
                    }
                } else {
                    let mut dc = ((d.deleted as f64 / tot as f64) * filled as f64).round() as usize;
                    dc = dc.clamp(1, filled - 1);
                    (filled - dc, dc, 0)
                }
            } else {
                diff_bar_segments(0, 0, d.files as u64, bar_slots)
            };
            let empty_c = bar_slots.saturating_sub(a_c + d_c + n_c);
            if a_c > 0 {
                row_spans.push(Span::styled(
                    "▪".repeat(a_c),
                    Style::default().fg(STATUS_RUNNING),
                ));
            }
            if d_c > 0 {
                row_spans.push(Span::styled(
                    "▪".repeat(d_c),
                    Style::default().fg(STATUS_FAILED),
                ));
            }
            if n_c > 0 {
                row_spans.push(Span::styled("▪".repeat(n_c), muted()));
            }
            if empty_c > 0 {
                row_spans.push(Span::styled("·".repeat(empty_c), muted()));
            }
            out.push(Line::from(row_spans));
        }
        if run.dirs_more > 0 {
            out.push(Line::from(Span::styled(
                format!("  … 另有 {} 个目录 · d 看 diff", run.dirs_more),
                muted(),
            )));
        }
    }

    // Pending notes (only when present)
    if !run.notes.is_empty() {
        out.push(Line::raw(""));
        out.push(section_header(&format!("待办 ({})", run.pending_notes)));
        for note in &run.notes {
            let mark = if note.done { "[✓]" } else { "[ ]" };
            let style = if note.done {
                muted()
            } else {
                Style::default().fg(STATUS_PENDING)
            };
            push_indented_wrapped(
                &mut out,
                "  ",
                &format!("{mark} #{}: {}", note.index, note.text),
                width,
                style,
            );
        }
    }

    // 5. 下一步 (only when present)
    if run.next_step.is_some() || run.recovery.is_some() {
        out.push(Line::raw(""));
        out.push(section_header("下一步"));
        if let Some(next) = &run.next_step {
            push_indented_wrapped(
                &mut out,
                "  ",
                next,
                width,
                Style::default().fg(STATUS_PENDING),
            );
        }
        if let Some(rec) = &run.recovery {
            push_indented_wrapped(
                &mut out,
                "  ",
                &format!("恢复: {rec}"),
                width,
                Style::default().fg(STATUS_FAILED),
            );
        }
    }

    out
}

fn section_header(title: &str) -> Line<'static> {
    Line::from(Span::styled(format!("▸ {title}"), bold_primary()))
}

fn push_wrapped(out: &mut Vec<Line<'static>>, text: &str, width: usize, style: Style) {
    for line in wrap_text(text, width.max(4)) {
        out.push(Line::from(Span::styled(line, style)));
    }
}

fn push_indented_wrapped(
    out: &mut Vec<Line<'static>>,
    indent: &str,
    text: &str,
    width: usize,
    style: Style,
) {
    let iw = display_width(indent);
    for line in wrap_text(text, width.saturating_sub(iw).max(4)) {
        out.push(Line::from(Span::styled(format!("{indent}{line}"), style)));
    }
}

fn render_bottom_bar(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    // Line 1: Lane mini meter + host load + refresh time
    let lane = &app.snapshot.lane;
    let l_run = lane.local_running();
    let l_q = lane.local_queued();
    let (l_filled, l_total) = lane_meter_cells(l_run, lane.max_heavy);
    let l_full = lane.max_heavy.is_some_and(|lim| lim > 0 && l_run >= lim);
    let meter_fill_color = if l_full { STATUS_PENDING } else { STATUS_RUNNING };

    let mut left_spans = vec![
        Span::styled(" Lane ", bold_primary()),
        Span::styled(
            "█".repeat(l_filled),
            Style::default().fg(meter_fill_color),
        ),
        Span::styled("░".repeat(l_total.saturating_sub(l_filled)), muted()),
        Span::raw(" "),
    ];
    let local_num = match lane.max_heavy {
        Some(lim) => format!("{l_run}/{lim}"),
        None => format!("运{l_run}"),
    };
    left_spans.push(Span::styled(local_num, primary()));
    if l_q > 0 {
        left_spans.push(Span::styled(
            format!(" +{l_q}排"),
            Style::default()
                .fg(STATUS_QUEUED)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let leases_strip = lane.leases_summary();
    let pool_text = if area.width >= 100 {
        if leases_strip.is_some() {
            lane.pool_summary(false)
                .or_else(|| lane.concurrency_summary.clone())
        } else {
            lane.concurrency_summary
                .clone()
                .or_else(|| lane.pool_summary(false))
        }
    } else {
        lane.compact_concurrency()
            .or_else(|| lane.concurrency_summary.clone())
    };
    if let Some(summary) = pool_text {
        left_spans.push(Span::styled(" · ", muted()));
        left_spans.push(Span::styled(summary, muted()));
    }
    if let Some(strip) = leases_strip {
        if area.width >= 90 {
            left_spans.push(Span::styled(" · ", muted()));
            left_spans.push(Span::styled(strip, primary()));
        }
    }

    if !lane.remote.is_empty() {
        let r_run = lane.remote_running();
        let r_q = lane.remote_queued();
        let (r_filled, r_total) = lane_meter_cells(r_run, lane.remote_max_heavy);
        left_spans.push(Span::styled(" · 远端 ", muted()));
        left_spans.push(Span::styled(
            "█".repeat(r_filled),
            Style::default().fg(STATUS_RUNNING),
        ));
        left_spans.push(Span::styled("░".repeat(r_total.saturating_sub(r_filled)), muted()));
        let r_num = match lane.remote_max_heavy {
            Some(lim) => format!(" {r_run}/{lim}"),
            None => format!(" 运{r_run}"),
        };
        left_spans.push(Span::styled(r_num, primary()));
        if r_q > 0 {
            left_spans.push(Span::styled(
                format!(" +{r_q}排"),
                Style::default().fg(STATUS_QUEUED),
            ));
        }
    }

    let load_part = app
        .snapshot
        .host_load
        .as_ref()
        .map(HostLoad::format_short)
        .unwrap_or_else(|| "负载 -".to_string());
    let right_text = format!("{load_part} · {} ", app.snapshot.refreshed_at);

    let w = area.width as usize;
    let lw: usize = left_spans.iter().map(|s| display_width(&s.content)).sum();
    let rw = display_width(&right_text);
    if lw + rw < w {
        left_spans.push(Span::raw(" ".repeat(w - lw - rw)));
        left_spans.push(Span::styled(right_text, muted()));
    } else if lw + 2 < w {
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            truncate(&right_text, w.saturating_sub(lw + 1)),
            muted(),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(left_spans)), rows[0]);

    // Line 2: filter prompt, toast, or quiet keybind strip
    let line2 = if app.filter_mode {
        Line::from(vec![
            Span::styled(
                " /过滤 ",
                Style::default()
                    .fg(STATUS_PENDING)
                    .add_modifier(Modifier::REVERSED | Modifier::BOLD),
            ),
            Span::styled(format!(" {}█ ", app.filter), bold_primary()),
            Span::styled("Enter 确认 · Esc 清除", muted()),
        ])
    } else if let Some((msg, _)) = &app.toast {
        Line::from(Span::styled(
            format!(" ✓ {}", truncate(msg, w.saturating_sub(3))),
            Style::default()
                .fg(STATUS_RUNNING)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        let keys = match app.tab {
            ViewTab::Runs => {
                if area.width >= 100 {
                    " jk 选择  1-6 任务·事件·合入·候选·工作·账号  w 工作  A 账号  Tab 切栏  Enter 结论  d diff  / 过滤  r 刷新  a/c/s/p 命令  ? 帮助  q 退出"
                } else {
                    " jk选 Tab栏 1-6切页 w工作 A账号 Enter结论 d diff /滤 r刷 a/c/s/p令 ?助 q退"
                }
            }
            ViewTab::Events => {
                if area.width >= 100 {
                    " jk 浏览事件  f 切换跟随  1-6 切页  w 工作  A 账号  Tab 切栏  / 过滤  r 刷新  ? 帮助  q 退出"
                } else {
                    " jk览 f跟随 1-6切页 w工作 A账号 Tab栏 /滤 r刷 ?助 q退"
                }
            }
            ViewTab::Merges => {
                if area.width >= 100 {
                    " jk 选择合入批次  a/Enter 生成 apply --keep-commits  1-6 切页  w 工作  A 账号  Tab 切栏  / 过滤  r 刷新  ? 帮助  q 退出"
                } else {
                    " jk选 a/Enter合回 1-6切页 w工作 A账号 Tab栏 /滤 r刷 ?助 q退"
                }
            }
            ViewTab::Candidates => {
                if area.width >= 100 {
                    " jk 选择候选  p/Enter 生成 radar pick 草稿命令  1-6 切页  w 工作  A 账号  Tab 切栏  / 过滤  r 刷新  ? 帮助  q 退出"
                } else {
                    " jk选 p/Enter挑草稿 1-6切页 w工作 A账号 Tab栏 /滤 r刷 ?助 q退"
                }
            }
            ViewTab::Work => {
                if area.width >= 100 {
                    " ↑↓/jk 选择  Enter 工作卡/折叠  t/→ 跳到 run  o 等你  x 已否  / 过滤  y 复制命令  w 切任务  6 账号  r 刷新  ? 帮助  q 退出"
                } else if area.width >= COMPACT_BREAKPOINT {
                    " jk选 Enter卡 t跳run o等你 x已否 /滤 y复 w任务 6账号 r刷 ?助 q退"
                } else {
                    " ↑↓选 Enter卡 t跳run o等你 x已否 w任务 q退"
                }
            }
            ViewTab::Accounts => {
                if area.width >= 100 {
                    " jk 选择账号  Enter/u 切换账号（就地确认）  y 复制命令  Tab 切栏  1-6 切页  / 过滤  r 刷新  ? 帮助  q 退出"
                } else {
                    " jk选 Enter/u切换 y复 Tab栏 1-6切页 /滤 r刷 ?助 q退"
                }
            }
        };
        Line::from(Span::styled(truncate(keys, w), muted()))
    };
    frame.render_widget(Paragraph::new(line2), rows[1]);
}

fn render_text_overlay(frame: &mut Frame, app: &mut App, area: Rect, is_result: bool) {
    let modal = area;
    frame.render_widget(Clear, modal);

    let (title, content, scroll_ref) = match &mut app.overlay {
        Overlay::FullResult {
            title,
            content,
            scroll,
            ..
        } => (format!(" 完整结论 · {title} "), content.as_str(), scroll),
        Overlay::DiffStat {
            title,
            content,
            scroll,
            ..
        } => (format!(" Diff 统计 · {title} "), content.as_str(), scroll),
        _ => return,
    };

    let inner_w = modal.width.saturating_sub(2) as usize;
    let inner_h = modal.height.saturating_sub(2) as usize;
    let rendered: Vec<Line<'static>> = if is_result {
        render_markdown(content, "", inner_w.max(8))
    } else {
        wrap_text(content, inner_w.max(8))
            .into_iter()
            .map(|l| colorize_diff_stat_line(&l))
            .collect()
    };
    let total = rendered.len();
    let max_scroll = total.saturating_sub(inner_h);
    if *scroll_ref > max_scroll {
        *scroll_ref = max_scroll;
    }
    let scroll = *scroll_ref;

    let footer = if is_result {
        format!(
            " {}/{} · jk 滚动 · PgUp/Dn 翻页 · P 分页器 · y 复制 · Esc 关闭 ",
            (scroll + 1).min(total.max(1)),
            total.max(1)
        )
    } else {
        format!(
            " {}/{} · jk 滚动 · y 复制 · Esc 关闭 ",
            (scroll + 1).min(total.max(1)),
            total.max(1)
        )
    };

    let block = rounded_block(
        Line::from(Span::styled(truncate(&title, inner_w), bold_primary())),
        true,
    )
    .title_bottom(Line::from(Span::styled(truncate(&footer, inner_w), muted())));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let lines: Vec<Line> = rendered
        .into_iter()
        .skip(scroll)
        .take(inner_h)
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn colorize_diff_stat_line(line: &str) -> Line<'static> {
    if let Some((left, right)) = line.split_once('|') {
        let mut spans = vec![
            Span::styled(left.to_string(), primary()),
            Span::styled("|", muted()),
        ];
        let mut plus = String::new();
        let mut minus = String::new();
        let mut prefix = String::new();
        for ch in right.chars() {
            match ch {
                '+' => plus.push(ch),
                '-' => minus.push(ch),
                _ if plus.is_empty() && minus.is_empty() => prefix.push(ch),
                _ => minus.push(ch),
            }
        }
        spans.push(Span::styled(prefix, muted()));
        if !plus.is_empty() {
            spans.push(Span::styled(plus, Style::default().fg(STATUS_RUNNING)));
        }
        if !minus.is_empty() {
            spans.push(Span::styled(minus, Style::default().fg(STATUS_FAILED)));
        }
        Line::from(spans)
    } else if line.contains("insertion") || line.contains("deletion") || line.contains("changed") {
        Line::from(Span::styled(line.to_string(), bold_primary()))
    } else {
        Line::from(Span::styled(line.to_string(), primary()))
    }
}

fn render_command_modal(
    frame: &mut Frame,
    area: Rect,
    action_label: &str,
    command: &str,
    note: &str,
) {
    let w = area.width.saturating_sub(6).clamp(36, 92);
    let h = if note.is_empty() { 7u16 } else { 9u16 }.min(area.height.saturating_sub(2));
    let modal = centered_rect(area, w, h);
    frame.render_widget(Clear, modal);

    let block = rounded_block(
        Line::from(Span::styled(
            format!(" {action_label} "),
            Style::default()
                .fg(STATUS_PENDING)
                .add_modifier(Modifier::BOLD),
        )),
        true,
    )
    .title_bottom(Line::from(Span::styled(
        " y/Enter 复制 · o 退出打印 · Esc 关闭 ",
        muted(),
    )));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let iw = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    for l in wrap_text(&format!("$ {command}"), iw) {
        lines.push(Line::from(Span::styled(l, bold_primary())));
    }
    if !note.is_empty() {
        lines.push(Line::raw(""));
        for l in wrap_text(note, iw) {
            lines.push(Line::from(Span::styled(
                l,
                Style::default().fg(STATUS_PENDING),
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_help_modal(frame: &mut Frame, area: Rect) {
    let w = area.width.saturating_sub(6).clamp(40, 74);
    let h = 18u16.min(area.height.saturating_sub(2));
    let modal = centered_rect(area, w, h);
    frame.render_widget(Clear, modal);

    let block = rounded_block(
        Line::from(Span::styled(" 快捷键 ", bold_primary())),
        true,
    )
    .title_bottom(Line::from(Span::styled(" Esc/q 关闭 ", muted())));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let rows = [
        ("↑↓ / j k", "选择列表项 / 滚动详情"),
        ("1-6 / w / A", "切换「任务 / 事件 / 合入 / 候选 / 工作表 / 账号」"),
        ("e / m / b", "快捷切换「事件 / 合入 / 候选」"),
        ("t / →", "工作表选中项跳到关联 run"),
        ("o / x", "工作表过滤「等你 / 已否」"),
        ("f", "事件页切换自动跟随"),
        ("Tab", "切换焦点栏"),
        ("Enter / u", "工作卡 / 完整结论 / 账号切换就地确认 / 命令"),
        ("d", "diff --stat 改动统计"),
        ("/", "过滤（Esc 清除）"),
        ("r", "立即刷新"),
        ("a / c / s / p", "生成 apply / clean / stop / reply (候选页 pick)"),
        ("y", "复制推荐/下一步命令"),
        ("q / Ctrl-C", "关闭浮层 / 退出"),
    ];
    let iw = inner.width as usize;
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, desc)| {
            Line::from(vec![
                Span::styled(pad_right(k, 14), bold_primary()),
                Span::raw(" "),
                Span::styled(truncate(desc, iw.saturating_sub(15)), muted()),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn centered_rect(area: Rect, w: u16, h: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w.min(area.width), h.min(area.height))
}

fn group_status_color(group: GroupKind) -> Option<Color> {
    match group {
        GroupKind::Pending => Some(STATUS_PENDING),
        GroupKind::Running => Some(STATUS_RUNNING),
        GroupKind::Queued => Some(STATUS_QUEUED),
        GroupKind::MergedUncleaned | GroupKind::Stopped => None,
    }
}

fn run_glyph_color(run: &RunItem) -> (&'static str, Option<Color>) {
    if run.question && !run.applied {
        return ("?", Some(STATUS_PENDING));
    }
    match run.group {
        GroupKind::Running => {
            if let Some(rev) = &run.review {
                if rev.status == "reviewing" {
                    return ("◎", Some(STATUS_QUEUED));
                }
                if rev.status == "reworking" {
                    return ("↻", Some(STATUS_PENDING));
                }
            }
            if run.stall_warning.is_some() {
                return ("!", Some(STATUS_PENDING));
            }
            if let Some(act) = &run.activity {
                return match act.kind.as_str() {
                    "quota" => ("!", Some(STATUS_PENDING)),
                    "error" => ("✕", Some(STATUS_FAILED)),
                    "waiting" => ("◔", Some(STATUS_QUEUED)),
                    _ => ("●", Some(STATUS_RUNNING)),
                };
            }
            ("●", Some(STATUS_RUNNING))
        }
        GroupKind::Queued => ("◔", Some(STATUS_QUEUED)),
        GroupKind::MergedUncleaned => ("○", None),
        GroupKind::Stopped => ("·", None),
        GroupKind::Pending => {
            if run
                .review
                .as_ref()
                .and_then(|r| r.verdict.as_deref())
                == Some("NEEDS_CONTROLLER")
            {
                return ("!", Some(STATUS_FAILED));
            }
            match run.state.as_str() {
                "rejected" | "failed" | "timeout" | "stalled" | "malformed" | "crashed"
                | "killed" => ("✕", Some(STATUS_FAILED)),
                _ => ("◆", Some(STATUS_PENDING)),
            }
        }
    }
}

fn render_dag_queue_row(q: &DagQueueItem, selected: bool, width: usize) -> Line<'static> {
    let (glyph, color) = match q.status.as_str() {
        "pending" | "dispatching" => ("▸", Some(STATUS_QUEUED)),
        "dispatched" => ("✓", Some(STATUS_RUNNING)),
        "paused" => ("!", Some(STATUS_FAILED)),
        _ => ("·", None),
    };
    let after = q.after.join(",");
    let extra = if let Some(reason) = &q.pause_reason {
        format!(" · {reason}")
    } else if let Some(rid) = &q.run_id {
        format!(" -> {rid}")
    } else {
        format!(" · after: {after}")
    };
    let text = format!("{} {} [{}{}]", q.status_badge(), q.name, q.repo, extra);
    if selected {
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        let accent = match color {
            Some(c) => Style::default().fg(c).add_modifier(Modifier::BOLD),
            None => bold_primary(),
        };
        let body = truncate(&text, width.saturating_sub(4));
        let used = 4 + display_width(&body);
        let pad = width.saturating_sub(used);
        return Line::from(vec![
            Span::styled("▎", accent),
            Span::styled(format!(" {glyph} "), rev.add_modifier(Modifier::BOLD)),
            Span::styled(body, rev),
            Span::styled(" ".repeat(pad), rev),
        ]);
    }
    let st = match color {
        Some(c) => Style::default().fg(c),
        None => muted(),
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{glyph} "), st.add_modifier(Modifier::BOLD)),
        Span::styled(truncate(&text, width.saturating_sub(4)), st),
    ])
}

fn render_dag_queue_detail_pane(
    frame: &mut Frame,
    app: &mut App,
    q: &DagQueueItem,
    area: Rect,
    focused: bool,
) {
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    let badge_color = match q.status.as_str() {
        "pending" | "dispatching" => STATUS_QUEUED,
        "dispatched" => STATUS_RUNNING,
        "paused" => STATUS_FAILED,
        _ => Color::DarkGray,
    };
    out.push(Line::from(vec![
        Span::styled(
            format!(" {} ", q.status_badge()),
            Style::default()
                .fg(badge_color)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(q.name.clone(), bold_primary()),
    ]));
    push_wrapped(
        &mut out,
        &format!("仓库: {} · 状态: {} · 创建于 {}", q.repo, q.status, q.created_at),
        inner_w,
        muted(),
    );
    out.push(Line::raw(""));
    out.push(section_header("上游依赖 (DAG)"));
    for dep in &q.after {
        push_indented_wrapped(&mut out, "  - ", dep, inner_w, primary());
    }
    if let Some(rid) = &q.run_id {
        out.push(Line::raw(""));
        out.push(section_header("已自动派发任务"));
        push_indented_wrapped(&mut out, "  ", rid, inner_w, Style::default().fg(STATUS_RUNNING));
    }
    if let Some(reason) = &q.pause_reason {
        out.push(Line::raw(""));
        out.push(section_header("暂停原因"));
        push_indented_wrapped(
            &mut out,
            "  ",
            reason,
            inner_w,
            Style::default()
                .fg(STATUS_FAILED)
                .add_modifier(Modifier::BOLD),
        );
    }
    out.push(Line::raw(""));
    out.push(section_header("提示词文件"));
    push_indented_wrapped(&mut out, "  ", &q.prompt_file, inner_w, muted());

    let max_scroll = out.len().saturating_sub(inner_h);
    if app.detail_scroll > max_scroll {
        app.detail_scroll = max_scroll;
    }
    let title = Line::from(Span::styled(
        format!(" 续派: {} ", truncate(&q.name, inner_w.saturating_sub(10))),
        if focused { bold_primary() } else { muted() },
    ));
    let block = rounded_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let visible: Vec<Line> = out
        .into_iter()
        .skip(app.detail_scroll)
        .take(inner_h)
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn event_kind_color(kind: &str) -> Option<Color> {
    match kind {
        "delivered" | "applied" | "queue_dispatched" | "merge_done" | "radar_done"
        | "premerge_ready" => Some(STATUS_RUNNING),
        "question" | "stall_warning" | "loop_review" | "review_verdict" | "rework_sent"
        | "merge_item" => Some(STATUS_PENDING),
        "rejected" | "failed" | "queue_paused" | "needs_attention" | "premerge_conflict"
        | "premerge_failed" => Some(STATUS_FAILED),
        "started" | "queued" | "lane_acquired" | "review_started" | "merge_started" => {
            Some(STATUS_QUEUED)
        }
        _ => None,
    }
}

fn short_event_hms(at: &str) -> &str {
    if at.len() >= 19 {
        &at[11..19]
    } else {
        at
    }
}

fn render_events_list_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::List && matches!(app.overlay, Overlay::None);
    let follow_tag = if app.events_follow { " · 跟随" } else { " · 暂停跟随" };
    let filter_tag = if app.filter.is_empty() { "" } else { " · 过滤" };
    let title = Line::from(Span::styled(
        format!(
            " 事件流 ({}){} {} ",
            app.filtered_events.len(),
            follow_tag,
            filter_tag
        ),
        if focused { bold_primary() } else { muted() },
    ));
    let block = rounded_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    if app.filtered_events.is_empty() {
        let msg = if app.filter.is_empty() {
            "暂无事件记录"
        } else {
            "无匹配事件"
        };
        frame.render_widget(Paragraph::new(Line::from(Span::styled(format!(" {msg}"), muted()))), inner);
        return;
    }

    let inner_w = inner.width as usize;
    let view_h = inner.height as usize;
    if let Some(sel) = app.selected_event {
        if sel < app.events_scroll {
            app.events_scroll = sel;
        } else if view_h > 0 && sel >= app.events_scroll + view_h {
            app.events_scroll = sel + 1 - view_h;
        }
    }
    let max_scroll = app.filtered_events.len().saturating_sub(view_h);
    if app.events_scroll > max_scroll {
        app.events_scroll = max_scroll;
    }

    let mut lines: Vec<Line> = Vec::with_capacity(view_h);
    for (pos, &ev_idx) in app
        .filtered_events
        .iter()
        .enumerate()
        .skip(app.events_scroll)
        .take(view_h)
    {
        let ev = &app.snapshot.events[ev_idx];
        let selected = app.selected_event == Some(pos);
        let color = event_kind_color(&ev.kind);
        let prefix = format!(
            "#{:<4} {} {} ",
            ev.seq,
            short_event_hms(&ev.at),
            pad_right(ev.kind_badge(), 4)
        );
        let repo_part = if ev.repo.is_empty() {
            String::new()
        } else {
            format!("[{}] ", ev.repo)
        };
        let rest_avail = inner_w.saturating_sub(2 + display_width(&prefix));
        let rest = truncate(&format!("{repo_part}{}", ev.summary), rest_avail);

        if selected {
            let rev = Style::default().add_modifier(Modifier::REVERSED);
            let accent = match color {
                Some(c) => Style::default().fg(c).add_modifier(Modifier::BOLD),
                None => bold_primary(),
            };
            let used = 2 + display_width(&prefix) + display_width(&rest);
            let pad = inner_w.saturating_sub(used);
            lines.push(Line::from(vec![
                Span::styled("▎", accent),
                Span::styled(" ", rev),
                Span::styled(prefix, rev.add_modifier(Modifier::BOLD)),
                Span::styled(rest, rev),
                Span::styled(" ".repeat(pad), rev),
            ]));
        } else {
            let badge_style = match color {
                Some(c) => Style::default().fg(c).add_modifier(Modifier::BOLD),
                None => muted(),
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("#{:<4} ", ev.seq), muted()),
                Span::styled(format!("{} ", short_event_hms(&ev.at)), muted()),
                Span::styled(format!("{} ", pad_right(ev.kind_badge(), 4)), badge_style),
                Span::styled(rest, primary()),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_event_detail_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::Detail && matches!(app.overlay, Overlay::None);
    let Some(ev) = app.selected_event_item().cloned() else {
        let block = rounded_block(Line::from(Span::styled(" 事件详情 ", muted())), focused);
        frame.render_widget(block, area);
        return;
    };
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    let badge_style = match event_kind_color(&ev.kind) {
        Some(c) => Style::default()
            .fg(c)
            .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        None => Style::default().add_modifier(Modifier::REVERSED),
    };
    out.push(Line::from(vec![
        Span::styled(format!(" {} ", ev.kind_badge()), badge_style),
        Span::raw(" "),
        Span::styled(format!("#{} · {}", ev.seq, ev.kind), bold_primary()),
    ]));
    push_wrapped(&mut out, &format!("时间: {}", ev.at), inner_w, muted());
    if !ev.repo.is_empty() {
        push_wrapped(&mut out, &format!("仓库: {}", ev.repo), inner_w, muted());
    }
    if !ev.run.is_empty() {
        push_wrapped(&mut out, &format!("任务: {}", ev.run), inner_w, primary());
    }
    if !ev.summary.is_empty() {
        out.push(Line::raw(""));
        out.push(section_header("摘要"));
        push_indented_wrapped(&mut out, "  ", &ev.summary, inner_w, primary());
    }
    out.push(Line::raw(""));
    out.push(section_header("Payload"));
    for line in ev.payload_pretty.lines() {
        push_indented_wrapped(&mut out, "  ", line, inner_w, muted());
    }

    let max_scroll = out.len().saturating_sub(inner_h);
    if app.detail_scroll > max_scroll {
        app.detail_scroll = max_scroll;
    }
    let title = Line::from(Span::styled(
        format!(" 事件 #{} ({}) ", ev.seq, ev.kind),
        if focused { bold_primary() } else { muted() },
    ));
    let block = rounded_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let visible: Vec<Line> = out
        .into_iter()
        .skip(app.detail_scroll)
        .take(inner_h)
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn render_merges_list_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::List && matches!(app.overlay, Overlay::None);
    let filter_tag = if app.filter.is_empty() { "" } else { " · 过滤" };
    let title = Line::from(Span::styled(
        format!(" 合入批次 ({}){} ", app.filtered_merges.len(), filter_tag),
        if focused { bold_primary() } else { muted() },
    ));
    let block = rounded_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    if app.filtered_merges.is_empty() {
        let msg = if app.filter.is_empty() {
            "暂无合入批次（运行 delegate merge --ready 生成）"
        } else {
            "无匹配合入批次"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {}", truncate(msg, inner.width.saturating_sub(1) as usize)),
                muted(),
            ))),
            inner,
        );
        return;
    }

    let inner_w = inner.width as usize;
    let view_h = inner.height as usize;
    if let Some(sel) = app.selected_merge {
        if sel < app.merges_scroll {
            app.merges_scroll = sel;
        } else if view_h > 0 && sel >= app.merges_scroll + view_h {
            app.merges_scroll = sel + 1 - view_h;
        }
    }
    let max_scroll = app.filtered_merges.len().saturating_sub(view_h);
    if app.merges_scroll > max_scroll {
        app.merges_scroll = max_scroll;
    }

    let mut lines: Vec<Line> = Vec::with_capacity(view_h);
    for (pos, &idx) in app
        .filtered_merges
        .iter()
        .enumerate()
        .skip(app.merges_scroll)
        .take(view_h)
    {
        let m = &app.snapshot.merges[idx];
        let selected = app.selected_merge == Some(pos);
        let (glyph, color) = if m.applied {
            ("○", None)
        } else if m.merged_count > 0 && m.merge_accept_ok {
            ("✓", Some(STATUS_RUNNING))
        } else if m.merged_count == 0 {
            ("✕", Some(STATUS_FAILED))
        } else {
            ("!", Some(STATUS_PENDING))
        };
        let auto_tag = if m.all_auto_merge {
            "自主"
        } else if m.auto_merge_count > 0 {
            "含自主"
        } else {
            "待点头"
        };
        let state_tag = if m.applied { "已合回" } else { auto_tag };
        let summary = format!(
            "[{}] {}  合{}/跳{} · {}",
            m.repo_label, m.merge_run, m.merged_count, m.skipped_count, state_tag
        );
        let text = truncate(&summary, inner_w.saturating_sub(4));
        if selected {
            let rev = Style::default().add_modifier(Modifier::REVERSED);
            let accent = match color {
                Some(c) => Style::default().fg(c).add_modifier(Modifier::BOLD),
                None => bold_primary(),
            };
            let used = 4 + display_width(&text);
            let pad = inner_w.saturating_sub(used);
            lines.push(Line::from(vec![
                Span::styled("▎", accent),
                Span::styled(format!(" {glyph} "), rev.add_modifier(Modifier::BOLD)),
                Span::styled(text, rev),
                Span::styled(" ".repeat(pad), rev),
            ]));
        } else {
            let st = match color {
                Some(c) => Style::default().fg(c).add_modifier(Modifier::BOLD),
                None => muted(),
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{glyph} "), st),
                Span::styled(text, if m.applied { muted() } else { primary() }),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_merge_detail_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::Detail && matches!(app.overlay, Overlay::None);
    let Some(m) = app.selected_merge_item().cloned() else {
        let block = rounded_block(Line::from(Span::styled(" 合入详情 ", muted())), focused);
        frame.render_widget(block, area);
        return;
    };
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    let badge_color = if m.applied {
        Color::DarkGray
    } else if m.merged_count > 0 && m.merge_accept_ok {
        STATUS_RUNNING
    } else {
        STATUS_FAILED
    };
    let badge_text = if m.applied {
        "已合回"
    } else if m.merged_count > 0 && m.merge_accept_ok {
        "待主控 apply"
    } else {
        "需检查"
    };
    out.push(Line::from(vec![
        Span::styled(
            format!(" {badge_text} "),
            Style::default()
                .fg(badge_color)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(m.merge_run.clone(), bold_primary()),
    ]));
    let base_short = &m.base_head[..m.base_head.len().min(7)];
    let head_short = &m.head[..m.head.len().min(7)];
    push_wrapped(
        &mut out,
        &format!(
            "仓库: {} · 基线 {base_short} → {head_short} · 合入 {} / 跳过 {} / 可自主合入 {}",
            m.repo_label, m.merged_count, m.skipped_count, m.auto_merge_count
        ),
        inner_w,
        muted(),
    );
    let ma_label = if m.merge_accept_skipped {
        "未执行".to_string()
    } else if m.merge_accept_ok {
        format!("PASS ({})", m.merge_accept_cmd)
    } else {
        format!("FAIL ({})", m.merge_accept_cmd)
    };
    push_wrapped(&mut out, &format!("总验收 mergeAccept: {ma_label}"), inner_w, primary());
    if !m.report_path.is_empty() {
        push_wrapped(&mut out, &format!("报告: {}", m.report_path), inner_w, muted());
    }

    out.push(Line::raw(""));
    out.push(section_header("逐项明细"));
    for it in &m.items {
        let is_merged = it.status == "MERGED";
        let st_style = if is_merged {
            Style::default().fg(STATUS_RUNNING).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(STATUS_FAILED).add_modifier(Modifier::BOLD)
        };
        let short_commit = &it.commit[..it.commit.len().min(7)];
        let auto_str = if is_merged {
            if it.auto_merge {
                " [可自主合入]"
            } else {
                " [待主控点头]"
            }
        } else {
            ""
        };
        let acc_str = match it.accept_ok {
            Some(true) => " 验✓",
            Some(false) => " 验✗",
            None => "",
        };
        let header = if is_merged {
            format!("{} {} {}{acc_str}{auto_str}", it.run, short_commit, it.title)
        } else {
            format!("{} {}", it.run, it.reason)
        };
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:<7} ", it.status), st_style),
            Span::styled(truncate(&header, inner_w.saturating_sub(10)), primary()),
        ]));
    }

    out.push(Line::raw(""));
    out.push(section_header("下一步（按 a 复制）"));
    push_indented_wrapped(
        &mut out,
        "  ",
        &format!("delegate apply --keep-commits {}", m.merge_run),
        inner_w,
        Style::default().fg(STATUS_QUEUED),
    );

    let max_scroll = out.len().saturating_sub(inner_h);
    if app.detail_scroll > max_scroll {
        app.detail_scroll = max_scroll;
    }
    let title = Line::from(Span::styled(
        format!(" 合入: {} ", truncate(&m.merge_run, inner_w.saturating_sub(10))),
        if focused { bold_primary() } else { muted() },
    ));
    let block = rounded_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let visible: Vec<Line> = out
        .into_iter()
        .skip(app.detail_scroll)
        .take(inner_h)
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn render_candidates_list_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::List && matches!(app.overlay, Overlay::None);
    let filter_tag = if app.filter.is_empty() { "" } else { " · 过滤" };
    let title = Line::from(Span::styled(
        format!(
            " Radar 候选板 ({}){} ",
            app.filtered_candidates.len(),
            filter_tag
        ),
        if focused { bold_primary() } else { muted() },
    ));
    let block = rounded_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    if app.filtered_candidates.is_empty() {
        let msg = if app.filter.is_empty() {
            "候选板为空（运行 delegate radar 采集侦察）"
        } else {
            "无匹配候选"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {}", truncate(msg, inner.width.saturating_sub(1) as usize)),
                muted(),
            ))),
            inner,
        );
        return;
    }

    let inner_w = inner.width as usize;
    let view_h = inner.height as usize;
    if let Some(sel) = app.selected_candidate {
        if sel < app.candidates_scroll {
            app.candidates_scroll = sel;
        } else if view_h > 0 && sel >= app.candidates_scroll + view_h {
            app.candidates_scroll = sel + 1 - view_h;
        }
    }
    let max_scroll = app.filtered_candidates.len().saturating_sub(view_h);
    if app.candidates_scroll > max_scroll {
        app.candidates_scroll = max_scroll;
    }

    let mut lines: Vec<Line> = Vec::with_capacity(view_h);
    for (pos, &idx) in app
        .filtered_candidates
        .iter()
        .enumerate()
        .skip(app.candidates_scroll)
        .take(view_h)
    {
        let c = &app.snapshot.candidates[idx];
        let selected = app.selected_candidate == Some(pos);
        let (glyph, color) = if !c.picked_at.is_empty() {
            ("✓", None)
        } else if c.brief_ready {
            ("◆", Some(STATUS_RUNNING))
        } else {
            ("◇", Some(STATUS_PENDING))
        };
        let ready_tag = if !c.picked_at.is_empty() {
            "已挑"
        } else if c.brief_ready {
            "可直派"
        } else {
            "需细化"
        };
        let summary = format!(
            "{} [{}] [{}] {}",
            c.id, c.repo_label, ready_tag, c.title
        );
        let text = truncate(&summary, inner_w.saturating_sub(4));
        if selected {
            let rev = Style::default().add_modifier(Modifier::REVERSED);
            let accent = match color {
                Some(col) => Style::default().fg(col).add_modifier(Modifier::BOLD),
                None => bold_primary(),
            };
            let used = 4 + display_width(&text);
            let pad = inner_w.saturating_sub(used);
            lines.push(Line::from(vec![
                Span::styled("▎", accent),
                Span::styled(format!(" {glyph} "), rev.add_modifier(Modifier::BOLD)),
                Span::styled(text, rev),
                Span::styled(" ".repeat(pad), rev),
            ]));
        } else {
            let st = match color {
                Some(col) => Style::default().fg(col).add_modifier(Modifier::BOLD),
                None => muted(),
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{glyph} "), st),
                Span::styled(text, primary()),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_candidate_detail_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::Detail && matches!(app.overlay, Overlay::None);
    let Some(c) = app.selected_candidate_item().cloned() else {
        let block = rounded_block(Line::from(Span::styled(" 候选详情 ", muted())), focused);
        frame.render_widget(block, area);
        return;
    };
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    let (badge, badge_color) = if c.brief_ready {
        ("可直接写成任务说明", STATUS_RUNNING)
    } else {
        ("需主控细化", STATUS_PENDING)
    };
    out.push(Line::from(vec![
        Span::styled(
            format!(" {badge} "),
            Style::default()
                .fg(badge_color)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(format!("[{}] {}", c.id, c.title), bold_primary()),
    ]));
    push_wrapped(
        &mut out,
        &format!(
            "仓库: {} · 粗估成本: {} · 首次发现: {}",
            c.repo_label, c.cost, c.first_seen_at
        ),
        inner_w,
        muted(),
    );
    if !c.brief_note.is_empty() {
        push_wrapped(&mut out, &format!("说明: {}", c.brief_note), inner_w, muted());
    }
    if !c.picked_at.is_empty() {
        push_wrapped(&mut out, &format!("已于 {} 生成任务草稿", c.picked_at), inner_w, muted());
    }

    out.push(Line::raw(""));
    out.push(section_header("证据锚点"));
    for ev in &c.evidence {
        push_indented_wrapped(&mut out, "  - ", ev, inner_w, Style::default().fg(STATUS_QUEUED));
    }

    out.push(Line::raw(""));
    out.push(section_header("建议做法"));
    for line in c.suggestion.lines() {
        push_indented_wrapped(&mut out, "  ", line, inner_w, primary());
    }

    out.push(Line::raw(""));
    out.push(section_header("挑选草稿（按 p 复制，不自动派活）"));
    push_indented_wrapped(
        &mut out,
        "  ",
        &format!("delegate radar pick {}", c.id),
        inner_w,
        Style::default().fg(STATUS_RUNNING),
    );
    if !c.board_path.is_empty() {
        push_wrapped(&mut out, &format!("候选板: {}", c.board_path), inner_w, muted());
    }

    let max_scroll = out.len().saturating_sub(inner_h);
    if app.detail_scroll > max_scroll {
        app.detail_scroll = max_scroll;
    }
    let title = Line::from(Span::styled(
        format!(" 候选: {} ", c.id),
        if focused { bold_primary() } else { muted() },
    ));
    let block = rounded_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let visible: Vec<Line> = out
        .into_iter()
        .skip(app.detail_scroll)
        .take(inner_h)
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

// ---------------------------------------------------------------------------
// Work Table View: L0 top bar, L1 grouped + dependency-indented list, L2 Work Card
// ---------------------------------------------------------------------------

fn render_work_top_bar(frame: &mut Frame, app: &App, area: Rect) {
    let c = app.snapshot.work_counts();

    let count_span = |label: &str, n: usize, color: Option<Color>| -> Span<'static> {
        let style = if n > 0 {
            match color {
                Some(col) => Style::default().fg(col).add_modifier(Modifier::BOLD),
                None => bold_primary(),
            }
        } else {
            muted()
        };
        Span::styled(format!("{label} {n}"), style)
    };

    let filter_badge: Option<Span<'static>> = match app.work_filter_mode {
        WorkFilterMode::DroppedOnly => Some(Span::styled(
            " 滤:已否 ",
            Style::default()
                .fg(STATUS_FAILED)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        )),
        WorkFilterMode::OwnerOnly => Some(Span::styled(
            " 滤:等你 ",
            Style::default()
                .fg(STATUS_PENDING)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        )),
        WorkFilterMode::All if !app.filter.is_empty() => Some(Span::styled(
            format!(" 滤:{} ", truncate(&app.filter, 12)),
            Style::default()
                .fg(STATUS_PENDING)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        )),
        WorkFilterMode::All => None,
    };

    if area.width < COMPACT_BREAKPOINT {
        // Two-line L0 top bar for narrow/mobile terminals (40-69 cols)
        let sep = if area.width >= 48 { " · " } else { "·" };
        let tab_label = if area.width >= 46 { " 5工作 " } else { "工作" };
        let line1 = Line::from(vec![
            Span::styled(
                tab_label,
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD),
            ),
            Span::raw(" "),
            count_span("等你", c.owner, Some(STATUS_FAILED)),
            Span::styled(sep, muted()),
            count_span("需处理", c.needs_action, Some(STATUS_PENDING)),
            Span::styled(sep, muted()),
            count_span("就绪", c.ready, Some(STATUS_QUEUED)),
            Span::styled(sep, muted()),
            count_span("推进中", c.in_progress, Some(STATUS_RUNNING)),
        ]);
        let mut line2_spans = vec![
            Span::raw(" "),
            count_span("等待", c.waiting, None),
            Span::styled(sep, muted()),
            count_span("暂缓", c.deferred, None),
            Span::styled(sep, muted()),
            count_span("今天完成", c.done_today, Some(STATUS_RUNNING)),
        ];
        if let Some(fb) = filter_badge {
            line2_spans.push(Span::raw(" "));
            line2_spans.push(fb);
        }
        frame.render_widget(Paragraph::new(vec![line1, Line::from(line2_spans)]), area);
        return;
    }

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(28);
    if area.width >= 115 {
        spans.push(Span::styled(
            " delegate-tui ",
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(" 1任务 ", muted()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled("2事 3合 4候", muted()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            " 5工作 ",
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled("6账", muted()));
        spans.push(Span::raw("  "));
    } else {
        spans.push(Span::styled(
            " 5工作 ",
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
    }

    let sep = if area.width >= 82 { " · " } else { "·" };
    spans.push(count_span("等你", c.owner, Some(STATUS_FAILED)));
    spans.push(Span::styled(sep, muted()));
    spans.push(count_span("需处理", c.needs_action, Some(STATUS_PENDING)));
    spans.push(Span::styled(sep, muted()));
    spans.push(count_span("就绪", c.ready, Some(STATUS_QUEUED)));
    spans.push(Span::styled(sep, muted()));
    spans.push(count_span("推进中", c.in_progress, Some(STATUS_RUNNING)));
    spans.push(Span::styled(sep, muted()));
    spans.push(count_span("等待", c.waiting, None));
    spans.push(Span::styled(sep, muted()));
    spans.push(count_span("暂缓", c.deferred, None));
    spans.push(Span::styled(sep, muted()));
    spans.push(count_span("今天完成", c.done_today, Some(STATUS_RUNNING)));

    if let Some(fb) = filter_badge {
        spans.push(Span::raw(" "));
        spans.push(fb);
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn work_bucket_color(bucket: WorkBucket) -> Option<Color> {
    match bucket {
        WorkBucket::Owner => Some(STATUS_FAILED),
        WorkBucket::NeedsAction => Some(STATUS_PENDING),
        WorkBucket::InProgress => Some(STATUS_RUNNING),
        WorkBucket::Ready => Some(STATUS_QUEUED),
        WorkBucket::Waiting | WorkBucket::Deferred | WorkBucket::Done | WorkBucket::Dropped => None,
    }
}

fn work_item_glyph_color(w: &WorkItem) -> (&'static str, Option<Color>) {
    if w.state == "done" {
        return if w.closed_kind == "dropped" {
            ("✕", Some(STATUS_FAILED))
        } else {
            ("✓", None)
        };
    }
    if let Some(ow) = &w.owner {
        return match ow.kind {
            OwnerKind::Act => ("★", Some(STATUS_FAILED)),
            OwnerKind::Nod => ("◆", Some(STATUS_PENDING)),
            OwnerKind::Card => ("◈", Some(STATUS_PENDING)),
            OwnerKind::Direction => ("◇", Some(STATUS_QUEUED)),
        };
    }
    match w.state.as_str() {
        "attention" => ("!", Some(STATUS_PENDING)),
        "verify" => ("◎", Some(STATUS_PENDING)),
        "active" => ("●", Some(STATUS_RUNNING)),
        "ready" => ("▸", Some(STATUS_QUEUED)),
        "blocked" => ("◌", None),
        "deferred" => ("·", None),
        _ => ("·", None),
    }
}

fn render_work_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = matches!(app.overlay, Overlay::None);
    let compact = area.width < COMPACT_BREAKPOINT;
    let c = app.snapshot.work_counts();
    let open_total = c.owner + c.needs_action + c.ready + c.in_progress + c.waiting + c.deferred;
    let mode_tag = match app.work_filter_mode {
        WorkFilterMode::All => {
            if app.filter.is_empty() {
                format!(" 工作表 ({open_total} 进行 · {} 完成) ", c.done_total)
            } else {
                format!(" 工作表 · 滤:{} ", truncate(&app.filter, 16))
            }
        }
        WorkFilterMode::OwnerOnly => format!(" 工作表 · 等你 ({}) ", c.owner),
        WorkFilterMode::DroppedOnly => format!(" 工作表 · 已否 ({}) ", c.dropped_total),
    };

    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;

    if app.work_rows.is_empty() {
        let block = rounded_block(
            Line::from(Span::styled(
                truncate(&mode_tag, inner_w.max(8)),
                if focused { bold_primary() } else { muted() },
            )),
            focused,
        );
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let msg = if app.loading {
            "加载中…".to_string()
        } else if app.work_filter_mode == WorkFilterMode::DroppedOnly {
            "暂无已否/已放弃的工作项".to_string()
        } else if app.work_filter_mode == WorkFilterMode::OwnerOnly {
            "暂无所有者事项（用 delegate work add --owner-kind 新建）".to_string()
        } else if !app.filter.is_empty() {
            format!("无匹配“{}”的工作项", app.filter)
        } else {
            "暂无工作项（用 delegate work add 新建）".to_string()
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {}", truncate(&msg, inner_w.saturating_sub(1))),
                muted(),
            ))),
            inner,
        );
        return;
    }

    let show_col_header = !compact && inner_h >= 4;
    let header_h: usize = usize::from(show_col_header);
    let view_h = inner_h.saturating_sub(header_h);

    // Compute exact line offsets for variable-height rows (2 lines/item in compact mode)
    let mut row_spans_meta: Vec<(usize, usize)> = Vec::with_capacity(app.work_rows.len());
    let mut total_lines = 0usize;
    for row in &app.work_rows {
        let h = match row {
            WorkRow::Item { .. } if compact => 2,
            _ => 1,
        };
        row_spans_meta.push((total_lines, h));
        total_lines += h;
    }

    if let Some(sel) = app.selected_work_row {
        if let Some(&(sel_start, sel_h)) = row_spans_meta.get(sel) {
            if sel_start < app.work_scroll_line {
                // Keep preceding group header visible when jumping upward
                app.work_scroll_line = sel_start.saturating_sub(1);
            } else if view_h > 0 && sel_start + sel_h > app.work_scroll_line + view_h {
                app.work_scroll_line = (sel_start + sel_h).saturating_sub(view_h);
            }
        }
    }
    let max_scroll = total_lines.saturating_sub(view_h);
    if app.work_scroll_line > max_scroll {
        app.work_scroll_line = max_scroll;
    }

    let scroll_hint = if total_lines > view_h {
        let first_v = (app.work_scroll_line + 1).min(total_lines);
        let last_v = (app.work_scroll_line + view_h).min(total_lines);
        format!(" {first_v}-{last_v}/{total_lines} ")
    } else {
        String::new()
    };

    let mut block = rounded_block(
        Line::from(Span::styled(
            truncate(&mode_tag, inner_w.max(8)),
            if focused { bold_primary() } else { muted() },
        )),
        focused,
    );
    if !scroll_hint.is_empty() {
        block = block.title_bottom(Line::from(Span::styled(scroll_hint, muted())));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::with_capacity(inner_h);
    let cols = WorkColWidths::compute(inner_w);
    if show_col_header {
        lines.push(render_work_col_header(cols, inner_w));
    }

    let win_start = app.work_scroll_line;
    let win_end = win_start + view_h;

    for (row_idx, row) in app.work_rows.iter().enumerate() {
        let (r_start, r_h) = row_spans_meta[row_idx];
        if r_start + r_h <= win_start || r_start >= win_end {
            continue;
        }
        let selected = app.selected_work_row == Some(row_idx);
        let rendered_row: Vec<Line<'static>> = match row {
            WorkRow::GroupHeader { bucket, count } => {
                vec![render_work_group_header(*bucket, *count, inner_w)]
            }
            WorkRow::FoldToggle {
                kind,
                count,
                expanded,
            } => vec![render_work_fold_row(*kind, *count, *expanded, selected, inner_w)],
            WorkRow::Item {
                work_idx,
                depth,
                branch_guides,
                is_last_sibling,
                extra_deps,
            } => {
                let item = &app.snapshot.work[*work_idx];
                let live = app.snapshot.work_live_status(item);
                if compact {
                    render_work_item_compact(
                        item,
                        &live,
                        *depth,
                        *extra_deps,
                        selected,
                        inner_w,
                    )
                } else {
                    vec![render_work_item_wide(
                        item,
                        &live,
                        *depth,
                        branch_guides,
                        *is_last_sibling,
                        *extra_deps,
                        selected,
                        cols,
                        inner_w,
                    )]
                }
            }
        };

        for (offset, ln) in rendered_row.into_iter().enumerate() {
            let line_pos = r_start + offset;
            if line_pos >= win_start && line_pos < win_end {
                lines.push(ln);
            }
        }
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

#[derive(Clone, Copy)]
struct WorkColWidths {
    id_w: usize,
    repo_w: usize,
    title_w: usize,
    state_w: usize,
    status_w: usize,
}

impl WorkColWidths {
    fn compute(width: usize) -> Self {
        let id_w = 4;
        let repo_w = if width >= 100 { 10 } else { 8 };
        let state_w = 6;
        let status_w = if width >= 110 {
            34
        } else if width >= 92 {
            28
        } else {
            22
        };
        // left: "▎ ● " (4) + id_w + 1 + repo_w + 1 + title_w + 1 + state_w + 1 + status_w
        let fixed = 4 + id_w + 1 + repo_w + 1 + 1 + state_w + 1 + status_w;
        let title_w = width.saturating_sub(fixed).max(8);
        Self {
            id_w,
            repo_w,
            title_w,
            state_w,
            status_w,
        }
    }
}

fn render_work_col_header(cols: WorkColWidths, width: usize) -> Line<'static> {
    let s = format!(
        "    {} {} {} {} {}",
        pad_right("ID", cols.id_w),
        pad_right("仓库", cols.repo_w),
        pad_right("标题", cols.title_w),
        pad_right("状态", cols.state_w),
        pad_right("此刻在干什么", cols.status_w),
    );
    Line::from(Span::styled(truncate(&s, width), muted()))
}

fn render_work_group_header(bucket: WorkBucket, count: usize, width: usize) -> Line<'static> {
    let style = match work_bucket_color(bucket) {
        Some(c) => Style::default().fg(c).add_modifier(Modifier::BOLD),
        None => muted().add_modifier(Modifier::BOLD),
    };
    let label = match bucket {
        WorkBucket::Owner => format!(" ▾ 等你 ({count}) — 动手 → 点头 → 看卡 → 方向"),
        _ => format!(" ▾ {} ({count})", bucket.label()),
    };
    Line::from(Span::styled(truncate(&label, width), style))
}

fn render_work_fold_row(
    kind: WorkFoldKind,
    count: usize,
    expanded: bool,
    selected: bool,
    width: usize,
) -> Line<'static> {
    let arrow = if expanded { "▾" } else { "▸" };
    let label = match kind {
        WorkFoldKind::Done => format!("{arrow} 已完成 {count} 条"),
        WorkFoldKind::Dropped => format!("{arrow} 已否/已放弃 {count} 条"),
    };
    if selected {
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        let hint = if width >= COMPACT_BREAKPOINT as usize {
            if expanded {
                "Enter 折叠 "
            } else {
                "Enter 展开 "
            }
        } else if expanded {
            "折叠 "
        } else {
            "展开 "
        };
        let hint_w = display_width(hint);
        let avail = width.saturating_sub(2 + hint_w + 1);
        let clipped = truncate(&label, avail);
        let gap = width.saturating_sub(2 + display_width(&clipped) + hint_w);
        return Line::from(vec![
            Span::styled("▎", bold_primary()),
            Span::styled(" ", rev),
            Span::styled(clipped, rev.add_modifier(Modifier::BOLD)),
            Span::styled(" ".repeat(gap), rev),
            Span::styled(hint, rev),
        ]);
    }
    Line::from(Span::styled(truncate(&format!(" {label}"), width), muted()))
}

fn format_tree_prefix_wide(depth: usize, branch_guides: &[bool], is_last_sibling: bool) -> String {
    if depth == 0 {
        return String::new();
    }
    let capped = depth.min(4);
    let mut s = String::new();
    for level in 0..capped.saturating_sub(1) {
        let has_more = branch_guides.get(level).copied().unwrap_or(false);
        if has_more {
            s.push_str("│  ");
        } else {
            s.push_str("   ");
        }
    }
    if is_last_sibling {
        s.push_str("└─ ");
    } else {
        s.push_str("├─ ");
    }
    s
}

fn format_tree_prefix_compact(depth: usize) -> String {
    if depth == 0 {
        return String::new();
    }
    let capped = depth.min(4);
    format!("{}↳ ", " ".repeat(capped.saturating_sub(1)))
}

#[allow(clippy::too_many_arguments)]
fn render_work_item_wide(
    item: &WorkItem,
    live_status: &str,
    depth: usize,
    branch_guides: &[bool],
    is_last_sibling: bool,
    extra_deps: usize,
    selected: bool,
    cols: WorkColWidths,
    width: usize,
) -> Line<'static> {
    let (glyph, status_color) = work_item_glyph_color(item);
    let tree_prefix = format_tree_prefix_wide(depth, branch_guides, is_last_sibling);
    let tree_w = display_width(&tree_prefix);

    let id_col = pad_right(&item.id, cols.id_w);
    let repo_col = pad_right(&item.repo_label, cols.repo_w);

    // Indent sits before ID and borrows width from the title column so state & live status stay aligned
    let title_w = cols.title_w.saturating_sub(tree_w).max(6);
    let title_col = pad_right(item.display_title(), title_w);
    let state_col = pad_right(item.state_badge(), cols.state_w);

    let extra_tag = if extra_deps > 0 {
        format!(" +{extra_deps}")
    } else {
        String::new()
    };
    let extra_w = display_width(&extra_tag);
    let status_avail = cols.status_w.saturating_sub(extra_w);
    let status_core = truncate(live_status, status_avail);
    let status_full = format!("{status_core}{extra_tag}");

    if selected {
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        let rev_bold = rev.add_modifier(Modifier::BOLD);
        let accent = match status_color {
            Some(c) => Style::default().fg(c).add_modifier(Modifier::BOLD),
            None => bold_primary(),
        };
        let mut spans = vec![
            Span::styled("▎", accent),
            Span::styled(" ", rev),
        ];
        if !tree_prefix.is_empty() {
            spans.push(Span::styled(tree_prefix, rev));
        }
        spans.push(Span::styled(format!("{glyph} "), rev_bold));
        spans.push(Span::styled(id_col, rev_bold));
        spans.push(Span::styled(" ", rev));
        spans.push(Span::styled(repo_col, rev));
        spans.push(Span::styled(" ", rev));
        spans.push(Span::styled(title_col, rev_bold));
        spans.push(Span::styled(" ", rev));
        spans.push(Span::styled(state_col, rev_bold));
        spans.push(Span::styled(" ", rev));
        spans.push(Span::styled(status_full, rev));

        let used: usize = spans.iter().map(|s| display_width(&s.content)).sum();
        if used < width {
            spans.push(Span::styled(" ".repeat(width - used), rev));
        }
        return Line::from(spans);
    }

    let state_style = match status_color {
        Some(c) => Style::default().fg(c),
        None => muted(),
    };
    let title_style = if item.state == "done" {
        muted()
    } else if item.owner.is_some() {
        bold_primary()
    } else {
        primary()
    };
    let status_style = if item.owner.is_some() && item.state != "done" {
        Style::default().fg(STATUS_PENDING)
    } else if matches!(item.state.as_str(), "active" | "attention" | "verify") {
        state_style
    } else {
        muted()
    };

    let mut spans = vec![Span::raw("  ")];
    if !tree_prefix.is_empty() {
        spans.push(Span::styled(tree_prefix, muted()));
    }
    spans.push(Span::styled(
        format!("{glyph} "),
        state_style.add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(id_col, bold_primary()));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(repo_col, muted()));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(title_col, title_style));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(state_col, state_style));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(status_core, status_style));
    if !extra_tag.is_empty() {
        spans.push(Span::styled(
            extra_tag,
            Style::default()
                .fg(STATUS_QUEUED)
                .add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

/// Compact 2-line layout for narrow / phone screens (< 70 cols):
/// Line 1: [sel] [↳] [glyph] ID [repo] ... [state] [+N]
/// Line 2:     title · live_status
fn render_work_item_compact(
    item: &WorkItem,
    live_status: &str,
    depth: usize,
    extra_deps: usize,
    selected: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let (glyph, status_color) = work_item_glyph_color(item);
    let tree_prefix = format_tree_prefix_compact(depth);
    let state_text = if extra_deps > 0 {
        format!("{} +{extra_deps}", item.state_badge())
    } else {
        item.state_badge().to_string()
    };
    let state_w = display_width(&state_text);

    // Line 1: ID + repo on left, state badge (+N) right-aligned
    let left_lead = format!("{tree_prefix}{glyph} {}", item.id);
    let repo_avail = width.saturating_sub(2 + display_width(&left_lead) + state_w + 3);
    let repo_part = if repo_avail >= 4 {
        format!(" [{}]", truncate(&item.repo_label, repo_avail.saturating_sub(2)))
    } else {
        String::new()
    };
    let left_full = format!("{left_lead}{repo_part}");
    let left_clipped = truncate(&left_full, width.saturating_sub(2 + state_w + 1));
    let gap1 = width.saturating_sub(2 + display_width(&left_clipped) + state_w);

    // Line 2: title · live_status
    let indent2 = format!("  {}", " ".repeat(display_width(&tree_prefix)));
    let avail2 = width.saturating_sub(display_width(&indent2) + 2);
    let title = item.display_title();
    let title_w = display_width(title);
    let status_w = display_width(live_status);
    let (title_part, status_part) = if live_status.is_empty() {
        (truncate(title, avail2), String::new())
    } else if title_w + 3 + status_w <= avail2 {
        (title.to_string(), live_status.to_string())
    } else {
        let s_budget = status_w.min((avail2 * 46) / 100).max(8);
        let t_budget = avail2.saturating_sub(s_budget + 3).max(6);
        (truncate(title, t_budget), truncate(live_status, s_budget))
    };

    if selected {
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        let rev_bold = rev.add_modifier(Modifier::BOLD);
        let accent = match status_color {
            Some(c) => Style::default().fg(c).add_modifier(Modifier::BOLD),
            None => bold_primary(),
        };
        let line1 = Line::from(vec![
            Span::styled("▎", accent),
            Span::styled(" ", rev),
            Span::styled(left_clipped, rev_bold),
            Span::styled(" ".repeat(gap1), rev),
            Span::styled(state_text, rev_bold),
        ]);
        let line2_body = if status_part.is_empty() {
            title_part
        } else {
            format!("{title_part} · {status_part}")
        };
        let line2_prefix = format!(" {indent2}");
        let used2 = 1 + display_width(&line2_prefix) + display_width(&line2_body);
        let pad2 = width.saturating_sub(used2);
        let line2 = Line::from(vec![
            Span::styled("▎", accent),
            Span::styled(line2_prefix, rev),
            Span::styled(line2_body, rev),
            Span::styled(" ".repeat(pad2), rev),
        ]);
        return vec![line1, line2];
    }

    let state_style = match status_color {
        Some(c) => Style::default().fg(c).add_modifier(Modifier::BOLD),
        None => muted(),
    };
    let line1 = Line::from(vec![
        Span::raw("  "),
        Span::styled(tree_prefix, muted()),
        Span::styled(format!("{glyph} "), state_style),
        Span::styled(item.id.clone(), bold_primary()),
        Span::styled(repo_part, muted()),
        Span::raw(" ".repeat(gap1)),
        Span::styled(state_text, state_style),
    ]);

    let title_style = if item.state == "done" {
        muted()
    } else {
        primary()
    };
    let status_style = if item.owner.is_some() && item.state != "done" {
        Style::default().fg(STATUS_PENDING)
    } else {
        muted()
    };
    let mut l2_spans = vec![
        Span::raw(format!("  {indent2}")),
        Span::styled(title_part, title_style),
    ];
    if !status_part.is_empty() {
        l2_spans.push(Span::styled(" · ", muted()));
        l2_spans.push(Span::styled(status_part, status_style));
    }
    vec![line1, Line::from(l2_spans)]
}

fn render_work_card_overlay(frame: &mut Frame, app: &mut App, area: Rect) {
    let (work_id, scroll_val) = match &app.overlay {
        Overlay::WorkCard { work_id, scroll } => (work_id.clone(), *scroll),
        _ => return,
    };

    frame.render_widget(Clear, area);
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;

    let Some(item) = app.snapshot.work_item_by_id(&work_id).cloned() else {
        let block = rounded_block(
            Line::from(Span::styled(format!(" 工作卡 · {work_id} "), bold_primary())),
            true,
        )
        .title_bottom(Line::from(Span::styled(" Esc 返回 ", muted())));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(" 工作项已不存在", muted()))),
            inner,
        );
        return;
    };

    let lines = build_work_card_lines(app, &item, inner_w.max(12));
    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_h);
    let clamped = scroll_val.min(max_scroll);
    if let Overlay::WorkCard { scroll, .. } = &mut app.overlay {
        *scroll = clamped;
    }

    let has_run = app.snapshot.find_run_for_work(&item).is_some();
    let jump_hint = if has_run { " · t跳run" } else { "" };
    let footer = if area.width >= COMPACT_BREAKPOINT {
        format!(
            " {}/{} · jk 滚动{jump_hint} · y 复制下一步 · Esc/Enter 返回 ",
            (clamped + 1).min(total.max(1)),
            total.max(1)
        )
    } else {
        format!(
            " {}/{} jk滚{jump_hint} y复 Esc返 ",
            (clamped + 1).min(total.max(1)),
            total.max(1)
        )
    };
    let title = format!(" 工作卡 · {} ", item.id);
    let block = rounded_block(
        Line::from(Span::styled(truncate(&title, inner_w), bold_primary())),
        true,
    )
    .title_bottom(Line::from(Span::styled(truncate(&footer, inner_w), muted())));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible: Vec<Line> = lines.into_iter().skip(clamped).take(inner_h).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn build_work_card_lines(app: &App, item: &WorkItem, width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let (_, status_color) = work_item_glyph_color(item);
    let badge_style = match status_color {
        Some(c) => Style::default()
            .fg(c)
            .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        None => Style::default().add_modifier(Modifier::REVERSED),
    };

    out.push(Line::from(vec![
        Span::styled(format!(" {} ", item.state_badge()), badge_style),
        Span::raw(" "),
        Span::styled(item.id.clone(), bold_primary()),
        Span::styled(format!(" · [{}]", item.repo_label), muted()),
    ]));
    push_wrapped(&mut out, item.display_title(), width, bold_primary());

    let mut meta_parts = vec![format!("状态: {}", item.state)];
    if !item.state_since.is_empty() {
        meta_parts.push(format!("自 {}", &item.state_since[..item.state_since.len().min(19)]));
    }
    if !item.repo.is_empty() {
        meta_parts.push(format!("仓库: {}", item.repo));
    }
    push_wrapped(&mut out, &meta_parts.join(" · "), width, muted());

    // Owner decision card fields
    if let Some(ow) = &item.owner {
        out.push(Line::raw(""));
        out.push(section_header(&format!("所有者事项（{}）", ow.kind.label())));
        if !ow.ask.is_empty() && ow.ask != item.title {
            push_indented_wrapped(&mut out, "  ", &format!("要你回答：{}", ow.ask), width, primary());
        }
        if !ow.recommendation.is_empty() {
            push_indented_wrapped(
                &mut out,
                "  ",
                &format!("主控建议：{}", ow.recommendation),
                width,
                Style::default()
                    .fg(STATUS_PENDING)
                    .add_modifier(Modifier::BOLD),
            );
        }
        if !ow.if_undecided.is_empty() {
            push_indented_wrapped(
                &mut out,
                "  ",
                &format!("不定会怎样：{}", ow.if_undecided),
                width,
                primary(),
            );
        }
        if !ow.cost.is_empty() {
            push_indented_wrapped(&mut out, "  ", &format!("成本：{}", ow.cost), width, muted());
        }
    }

    // Done condition
    if !item.done_when.is_empty() {
        out.push(Line::raw(""));
        out.push(section_header("完成条件"));
        push_indented_wrapped(&mut out, "  ", &item.done_when, width, primary());
    }

    // Brief / Owner markdown card
    if !item.brief.is_empty() {
        out.push(Line::raw(""));
        let hdr = if item.owner.is_some() {
            "卡片正文"
        } else {
            "任务说明"
        };
        out.push(section_header(hdr));
        push_indented_wrapped(&mut out, "  ", &item.brief, width, muted());
        if item.owner.is_some() {
            if let Some(res) = read_work_brief_markdown(item) {
                out.push(Line::raw(""));
                match res {
                    Ok(md) if !md.trim().is_empty() => {
                        out.extend(render_markdown(&md, "  ", width));
                    }
                    Ok(_) => {
                        push_indented_wrapped(&mut out, "  ", "（卡片文件为空）", width, muted());
                    }
                    Err(err) => {
                        push_indented_wrapped(
                            &mut out,
                            "  ",
                            &format!("（{err}）"),
                            width,
                            Style::default().fg(STATUS_PENDING),
                        );
                    }
                }
            }
        }
    }

    // Prerequisites with evaluated status
    if !item.wait_on.is_empty() || !item.wait_on_status.is_empty() {
        out.push(Line::raw(""));
        out.push(section_header("前置条件"));
        if !item.wait_on_status.is_empty() {
            for ws in &item.wait_on_status {
                let (mark, style) = if ws.satisfied {
                    ("✓", Style::default().fg(STATUS_RUNNING))
                } else {
                    ("⏳", Style::default().fg(STATUS_PENDING))
                };
                let ptr = if ws.pointer.is_empty() {
                    String::new()
                } else {
                    format!("（{}）", ws.pointer)
                };
                push_indented_wrapped(
                    &mut out,
                    "  ",
                    &format!("{mark} {} — {}{ptr}", ws.condition, ws.detail),
                    width,
                    style,
                );
            }
        } else {
            for raw in &item.wait_on {
                push_indented_wrapped(
                    &mut out,
                    "  ",
                    &format!("⏳ {raw}"),
                    width,
                    Style::default().fg(STATUS_PENDING),
                );
            }
        }
    }

    // Auto-dispatch
    if !item.auto_dispatch_prompt.is_empty() {
        out.push(Line::raw(""));
        out.push(section_header("自动派发"));
        let st = if item.auto_dispatch_run.is_empty() {
            "待触发".to_string()
        } else {
            format!("已派 {}", item.auto_dispatch_run)
        };
        push_indented_wrapped(
            &mut out,
            "  ",
            &format!("{st} · {}", item.auto_dispatch_prompt),
            width,
            muted(),
        );
    }

    // Linked runs (with live status + jump hint)
    if !item.runs.is_empty() {
        let has_live = app.snapshot.find_run_for_work(item).is_some();
        let hdr = if has_live {
            format!("关联运行（{}，按 t 跳到 run 视图）", item.runs.len())
        } else {
            format!("关联运行（{}）", item.runs.len())
        };
        out.push(Line::raw(""));
        out.push(section_header(&hdr));
        for rid in &item.runs {
            if let Some((_, _, r)) = app.snapshot.find_run_by_id(rid) {
                let summary = live_run_one_liner(r);
                let style = if matches!(r.group, GroupKind::Running | GroupKind::Queued) {
                    Style::default().fg(STATUS_RUNNING)
                } else if r.group == GroupKind::Pending {
                    Style::default().fg(STATUS_PENDING)
                } else {
                    muted()
                };
                push_indented_wrapped(
                    &mut out,
                    "  - ",
                    &format!("{rid} · {summary}"),
                    width,
                    style,
                );
            } else {
                push_indented_wrapped(
                    &mut out,
                    "  - ",
                    &format!("{rid}（已清理 · delegate show {rid}）"),
                    width,
                    muted(),
                );
            }
        }
    }

    // Artifacts
    if !item.artifacts.is_empty() {
        out.push(Line::raw(""));
        out.push(section_header("待接线产物"));
        for art in &item.artifacts {
            let note = if art.note.is_empty() {
                String::new()
            } else {
                format!(" — {}", art.note)
            };
            push_indented_wrapped(&mut out, "  - ", &format!("{}{note}", art.path), width, primary());
        }
    }

    // Followups
    if !item.followups.is_empty() {
        out.push(Line::raw(""));
        out.push(section_header(&format!(
            "细分待办（未完成 {} / 共 {}）",
            item.pending_followups(),
            item.followups.len()
        )));
        for f in &item.followups {
            let mark = if f.done { "[✓]" } else { "[ ]" };
            let style = if f.done {
                muted()
            } else {
                Style::default().fg(STATUS_PENDING)
            };
            let anc = if f.anchor.is_empty() {
                String::new()
            } else {
                format!(" — {}", f.anchor)
            };
            push_indented_wrapped(
                &mut out,
                "  ",
                &format!("{mark} [{}] {}{anc}", f.kind, f.text),
                width,
                style,
            );
        }
    }

    // Defer / Close / Drop evidence
    if item.state == "deferred" || !item.defer_reason.is_empty() {
        out.push(Line::raw(""));
        out.push(section_header("暂缓"));
        let until = if item.defer_until.is_empty() {
            "已到期"
        } else {
            item.defer_until.as_str()
        };
        push_indented_wrapped(
            &mut out,
            "  ",
            &format!("{}（至 {until}）", item.defer_reason),
            width,
            muted(),
        );
    }
    if item.state == "done" {
        out.push(Line::raw(""));
        if item.closed_kind == "dropped" {
            out.push(section_header("已否/放弃理由"));
            push_indented_wrapped(
                &mut out,
                "  ",
                &item.close_reason,
                width,
                Style::default().fg(STATUS_FAILED),
            );
        } else {
            out.push(section_header("关闭证据"));
            push_indented_wrapped(
                &mut out,
                "  ",
                &item.close_evidence,
                width,
                Style::default().fg(STATUS_RUNNING),
            );
        }
    }

    // Next step command
    let next_cmd = if item.next.is_empty() {
        format!("delegate work show {}", item.id)
    } else if item.next.starts_with("delegate ") {
        item.next.clone()
    } else {
        format!("delegate {}", item.next)
    };
    out.push(Line::raw(""));
    out.push(section_header("下一步（按 y 复制）"));
    push_indented_wrapped(
        &mut out,
        "  ",
        &next_cmd,
        width,
        Style::default().fg(STATUS_QUEUED),
    );

    out
}

// ---------------------------------------------------------------------------
// Agy Accounts View: list, active marker, detail pane, and in-place confirm modal
// ---------------------------------------------------------------------------

fn render_accounts_list_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::List && matches!(app.overlay, Overlay::None);
    let total = app.filtered_accounts.len();
    let running_n = app.snapshot.agy_accounts.running_agy.len();
    let title_text = if app.filter.is_empty() {
        if running_n > 0 {
            format!(" agy 账号 ({total}) · 在跑 {running_n} ")
        } else {
            format!(" agy 账号 ({total}) ")
        }
    } else {
        format!(" agy 账号 ({total}) · 过滤 ")
    };
    let title = Line::from(Span::styled(
        title_text,
        if focused { bold_primary() } else { muted() },
    ));
    let block = rounded_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if app.filtered_accounts.is_empty() {
        let msg = if !app.filter.is_empty() {
            format!("无匹配“{}”的 agy 账号", app.filter)
        } else {
            "暂无 agy 账号 · 运行 delegate agy account add 或 import-agy-swap --dry-run".to_string()
        };
        let lines = wrap_text(&msg, inner.width.saturating_sub(2) as usize)
            .into_iter()
            .map(|l| Line::from(Span::styled(format!(" {l}"), muted())))
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }

    let iw = inner.width as usize;
    let show_header = inner.height >= 4;
    let header_h: usize = if show_header { 1 } else { 0 };
    let view_h = (inner.height as usize).saturating_sub(header_h);

    if let Some(sel) = app.selected_account {
        if sel < app.accounts_scroll {
            app.accounts_scroll = sel;
        } else if view_h > 0 && sel >= app.accounts_scroll + view_h {
            app.accounts_scroll = sel + 1 - view_h;
        }
    }
    let max_scroll = app.filtered_accounts.len().saturating_sub(view_h);
    if app.accounts_scroll > max_scroll {
        app.accounts_scroll = max_scroll;
    }

    let id_w: usize = if iw >= 56 { 14 } else { 10 };
    let used_w: usize = if iw >= 52 { 16 } else { 0 };
    let status_w: usize = 6; // "★ 活跃" or "  备用"
    let email_w: usize = iw
        .saturating_sub(2 + status_w + 1 + id_w + 1 + if used_w > 0 { 1 + used_w } else { 0 })
        .max(8);

    let mut lines: Vec<Line> = Vec::with_capacity(inner.height as usize);
    if show_header {
        let mut hdr = String::new();
        hdr.push_str("  ");
        hdr.push_str(&pad_right("状态", status_w));
        hdr.push(' ');
        hdr.push_str(&pad_right("槽位", id_w));
        hdr.push(' ');
        hdr.push_str(&pad_right("邮箱", email_w));
        if used_w > 0 {
            hdr.push(' ');
            hdr.push_str(&pad_left("最后使用", used_w));
        }
        lines.push(Line::from(Span::styled(truncate(&hdr, iw), muted())));
    }

    for (pos, &acct_idx) in app
        .filtered_accounts
        .iter()
        .enumerate()
        .skip(app.accounts_scroll)
        .take(view_h)
    {
        let acct = &app.snapshot.agy_accounts.accounts[acct_idx];
        let selected = app.selected_account == Some(pos);
        let stripe = if selected {
            Span::styled("▎ ", Style::default().fg(STATUS_QUEUED).add_modifier(Modifier::BOLD))
        } else {
            Span::raw("  ")
        };
        let (badge_txt, badge_style) = if acct.active {
            (
                pad_right("★ 活跃", status_w),
                Style::default()
                    .fg(STATUS_RUNNING)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            (pad_right("  备用", status_w), muted())
        };
        let id_style = if selected || acct.active {
            bold_primary()
        } else {
            primary()
        };
        let email_text = if acct.email.is_empty() {
            "-"
        } else {
            acct.email.as_str()
        };
        let mut row_spans = vec![
            stripe,
            Span::styled(badge_txt, badge_style),
            Span::raw(" "),
            Span::styled(pad_right(&acct.id, id_w), id_style),
            Span::raw(" "),
            Span::styled(pad_right(email_text, email_w), if selected { primary() } else { muted() }),
        ];
        if used_w > 0 {
            let used_short = format_iso_compact(&acct.last_used_at);
            row_spans.push(Span::raw(" "));
            row_spans.push(Span::styled(pad_left(&used_short, used_w), muted()));
        }
        let line = if selected {
            Line::from(row_spans).style(Style::default().add_modifier(Modifier::BOLD))
        } else {
            Line::from(row_spans)
        };
        lines.push(line);
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn format_iso_compact(iso: &str) -> String {
    let t = iso.trim();
    if t.is_empty() {
        return "-".to_string();
    }
    if t.len() >= 16 && t.as_bytes()[10] == b'T' {
        return format!("{} {}", &t[5..10], &t[11..16]);
    }
    truncate(t, 16)
}

fn render_account_detail_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::Detail && matches!(app.overlay, Overlay::None);
    let title = match app.selected_account_item() {
        Some(a) => format!(" 账号详情 · {} ", a.id),
        None => " 账号详情 ".to_string(),
    };
    let block = rounded_block(
        Line::from(Span::styled(
            title,
            if focused { bold_primary() } else { muted() },
        )),
        focused,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let iw = inner.width as usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    let state = &app.snapshot.agy_accounts;

    if let Some(err) = &app.account_switch_error {
        out.push(Line::from(Span::styled(
            "▸ 上次切换被拒绝 / 失败（Esc 清除）",
            Style::default()
                .fg(STATUS_FAILED)
                .add_modifier(Modifier::BOLD),
        )));
        for line in err.lines() {
            push_indented_wrapped(
                &mut out,
                "  ",
                line,
                iw,
                Style::default().fg(STATUS_FAILED),
            );
        }
        out.push(Line::raw(""));
    }

    if let Some(acct) = app.selected_account_item() {
        let status_label = if acct.active {
            "★ 当前活跃"
        } else {
            "备用槽位"
        };
        let status_style = if acct.active {
            Style::default()
                .fg(STATUS_RUNNING)
                .add_modifier(Modifier::BOLD)
        } else {
            muted()
        };
        out.push(Line::from(vec![
            Span::styled("槽位: ", muted()),
            Span::styled(acct.id.clone(), bold_primary()),
            Span::raw("  "),
            Span::styled(status_label, status_style),
        ]));
        if !acct.email.is_empty() {
            out.push(Line::from(vec![
                Span::styled("邮箱: ", muted()),
                Span::styled(acct.email.clone(), primary()),
            ]));
        }
        if !acct.label.is_empty() {
            out.push(Line::from(vec![
                Span::styled("备注: ", muted()),
                Span::styled(acct.label.clone(), primary()),
            ]));
        }
        out.push(Line::from(vec![
            Span::styled("最后使用: ", muted()),
            Span::styled(
                if acct.last_used_at.is_empty() {
                    "-"
                } else {
                    acct.last_used_at.as_str()
                }
                .to_string(),
                primary(),
            ),
        ]));
        if !acct.last_synced_at.is_empty() {
            out.push(Line::from(vec![
                Span::styled("最近回存: ", muted()),
                Span::styled(acct.last_synced_at.clone(), muted()),
            ]));
        }
        if !acct.token_expires_at.is_empty() {
            out.push(Line::from(vec![
                Span::styled("本地令牌到期: ", muted()),
                Span::styled(acct.token_expires_at.clone(), muted()),
            ]));
        }
        out.push(Line::from(vec![
            Span::styled("凭据文件: ", muted()),
            Span::styled(
                format!("{} 份（仅本地 0600，不打印令牌）", acct.file_count),
                primary(),
            ),
        ]));
        if !acct.files.is_empty() {
            push_indented_wrapped(&mut out, "  ", &acct.files.join(", "), iw, muted());
        }
        if !acct.source.is_empty() {
            out.push(Line::from(vec![
                Span::styled("来源: ", muted()),
                Span::styled(acct.source.clone(), muted()),
            ]));
        }
        out.push(Line::raw(""));
    }

    // Live environment & running agy processes section
    out.push(section_header("本机状态"));
    if !state.live_email.is_empty() {
        let style = if state.live_drifted {
            Style::default()
                .fg(STATUS_PENDING)
                .add_modifier(Modifier::BOLD)
        } else {
            primary()
        };
        let drift_note = if state.live_drifted {
            "（与活跃槽位不一致）"
        } else {
            ""
        };
        push_indented_wrapped(
            &mut out,
            "  ",
            &format!("~/.gemini 当前账号: {}{drift_note}", state.live_email),
            iw,
            style,
        );
    }
    let run_cnt = state.running_agy.len();
    let proc_style = if run_cnt > 0 {
        Style::default()
            .fg(STATUS_PENDING)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(STATUS_RUNNING)
    };
    let proc_summary = if run_cnt > 0 {
        format!("在跑 agy 进程: {run_cnt} 个（有进程时拒绝切换，确保令牌不冲突）")
    } else {
        "在跑 agy 进程: 0 个（可安全手动切换）".to_string()
    };
    push_indented_wrapped(&mut out, "  ", &proc_summary, iw, proc_style);
    for p in state.running_agy.iter().take(6) {
        let line = match &p.run {
            Some(r) => format!("pid={} (run: {}) {}", p.pid, r, p.cmdline),
            None => format!("pid={} {}", p.pid, p.cmdline),
        };
        push_indented_wrapped(&mut out, "    ", &line, iw, muted());
    }
    if state.running_agy.len() > 6 {
        push_indented_wrapped(
            &mut out,
            "    ",
            &format!("… 另有 {} 个 agy 进程", state.running_agy.len() - 6),
            iw,
            muted(),
        );
    }

    out.push(Line::raw(""));
    out.push(section_header("操作说明"));
    push_indented_wrapped(
        &mut out,
        "  ",
        "按 Enter 或 u 弹出就地确认并切换选中账号；不做自动轮换，不调外部额度接口。",
        iw,
        muted(),
    );
    push_indented_wrapped(
        &mut out,
        "  ",
        "新增账号请在终端运行: delegate agy account add [<id>]",
        iw,
        muted(),
    );

    let view_h = inner.height as usize;
    let max_scroll = out.len().saturating_sub(view_h);
    if app.detail_scroll > max_scroll {
        app.detail_scroll = max_scroll;
    }
    let visible: Vec<Line> = out
        .into_iter()
        .skip(app.detail_scroll)
        .take(view_h)
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

#[allow(clippy::too_many_arguments)]
fn render_account_switch_modal(
    frame: &mut Frame,
    area: Rect,
    target_id: &str,
    target_email: &str,
    current_active: Option<&str>,
    is_already_active: bool,
    running_count: usize,
) {
    let w = area.width.saturating_sub(6).clamp(40, 76);
    let h = if running_count > 0 { 11u16 } else { 9u16 }.min(area.height.saturating_sub(2));
    let modal = centered_rect(area, w, h);
    frame.render_widget(Clear, modal);

    let title_style = if running_count > 0 {
        Style::default()
            .fg(STATUS_FAILED)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(STATUS_PENDING)
            .add_modifier(Modifier::BOLD)
    };
    let block = rounded_block(
        Line::from(Span::styled(" 确认切换 agy 账号 ", title_style)),
        true,
    )
    .title_bottom(Line::from(Span::styled(
        " Enter/y 确认切换 · Esc/n 取消 ",
        muted(),
    )));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let iw = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    let target_desc = if target_email.is_empty() {
        target_id.to_string()
    } else {
        format!("{target_id} ({target_email})")
    };
    lines.push(Line::from(vec![
        Span::styled("目标账号: ", muted()),
        Span::styled(truncate(&target_desc, iw.saturating_sub(10)), bold_primary()),
    ]));
    lines.push(Line::from(vec![
        Span::styled("当前活跃: ", muted()),
        Span::styled(current_active.unwrap_or("无").to_string(), primary()),
    ]));
    lines.push(Line::raw(""));

    let note = if is_already_active {
        "该账号已是当前活跃账号；确认后将把当前最新刷新令牌回存到槽位并保持激活。"
    } else {
        "确认后将先回存当前活跃账号的刷新令牌，再原子替换本地 agy 凭据（手动切换，不做自动轮换）。"
    };
    for l in wrap_text(note, iw) {
        lines.push(Line::from(Span::styled(l, primary())));
    }
    if running_count > 0 {
        lines.push(Line::raw(""));
        let warn = format!(
            "注意：当前检测到 {running_count} 个在跑 agy 进程，确认后命令将安全拒绝并列出进程。"
        );
        for l in wrap_text(&warn, iw) {
            lines.push(Line::from(Span::styled(
                l,
                Style::default()
                    .fg(STATUS_FAILED)
                    .add_modifier(Modifier::BOLD),
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
