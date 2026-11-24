use crate::app::{
    App, CollapseKind, FocusPane, ListRow, Overlay, OverviewBarMode, ViewTab, WorkFilterMode,
    WorkFoldKind, WorkRow,
};
use crate::data::read_work_brief_markdown;
use crate::markdown::render_markdown;
use crate::model::*;
use crate::width::{display_width, pad_left, pad_right, truncate, wrap_text};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::bar;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Bar, BarChart, BarGroup, Block, BorderType, Borders, Clear, Paragraph, Sparkline};
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
    } else if app.tab == ViewTab::Overview {
        render_overview_pane(frame, app, chunks[1]);
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
            ViewTab::Work | ViewTab::Overview => unreachable!(),
        }
    } else {
        let top_pct = if app.tab == ViewTab::Accounts {
            let need = (app.filtered_accounts.len() as u16).saturating_add(3).clamp(5, 9);
            Constraint::Length(need)
        } else {
            Constraint::Percentage(46)
        };
        let bot_constraint = if app.tab == ViewTab::Accounts {
            Constraint::Min(8)
        } else {
            Constraint::Percentage(54)
        };
        let body = Layout::default()
            .direction(Direction::Vertical)
            .constraints([top_pct, bot_constraint])
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
            ViewTab::Work | ViewTab::Overview => unreachable!(),
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

/// Build the unified tab strip used across all pages (including Work) so tab labels never shrink
/// or jump when switching views. Width pressure is handled by trimming right-side counts first.
fn build_top_bar_tabs(app: &App, width: u16) -> Vec<Span<'static>> {
    let wc = app.snapshot.work_counts();
    let work_open =
        wc.owner + wc.needs_action + wc.ready + wc.in_progress + wc.waiting + wc.deferred;
    let accounts_count = app.snapshot.agy_accounts.accounts.len();

    let active_style = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);
    let tab_overview_style = if app.tab == ViewTab::Overview {
        active_style
    } else {
        muted()
    };
    let tab_runs_style = if app.tab == ViewTab::Runs {
        active_style
    } else {
        muted()
    };
    let tab_events_style = if app.tab == ViewTab::Events {
        active_style
    } else {
        muted()
    };
    let tab_merges_style = if app.tab == ViewTab::Merges {
        active_style
    } else {
        muted()
    };
    let tab_candidates_style = if app.tab == ViewTab::Candidates {
        active_style
    } else {
        muted()
    };
    let tab_work_style = if app.tab == ViewTab::Work {
        active_style
    } else if wc.owner > 0 {
        Style::default()
            .fg(STATUS_PENDING)
            .add_modifier(Modifier::BOLD)
    } else {
        muted()
    };
    let tab_accounts_style = if app.tab == ViewTab::Accounts {
        active_style
    } else if app.snapshot.agy_accounts.live_drifted {
        Style::default()
            .fg(STATUS_PENDING)
            .add_modifier(Modifier::BOLD)
    } else {
        muted()
    };

    let show_tab_counts = width >= NARROW_BREAKPOINT;
    let mut spans = Vec::with_capacity(10);
    if width >= COMPACT_BREAKPOINT {
        spans.push(Span::styled(" delegate-tui ", active_style));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(" 0总览 ", tab_overview_style));
    spans.push(Span::styled(" 1任务 ", tab_runs_style));
    spans.push(Span::styled(
        if show_tab_counts {
            format!(" 2事件({}) ", app.snapshot.events.len())
        } else {
            " 2事件 ".to_string()
        },
        tab_events_style,
    ));
    spans.push(Span::styled(
        if show_tab_counts {
            format!(" 3合入({}) ", app.snapshot.merges.len())
        } else {
            " 3合入 ".to_string()
        },
        tab_merges_style,
    ));
    spans.push(Span::styled(
        if show_tab_counts {
            format!(" 4候选({}) ", app.snapshot.candidates.len())
        } else {
            " 4候选 ".to_string()
        },
        tab_candidates_style,
    ));
    spans.push(Span::styled(
        if show_tab_counts && work_open > 0 {
            format!(" 5工作({work_open}) ")
        } else {
            " 5工作 ".to_string()
        },
        tab_work_style,
    ));
    spans.push(Span::styled(
        if show_tab_counts && accounts_count > 0 {
            format!(" 6账号({accounts_count}) ")
        } else {
            " 6账号 ".to_string()
        },
        tab_accounts_style,
    ));
    spans
}

/// Append right-side status counts one by one, stopping before overflowing `max_w` so no item or
/// separator (` ·`) is ever clipped mid-way.
fn append_fitting_counts(
    spans: &mut Vec<Span<'static>>,
    max_w: usize,
    items: Vec<(String, Style)>,
    sep: &str,
    filter_badge: Option<Span<'static>>,
) {
    let mut used: usize = spans.iter().map(|s| display_width(&s.content)).sum();
    let badge_reserve = filter_badge
        .as_ref()
        .map(|b| display_width(&b.content) + 1)
        .unwrap_or(0);
    let budget = max_w.saturating_sub(badge_reserve);
    let sep_w = display_width(sep);
    let mut added_any = false;

    for (text, style) in items {
        let lead = if added_any { sep } else { "  " };
        let lead_w = if added_any { sep_w } else { 2 };
        let item_w = display_width(&text);
        if used + lead_w + item_w > budget {
            break;
        }
        if added_any {
            spans.push(Span::styled(lead.to_string(), muted()));
        } else {
            spans.push(Span::raw(lead.to_string()));
        }
        spans.push(Span::styled(text, style));
        used += lead_w + item_w;
        added_any = true;
    }

    if let Some(fb) = filter_badge {
        let fb_w = display_width(&fb.content);
        if used + 1 + fb_w <= max_w {
            spans.push(Span::raw(" "));
            spans.push(fb);
        }
    }
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

    let mut spans = build_top_bar_tabs(app, area.width);
    let compact_counts = area.width < NARROW_BREAKPOINT;

    let mut counts: Vec<(String, Style)> = vec![
        (
            if compact_counts {
                format!("待{pending}")
            } else {
                format!("待处理 {pending}")
            },
            if pending > 0 {
                Style::default()
                    .fg(STATUS_PENDING)
                    .add_modifier(Modifier::BOLD)
            } else {
                muted()
            },
        ),
        (
            if compact_counts {
                format!("运{running}")
            } else {
                format!("运行 {running}")
            },
            if running > 0 {
                Style::default()
                    .fg(STATUS_RUNNING)
                    .add_modifier(Modifier::BOLD)
            } else {
                muted()
            },
        ),
        (
            if compact_counts {
                format!("排{queued}")
            } else {
                format!("排队 {queued}")
            },
            if queued > 0 {
                Style::default()
                    .fg(STATUS_QUEUED)
                    .add_modifier(Modifier::BOLD)
            } else {
                muted()
            },
        ),
        (
            if compact_counts {
                format!("合{merged}")
            } else {
                format!("已合 {merged}")
            },
            muted(),
        ),
    ];
    if dag_pending > 0 {
        counts.push((
            format!("续派 {dag_pending}"),
            Style::default()
                .fg(STATUS_QUEUED)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if stopped > 0 && !compact_counts {
        counts.push((format!("已停 {stopped}"), muted()));
    }

    let filter_badge = if !app.filter.is_empty() {
        let max_f = if compact_counts { 8 } else { 16 };
        Some(Span::styled(
            format!(" 滤:{} ", truncate(&app.filter, max_f)),
            Style::default()
                .fg(STATUS_PENDING)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ))
    } else {
        None
    };

    let sep = if compact_counts { " " } else { " · " };
    append_fitting_counts(&mut spans, area.width as usize, counts, sep, filter_badge);
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
        let model_w = if width >= 54 { 7 } else { 6 };
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
                // Deterministic DAG follow-up queue header.
                let text = format!("▌ 续派队列 ({count})");
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
        CollapseKind::Collected => format!("{arrow} 已收取 {count} 条{ago}"),
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
            CollapseKind::Collected | CollapseKind::MergedUncleaned | CollapseKind::Stopped => {
                bold_primary()
            }
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

fn format_model_cell(run: &RunItem, max_cols: usize) -> String {
    if let Some(level) = run.thinking.as_deref().and_then(thinking_short_zh) {
        let suffix = format!("·{level}");
        let sw = display_width(&suffix);
        if max_cols > sw {
            return format!("{}{suffix}", truncate(&run.model, max_cols - sw));
        }
        return truncate(&format!("{}{suffix}", run.model), max_cols);
    }
    truncate(&run.model, max_cols)
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
        spans.push(Span::styled(
            pad_right(&format_model_cell(run, cols.model_w), cols.model_w),
            rev,
        ));
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
    let name_style = if matches!(
        run.group,
        GroupKind::Collected | GroupKind::MergedUncleaned | GroupKind::Stopped
    ) {
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
    spans.push(Span::styled(
        pad_right(&format_model_cell(run, cols.model_w), cols.model_w),
        muted(),
    ));
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
    out.push(Line::from(head_spans));
    if let Some(ref wid) = linked_wid {
        let w_title = app
            .snapshot
            .work_item_by_id(wid)
            .map(|w| format!(" {}", w.display_title()))
            .unwrap_or_default();
        push_wrapped(
            &mut out,
            &format!("工作项: [{wid}]{w_title}"),
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
    let model_display = match run.thinking.as_deref().and_then(thinking_short_zh) {
        Some(level) => format!("{}·{level}", run.model),
        None => run.model.clone(),
    };
    push_wrapped(
        &mut out,
        &format!("{}  ·  {}{tier_part}{mode_part}", run.repo_label, model_display),
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
    // `chain_round` is already shown in the detail pane title (`第N轮`).
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
                    format!("  … 共 {total_chars} 字"),
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

    // 3. 最近命令 (only when present; accept command is already shown in the 验收 line above)
    if let Some(last) = &run.last_command {
        out.push(Line::raw(""));
        out.push(section_header("最近命令"));
        // Idle duration is already shown in the timing metadata line above.
        push_indented_wrapped(&mut out, "  ", last, width, primary());
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
                format!("  … 另有 {} 个目录", run.dirs_more),
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
        // Bottom bar lists only keys that actually act on the current page (tabs 0-6 are in the top bar).
        let keys = match app.tab {
            ViewTab::Overview => {
                if area.width >= 100 {
                    " v 切图  Q 刷新额度  r 刷新  ? 帮助  q 退出"
                } else {
                    " jk滚 Q刷额度 r刷 ?助 q退"
                }
            }
            ViewTab::Runs => {
                if area.width >= 100 {
                    " jk 选  Tab 切栏  Enter 结论  d diff  / 过滤  a/c/s/p 命令  r 刷新  ? 帮助  q 退出"
                } else {
                    " jk选 Tab栏 Enter结论 d diff /滤 a/c/s/p令 r刷 ?助 q退"
                }
            }
            ViewTab::Events => {
                if area.width >= 100 {
                    " jk 选  f 跟随  Tab 切栏  / 过滤  r 刷新  ? 帮助  q 退出"
                } else {
                    " jk选 f跟随 Tab栏 /滤 r刷 ?助 q退"
                }
            }
            ViewTab::Merges => {
                if area.width >= 100 {
                    " jk 选  a/Enter 合回  Tab 切栏  / 过滤  r 刷新  ? 帮助  q 退出"
                } else {
                    " jk选 a/Enter合回 Tab栏 /滤 r刷 ?助 q退"
                }
            }
            ViewTab::Candidates => {
                if area.width >= 100 {
                    " jk 选  p/Enter 挑草稿  Tab 切栏  / 过滤  r 刷新  ? 帮助  q 退出"
                } else {
                    " jk选 p/Enter挑草稿 Tab栏 /滤 r刷 ?助 q退"
                }
            }
            ViewTab::Work => {
                if area.width >= 100 {
                    " jk 选  Enter 卡片/折叠  t 跳run  o 等你  x 已否  / 过滤  y 复制  r 刷新  ? 帮助  q 退出"
                } else if area.width >= COMPACT_BREAKPOINT {
                    " jk选 Enter卡 t跳run o等你 x已否 /滤 y复 r刷 ?助 q退"
                } else {
                    " ↑↓选 Enter卡 t跳run o等你 x已否 w任务 q退"
                }
            }
            ViewTab::Accounts => {
                if area.width >= 100 {
                    " jk 选  Enter/u 切号  i 全称  Q 刷新额度  y 复制  Tab 切栏  / 过滤  r 刷新  ? 帮助  q 退出"
                } else {
                    " jk选 Enter/u切号 i全称 Q刷额度 y复 Tab栏 /滤 r刷 ?助 q退"
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
        ("↑↓ / j k", "选择 / 滚动"),
        ("0-6 / O w A e m b", "切页"),
        ("Tab", "切栏"),
        ("Enter", "详情 / 折叠 / 确认"),
        ("/", "过滤（Esc 清除）"),
        ("r / Q", "刷新数据 / 刷新额度"),
        ("v", "总览切图"),
        ("u", "切号"),
        ("d", "diff 统计"),
        ("a / c / s / p", "apply / clean / stop / reply（候选 pick）"),
        ("y", "复制命令"),
        ("f", "事件跟随"),
        ("t / o / x", "工作表：跳 run / 等你 / 已否"),
        ("q / Ctrl-C", "关闭 / 退出"),
    ];
    let iw = inner.width as usize;
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, desc)| {
            Line::from(vec![
                Span::styled(pad_right(k, 18), bold_primary()),
                Span::raw(" "),
                Span::styled(truncate(desc, iw.saturating_sub(19)), muted()),
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
        GroupKind::Collected | GroupKind::MergedUncleaned | GroupKind::Stopped => None,
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
        GroupKind::Collected => ("✓", None),
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
    out.push(section_header("上游依赖"));
    for dep in &q.after {
        push_indented_wrapped(&mut out, "  - ", dep, inner_w, primary());
    }
    if let Some(rid) = &q.run_id {
        out.push(Line::raw(""));
        out.push(section_header("已派发任务"));
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
    // Event seq is shown in the detail pane title (`事件 #N`); body shows kind badge + kind once.
    out.push(Line::from(vec![
        Span::styled(format!(" {} ", ev.kind_badge()), badge_style),
        Span::raw(" "),
        Span::styled(ev.kind.clone(), bold_primary()),
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
        format!(" 事件 #{} ", ev.seq),
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
    // Merge run ID is shown in the detail pane title; body starts with status badge + repo/baseline.
    let base_short = &m.base_head[..m.base_head.len().min(7)];
    let head_short = &m.head[..m.head.len().min(7)];
    out.push(Line::from(vec![
        Span::styled(
            format!(" {badge_text} "),
            Style::default()
                .fg(badge_color)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{} · {base_short} → {head_short}", m.repo_label),
            bold_primary(),
        ),
    ]));
    push_wrapped(
        &mut out,
        &format!(
            "合入 {} / 跳过 {} / 可自主合入 {}",
            m.merged_count, m.skipped_count, m.auto_merge_count
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
    // Keybind `a` is already shown in the Merges bottom bar.
    out.push(section_header("下一步"));
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
        format!(" {} ", truncate(&m.merge_run, inner_w.saturating_sub(4))),
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
    // Candidate ID is shown in the detail pane title; body starts with readiness badge + title.
    out.push(Line::from(vec![
        Span::styled(
            format!(" {badge} "),
            Style::default()
                .fg(badge_color)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(c.title.clone(), bold_primary()),
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
    // `radar pick` only creates a brief draft and does not dispatch automatically.
    // Keybind `p` is already shown in the Candidates bottom bar.
    out.push(section_header("生成任务草稿"));
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
        format!(" {} ", c.id),
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
        if c.stale > 0 {
            line2_spans.push(Span::styled(sep, muted()));
            line2_spans.push(count_span("老化", c.stale, Some(STATUS_PENDING)));
        }
        if let Some(fb) = filter_badge {
            line2_spans.push(Span::raw(" "));
            line2_spans.push(fb);
        }
        frame.render_widget(Paragraph::new(vec![line1, Line::from(line2_spans)]), area);
        return;
    }

    let mut spans = build_top_bar_tabs(app, area.width);
    let count_item = |label: &str, n: usize, color: Option<Color>| -> (String, Style) {
        let style = if n > 0 {
            match color {
                Some(col) => Style::default().fg(col).add_modifier(Modifier::BOLD),
                None => bold_primary(),
            }
        } else {
            muted()
        };
        (format!("{label} {n}"), style)
    };
    let mut items = vec![
        count_item("等你", c.owner, Some(STATUS_FAILED)),
        count_item("需处理", c.needs_action, Some(STATUS_PENDING)),
        count_item("就绪", c.ready, Some(STATUS_QUEUED)),
        count_item("推进中", c.in_progress, Some(STATUS_RUNNING)),
        count_item("等待", c.waiting, None),
        count_item("暂缓", c.deferred, None),
        count_item("今天完成", c.done_today, Some(STATUS_RUNNING)),
    ];
    if c.stale > 0 {
        items.push(count_item("老化", c.stale, Some(STATUS_PENDING)));
    }
    let sep = if area.width >= 100 { " · " } else { "·" };
    append_fitting_counts(&mut spans, area.width as usize, items, sep, filter_badge);

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
                let stale_info = app.snapshot.work_item_stale_info(item);
                if compact {
                    render_work_item_compact(
                        item,
                        &live,
                        stale_info,
                        *depth,
                        *extra_deps,
                        selected,
                        inner_w,
                    )
                } else {
                    vec![render_work_item_wide(
                        item,
                        &live,
                        stale_info,
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
    // Owner items are ordered as 动手 -> 点头 -> 看卡 -> 方向 (see app::rebuild_work_rows).
    let label = format!(" ▾ {} ({count})", bucket.label());
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

fn work_item_tail_tags(item: &WorkItem, stale_info: Option<(u64, bool)>) -> Vec<(String, Color)> {
    let mut tags = Vec::new();
    if let Some((hours, is_2x)) = stale_info {
        let color = if is_2x { STATUS_FAILED } else { STATUS_PENDING };
        tags.push((format!(" 停 {hours}h"), color));
    }
    if item.is_missing_brief() {
        tags.push((" 缺说明".to_string(), STATUS_PENDING));
    }
    if item.is_missing_owner_advice() {
        tags.push((" 缺建议".to_string(), STATUS_PENDING));
    }
    tags
}

/// When the row tail already displays `停 Nh`, drop a trailing ` {N}[smhd]` age from
/// `可派发 {age}` / `待闭环 {age}` so the same duration is never shown twice on one row.
fn dedup_stale_live_status(live_status: &str, has_stale_tag: bool) -> &str {
    if !has_stale_tag {
        return live_status;
    }
    if let Some((head, tail)) = live_status.rsplit_once(' ') {
        let is_age_token = tail.len() >= 2
            && matches!(tail.as_bytes().last(), Some(b's' | b'm' | b'h' | b'd'))
            && tail[..tail.len() - 1].bytes().all(|b| b.is_ascii_digit());
        if is_age_token && (head.ends_with("可派发") || head.ends_with("待闭环")) {
            return head;
        }
    }
    live_status
}

#[allow(clippy::too_many_arguments)]
fn render_work_item_wide(
    item: &WorkItem,
    live_status: &str,
    stale_info: Option<(u64, bool)>,
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
    let tail_tags = work_item_tail_tags(item, stale_info);
    let live_status = dedup_stale_live_status(live_status, stale_info.is_some());
    let tail_w: usize = tail_tags.iter().map(|(t, _)| display_width(t)).sum();
    let extra_w = display_width(&extra_tag);
    let status_avail = cols.status_w.saturating_sub(extra_w + tail_w);
    let status_core = truncate(live_status, status_avail);
    let tail_plain: String = tail_tags.iter().map(|(t, _)| t.as_str()).collect();
    let status_full = format!("{status_core}{extra_tag}{tail_plain}");

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
    for (tag, col) in tail_tags {
        spans.push(Span::styled(tag, Style::default().fg(col)));
    }
    Line::from(spans)
}

/// Compact 2-line layout for narrow / phone screens (< 70 cols):
/// Line 1: [sel] [↳] [glyph] ID [repo] ... [state] [+N]
/// Line 2:     title · live_status [停 Nh] [缺说明]
fn render_work_item_compact(
    item: &WorkItem,
    live_status: &str,
    stale_info: Option<(u64, bool)>,
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

    // Line 2: title · live_status [tail_tags]
    let tail_tags = work_item_tail_tags(item, stale_info);
    let live_status = dedup_stale_live_status(live_status, stale_info.is_some());
    let tail_w: usize = tail_tags.iter().map(|(t, _)| display_width(t)).sum();
    let indent2 = format!("  {}", " ".repeat(display_width(&tree_prefix)));
    let avail2 = width.saturating_sub(display_width(&indent2) + 2 + tail_w);
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
        let tail_plain: String = tail_tags.iter().map(|(t, _)| t.as_str()).collect();
        let line2_body = if status_part.is_empty() {
            format!("{title_part}{tail_plain}")
        } else {
            format!("{title_part} · {status_part}{tail_plain}")
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
    for (tag, col) in tail_tags {
        l2_spans.push(Span::styled(tag, Style::default().fg(col)));
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
            " {}/{} · jk 滚动{jump_hint} · y 复制 · Esc/Enter 返回 ",
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

    // Work item ID is already in the card overlay title (`工作卡 · <ID>`).
    out.push(Line::from(vec![
        Span::styled(format!(" {} ", item.state_badge()), badge_style),
        Span::raw(" "),
        Span::styled(format!("[{}]", item.repo_label), muted()),
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
        let mut waiters: Vec<&WorkItem> = app
            .snapshot
            .work
            .iter()
            .filter(|w| {
                w.owner.is_none()
                    && w.state != "done"
                    && w.wait_on.iter().any(|c| {
                        c.strip_prefix("owner:").is_some_and(|raw| {
                            let norm = crate::app::normalize_wid_for_match(raw);
                            norm == item.id || raw.trim().eq_ignore_ascii_case(&item.id)
                        })
                    })
            })
            .collect();
        waiters.sort_by(|a, b| a.id.cmp(&b.id));
        if !waiters.is_empty() {
            out.push(Line::raw(""));
            out.push(section_header("在等它的工作"));
            for w in waiters {
                let mut parts = vec![format!("[{}] {}", w.id, w.title)];
                if !w.done_when.trim().is_empty() {
                    parts.push(format!("完成条件：{}", w.done_when.trim()));
                }
                if !w.brief.trim().is_empty() {
                    parts.push(format!("说明：{}", w.brief.trim()));
                }
                push_indented_wrapped(&mut out, "  - ", &parts.join(" · "), width, primary());
            }
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

    // Linked runs (jump key `t` is shown in the overlay bottom border when a live run exists)
    if !item.runs.is_empty() {
        let hdr = format!("关联运行（{}）", item.runs.len());
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
    // Copy key `y` is already shown in the overlay bottom border.
    out.push(section_header("下一步"));
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
// Overview Dashboard (`0总览`): Quota Gauges, 24h Sparklines, BarChart, 7d Trends
//
// Design notes (not shown in UI):
// - Quota data is collected via `agy -p /usage` and cached locally; refreshed every
//   10 minutes while the TUI is open.
// - All quota figures and depletion estimates are advisory only; the TUI never
//   auto-switches accounts.
// - Real quota takes precedence over manual `note` snapshots when both exist.
// ---------------------------------------------------------------------------

fn quota_bar_color(pct_x100: u32) -> Color {
    if pct_x100 < 2000 {
        STATUS_FAILED
    } else if pct_x100 < 5000 {
        STATUS_PENDING
    } else {
        STATUS_RUNNING
    }
}

fn format_quota_updated(age_secs: u64) -> String {
    if age_secs < 45 {
        "刚刚更新".to_string()
    } else if age_secs < 3600 {
        format!("{} 分钟前更新", (age_secs / 60).max(1))
    } else if age_secs < 86400 {
        format!("{} 小时前更新", age_secs / 3600)
    } else {
        format!("{} 天前更新", age_secs / 86400)
    }
}

fn format_reset_countdown_zh(secs: Option<u64>) -> String {
    match secs {
        None => String::new(),
        Some(0) => "已重置".to_string(),
        Some(s) if s < 60 => format!("{s} 秒后重置"),
        Some(s) if s < 3600 => format!("{} 分钟后重置", s / 60),
        Some(s) if s < 48 * 3600 => {
            let h = s / 3600;
            let m = (s % 3600) / 60;
            if m == 0 {
                format!("{h} 小时后重置")
            } else {
                format!("{h} 小时 {m} 分钟后重置")
            }
        }
        Some(s) => {
            let d = s / 86400;
            let h = (s % 86400) / 3600;
            if h == 0 {
                format!("{d} 天后重置")
            } else {
                format!("{d} 天 {h} 小时后重置")
            }
        }
    }
}

fn format_minutes_zh(mins: u64) -> String {
    if mins == 0 {
        "不到 1 分钟".to_string()
    } else if mins < 60 {
        format!("{mins} 分钟")
    } else if mins < 24 * 60 {
        let h = mins / 60;
        let m = mins % 60;
        if m == 0 {
            format!("{h} 小时")
        } else {
            format!("{h} 小时 {m} 分钟")
        }
    } else {
        let d = mins / (24 * 60);
        let h = (mins % (24 * 60)) / 60;
        if h == 0 {
            format!("{d} 天")
        } else {
            format!("{d} 天 {h} 小时")
        }
    }
}

fn short_pool_name(pool: &str) -> String {
    let low = pool.to_ascii_lowercase();
    if low.contains("gemini") {
        "Gemini".to_string()
    } else if low.contains("claude") && low.contains("gpt") {
        "Claude/GPT".to_string()
    } else if low.contains("claude") {
        "Claude".to_string()
    } else if low.contains("gpt") {
        "GPT".to_string()
    } else {
        truncate(pool, 12)
    }
}

fn window_display_name(e: &QuotaPoolEntry) -> String {
    match e.window.as_str() {
        "weekly" => "本周".to_string(),
        "fiveHour" => "5 小时".to_string(),
        _ => truncate(&e.window_label, 8),
    }
}

fn format_pct_clean(pct_x100: u32) -> String {
    if pct_x100.is_multiple_of(100) {
        format!("{}%", pct_x100 / 100)
    } else if pct_x100.is_multiple_of(10) {
        format!("{:.1}%", pct_x100 as f64 / 100.0)
    } else {
        format!("{:.2}%", pct_x100 as f64 / 100.0)
    }
}

#[derive(Clone, Copy)]
enum WindowEstimateOutcome {
    WontRunOut,
    RunsOutIn { mins: u64, before_reset: bool },
}

fn find_reset_seconds(
    acct: &AgyAccountItem,
    window_key: &str,
    est_pool: Option<&str>,
) -> Option<u64> {
    let q = acct.quota.as_ref().filter(|q| q.ok)?;
    if let Some(pool) = est_pool {
        if let Some(secs) = q
            .entries
            .iter()
            .find(|e| e.window == window_key && e.pool == pool)
            .and_then(|e| e.reset_in_seconds)
        {
            return Some(secs);
        }
    }
    q.entries
        .iter()
        .filter(|e| e.window == window_key)
        .filter_map(|e| e.reset_in_seconds)
        .max()
}

fn account_has_multiple_pools(acct: &AgyAccountItem) -> bool {
    let Some(q) = acct.quota.as_ref().filter(|q| q.ok) else {
        return false;
    };
    let mut first: Option<String> = None;
    for e in &q.entries {
        let p = short_pool_name(&e.pool);
        match &first {
            None => first = Some(p),
            Some(f) if f != &p => return true,
            _ => {}
        }
    }
    false
}

fn pace_prefix(burn_rate_source: &str) -> String {
    match burn_rate_source {
        "24h" => "按最近 24 小时的平均用量".to_string(),
        "5h" => "按最近 5 小时的平均用量".to_string(),
        "7d" => "按最近 7 天的平均用量".to_string(),
        other => {
            if let Some(hours) = other
                .strip_prefix("since:")
                .and_then(|s| s.strip_suffix('h'))
                .and_then(|n| n.parse::<u64>().ok())
                .filter(|&n| n > 0)
            {
                format!("按最近 {hours} 小时的平均用量")
            } else {
                "按当前用量".to_string()
            }
        }
    }
}

fn evaluate_window_outcome(
    acct: &AgyAccountItem,
    window_key: &str,
    est: &QuotaWindowEstimate,
) -> Option<WindowEstimateOutcome> {
    let mins = est.estimated_remaining_minutes?;
    if let Some(reset_secs) = find_reset_seconds(acct, window_key, est.pool.as_deref()) {
        if mins.saturating_mul(60) >= reset_secs {
            return Some(WindowEstimateOutcome::WontRunOut);
        }
        return Some(WindowEstimateOutcome::RunsOutIn {
            mins,
            before_reset: true,
        });
    }
    Some(WindowEstimateOutcome::RunsOutIn {
        mins,
        before_reset: false,
    })
}

fn format_single_window_conclusion(
    est: &QuotaWindowEstimate,
    outcome: WindowEstimateOutcome,
    win_label: &str,
    multi_pool: bool,
) -> (String, bool) {
    let pace = pace_prefix(&est.burn_rate_source);
    let subject = if multi_pool {
        match est.pool.as_deref() {
            Some(p) if !p.is_empty() => format!("{} {win_label}", short_pool_name(p)),
            _ => win_label.to_string(),
        }
    } else {
        win_label.to_string()
    };
    match outcome {
        WindowEstimateOutcome::WontRunOut => {
            (format!("{pace}，{subject}额度在重置前用不完"), false)
        }
        WindowEstimateOutcome::RunsOutIn { mins, before_reset } => (
            format!("{pace}，{subject}额度约 {}后用完", format_minutes_zh(mins)),
            before_reset,
        ),
    }
}

/// Build plain-language depletion conclusion line(s) for an account.
/// Intermediate `tokens_per_pct` metrics are intentionally omitted from the UI.
/// Returns `(text, is_warning)` where `is_warning` is true when depletion is projected before reset.
fn format_quota_estimate_lines(
    acct: &AgyAccountItem,
    single_sentence: bool,
) -> Vec<(String, bool)> {
    let Some(est) = &acct.quota_estimate else {
        return Vec::new();
    };
    let multi_pool = account_has_multiple_pools(acct);
    let fh = est
        .five_hour
        .as_ref()
        .and_then(|e| evaluate_window_outcome(acct, "fiveHour", e).map(|o| (e, o)));
    let wk = est
        .weekly
        .as_ref()
        .and_then(|e| evaluate_window_outcome(acct, "weekly", e).map(|o| (e, o)));

    match (fh, wk) {
        (
            Some((fe, WindowEstimateOutcome::WontRunOut)),
            Some((we, WindowEstimateOutcome::WontRunOut)),
        ) => {
            if fe.burn_rate_source == we.burn_rate_source || single_sentence {
                let pace = pace_prefix(&fe.burn_rate_source);
                vec![(format!("{pace}，5 小时与本周额度在重置前用不完"), false)]
            } else {
                vec![
                    format_single_window_conclusion(fe, WindowEstimateOutcome::WontRunOut, "5 小时", multi_pool),
                    format_single_window_conclusion(we, WindowEstimateOutcome::WontRunOut, "本周", multi_pool),
                ]
            }
        }
        (
            Some((fe, fo @ WindowEstimateOutcome::RunsOutIn { .. })),
            Some((we, wo @ WindowEstimateOutcome::WontRunOut)),
        ) => {
            let line_fh = format_single_window_conclusion(fe, fo, "5 小时", multi_pool);
            if single_sentence {
                vec![line_fh]
            } else {
                vec![
                    line_fh,
                    format_single_window_conclusion(we, wo, "本周", multi_pool),
                ]
            }
        }
        (
            Some((fe, fo @ WindowEstimateOutcome::WontRunOut)),
            Some((we, wo @ WindowEstimateOutcome::RunsOutIn { .. })),
        ) => {
            let line_wk = format_single_window_conclusion(we, wo, "本周", multi_pool);
            if single_sentence {
                vec![line_wk]
            } else {
                vec![
                    format_single_window_conclusion(fe, fo, "5 小时", multi_pool),
                    line_wk,
                ]
            }
        }
        (
            Some((fe, fo @ WindowEstimateOutcome::RunsOutIn { mins: fm, .. })),
            Some((we, wo @ WindowEstimateOutcome::RunsOutIn { mins: wm, .. })),
        ) => {
            let line_fh = format_single_window_conclusion(fe, fo, "5 小时", multi_pool);
            let line_wk = format_single_window_conclusion(we, wo, "本周", multi_pool);
            if single_sentence {
                if fm <= wm {
                    vec![line_fh]
                } else {
                    vec![line_wk]
                }
            } else {
                vec![line_fh, line_wk]
            }
        }
        (Some((fe, fo)), None) => {
            vec![format_single_window_conclusion(fe, fo, "5 小时", multi_pool)]
        }
        (None, Some((we, wo))) => {
            vec![format_single_window_conclusion(we, wo, "本周", multi_pool)]
        }
        (None, None) => Vec::new(),
    }
}

fn format_sparkline_axis(axis_w: usize) -> String {
    let left = "24 小时前";
    let mid = "12 小时前";
    let right = "现在";
    let lw = display_width(left);
    let mw = display_width(mid);
    let rw = display_width(right);
    if axis_w >= lw + mw + rw + 6 {
        let gap1 = (axis_w / 2).saturating_sub(lw + mw / 2);
        let gap2 = axis_w.saturating_sub(lw + gap1 + mw + rw);
        format!("{left}{}{mid}{}{right}", " ".repeat(gap1), " ".repeat(gap2))
    } else {
        "24 小时前 .. 现在".to_string()
    }
}

fn inline_spark_text(values: &[u64], buckets: usize) -> String {
    if values.is_empty() || buckets == 0 {
        return "-".to_string();
    }
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let step = (values.len() / buckets).max(1);
    let mut sampled = Vec::with_capacity(buckets);
    for chunk in values.chunks(step) {
        sampled.push(chunk.iter().copied().sum::<u64>());
    }
    let max_v = sampled.iter().copied().max().unwrap_or(0);
    if max_v == 0 {
        return "▁".repeat(sampled.len());
    }
    sampled
        .into_iter()
        .map(|v| {
            let idx = ((v as f64 / max_v as f64) * 7.0).round() as usize;
            BLOCKS[idx.min(7)]
        })
        .collect()
}

fn render_quota_bar_spans(ratio: f64, bar_w: usize, color: Color) -> Vec<Span<'static>> {
    if bar_w == 0 {
        return Vec::new();
    }
    const PARTIALS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (ratio.clamp(0.0, 1.0) * (bar_w as f64 * 8.0)).round() as usize;
    let full = (eighths / 8).min(bar_w);
    let rem = if full < bar_w { eighths % 8 } else { 0 };
    let empty = bar_w.saturating_sub(full + usize::from(rem > 0));

    let fill_style = Style::default().fg(color);
    let mut filled = "█".repeat(full);
    if rem > 0 {
        filled.push(PARTIALS[rem]);
    }
    let mut spans = Vec::with_capacity(2);
    if !filled.is_empty() {
        spans.push(Span::styled(filled, fill_style));
    }
    if empty > 0 {
        spans.push(Span::styled("░".repeat(empty), muted()));
    }
    spans
}

fn render_quota_gauge_row(frame: &mut Frame, area: Rect, label: &str, entry: &QuotaPoolEntry) {
    if area.width < 16 || area.height == 0 {
        return;
    }
    let total_w = area.width as usize;
    let label_w: usize = if total_w >= 56 {
        22
    } else if total_w >= 48 {
        21
    } else {
        16
    };
    let avail = total_w.saturating_sub(label_w);
    let ratio = (entry.remaining_pct_x100 as f64 / 10_000.0).clamp(0.0, 1.0);
    let pct_txt = format!("剩余 {}", format_pct_clean(entry.remaining_pct_x100));
    let reset_txt = format_reset_countdown_zh(entry.reset_in_seconds);
    let gauge_lbl = if reset_txt.is_empty() {
        pct_txt
    } else {
        format!("{pct_txt} · {reset_txt}")
    };

    let text_w = display_width(&gauge_lbl);
    let max_bar_w = avail.saturating_sub(text_w + 1);
    // Keep bars aligned across rows in the same panel when there is room for typical text (~32 cols).
    let preferred_bar_w = if avail >= 37 {
        avail.saturating_sub(33).clamp(4, 16)
    } else {
        max_bar_w.min(12)
    };
    let bar_w = preferred_bar_w.min(max_bar_w);

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(5);
    spans.push(Span::styled(pad_right(label, label_w), primary()));
    if bar_w > 0 {
        spans.extend(render_quota_bar_spans(
            ratio,
            bar_w,
            quota_bar_color(entry.remaining_pct_x100),
        ));
        spans.push(Span::raw(" "));
        let text_budget = avail.saturating_sub(bar_w + 1);
        spans.push(Span::styled(truncate(&gauge_lbl, text_budget), bold_primary()));
    } else {
        spans.push(Span::styled(
            truncate(&gauge_lbl, avail),
            Style::default().fg(quota_bar_color(entry.remaining_pct_x100)),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_overview_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.width < NARROW_BREAKPOINT {
        render_overview_narrow(frame, app, area);
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(54), Constraint::Percentage(46)])
        .split(area);
    let top_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(rows[0]);
    let bot_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(rows[1]);

    render_overview_quota_panel(frame, app, top_cols[0]);
    render_overview_sparkline_panel(frame, app, top_cols[1]);
    render_overview_barchart_panel(frame, app, bot_cols[0]);
    render_overview_trend_panel(frame, app, bot_cols[1]);
}

fn render_overview_quota_panel(frame: &mut Frame, app: &App, area: Rect) {
    let state = &app.snapshot.agy_accounts;
    let age_badge = if app.quota_refreshing {
        " · 刷新中…".to_string()
    } else if let Some(age) = state.quota_age_seconds {
        format!(" · {}", format_quota_updated(age))
    } else if let Some(age) = state
        .accounts
        .iter()
        .filter_map(|a| a.quota.as_ref().map(|q| q.age_seconds))
        .min()
    {
        format!(" · {}", format_quota_updated(age))
    } else {
        " · 未更新".to_string()
    };
    let block = rounded_block(
        Line::from(Span::styled(
            format!(" 账号额度{age_badge} "),
            bold_primary(),
        )),
        true,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if state.accounts.is_empty() {
        // The `delegate agy account add` command hint is shown in the Accounts page empty state.
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("暂无 agy 账号", muted()))),
            inner,
        );
        return;
    }

    let mut y = inner.y;
    let max_y = inner.y.saturating_add(inner.height);
    let iw = inner.width as usize;

    for (idx, acct) in state.accounts.iter().enumerate() {
        if y >= max_y {
            break;
        }
        if idx > 0 && y + 1 < max_y {
            y += 1;
        }
        let mark = if acct.active { "★" } else { "·" };
        let mark_style = if acct.active {
            Style::default()
                .fg(STATUS_RUNNING)
                .add_modifier(Modifier::BOLD)
        } else {
            muted()
        };
        let disp_name = acct.display_name(app.is_account_revealed(&acct.id));
        let hdr_line = Line::from(vec![
            Span::styled(format!("{mark} "), mark_style),
            Span::styled(truncate(&disp_name, iw.saturating_sub(2)), bold_primary()),
        ]);
        frame.render_widget(
            Paragraph::new(hdr_line),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;

        if let Some(q) = &acct.quota {
            if !q.ok {
                if y < max_y {
                    let err_msg = q.error.as_deref().unwrap_or("刷新失败");
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            truncate(&format!("  ✗ {err_msg}"), iw),
                            Style::default().fg(STATUS_FAILED),
                        ))),
                        Rect::new(inner.x, y, inner.width, 1),
                    );
                    y += 1;
                }
            } else if q.entries.is_empty() {
                if y < max_y {
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled("  暂无额度数据", muted()))),
                        Rect::new(inner.x, y, inner.width, 1),
                    );
                    y += 1;
                }
            } else {
                for entry in &q.entries {
                    if y >= max_y {
                        break;
                    }
                    let lbl = format!(
                        "  {} · {}",
                        short_pool_name(&entry.pool),
                        window_display_name(entry)
                    );
                    render_quota_gauge_row(
                        frame,
                        Rect::new(inner.x, y, inner.width, 1),
                        &lbl,
                        entry,
                    );
                    y += 1;
                }
                // Unparsed raw lines from `agy /usage` are omitted from Overview
                // and shown verbatim only in the Accounts detail view.
            }
        } else if let Some(note) = &acct.quota_note {
            if let Some(fh) = note.five_hour_pct_x100 {
                if y < max_y {
                    let fake = QuotaPoolEntry {
                        pool: "手动".into(),
                        window: "fiveHour".into(),
                        window_label: "5 小时".into(),
                        remaining_pct_x100: fh,
                        reset_at: String::new(),
                        reset_in_seconds: None,
                    };
                    render_quota_gauge_row(
                        frame,
                        Rect::new(inner.x, y, inner.width, 1),
                        "  手动 · 5 小时",
                        &fake,
                    );
                    y += 1;
                }
            }
            if let Some(w) = note.weekly_pct_x100 {
                if y < max_y {
                    let fake = QuotaPoolEntry {
                        pool: "手动".into(),
                        window: "weekly".into(),
                        window_label: "本周".into(),
                        remaining_pct_x100: w,
                        reset_at: String::new(),
                        reset_in_seconds: None,
                    };
                    render_quota_gauge_row(
                        frame,
                        Rect::new(inner.x, y, inner.width, 1),
                        "  手动 · 本周",
                        &fake,
                    );
                    y += 1;
                }
            }
        } else if y < max_y {
            // Keybind `Q 刷新额度` is shown in the bottom bar; avoid duplicating "未更新" when title says it.
            let msg = if age_badge.contains("未更新") { "  -" } else { "  未更新" };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(msg, muted()))),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }

        for (est_line, is_warn) in format_quota_estimate_lines(acct, true) {
            let style = if is_warn {
                Style::default().fg(STATUS_PENDING)
            } else {
                muted()
            };
            for wrapped in wrap_text(&est_line, iw.saturating_sub(2).max(8)) {
                if y >= max_y {
                    break;
                }
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(format!("  {wrapped}"), style))),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                y += 1;
            }
        }
    }
}

fn render_overview_sparkline_panel(frame: &mut Frame, app: &App, area: Rect) {
    let block = rounded_block(
        Line::from(Span::styled(" 近 24 小时 Token（按账号） ", bold_primary())),
        false,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Collect (id, display_name, active, hourly_24h) from agy_accounts first, plus any extra from analytics.
    let mut series: Vec<(String, String, bool, Vec<u64>)> = Vec::new();
    for acct in &app.snapshot.agy_accounts.accounts {
        let mut h = acct.usage.hourly_24h.clone();
        if h.is_empty() {
            if let Some((_, found)) = app
                .snapshot
                .analytics
                .hourly_24h_by_account
                .iter()
                .find(|(k, _)| k == &acct.id)
            {
                h = found.clone();
            }
        }
        if h.is_empty() {
            h = vec![0; 24];
        }
        let disp = acct.display_name(app.is_account_revealed(&acct.id));
        series.push((acct.id.clone(), disp, acct.active, h));
    }
    for (id, h) in &app.snapshot.analytics.hourly_24h_by_account {
        if !series.iter().any(|(k, _, _, _)| k == id) {
            let disp = if app.is_account_revealed(id) {
                id.clone()
            } else {
                mask_account_name(id)
            };
            series.push((id.clone(), disp, false, h.clone()));
        }
    }

    if series.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("近 24 小时暂无 Token 记录", muted()))),
            inner,
        );
        return;
    }

    let shown: Vec<(String, String, bool, Vec<u64>)> = series.into_iter().take(4).collect();
    let zero_n = shown
        .iter()
        .filter(|(_, _, _, d)| d.iter().copied().sum::<u64>() == 0)
        .count();
    let active_n = shown.len().saturating_sub(zero_n);
    let chart_budget = (inner.height as usize).saturating_sub(zero_n);
    let per_h = chart_budget.checked_div(active_n).unwrap_or(3).max(3);
    let spark_h = per_h.saturating_sub(2).clamp(1, 3) as u16;
    let iw = inner.width as usize;
    let mut y = inner.y;
    let max_y = inner.y.saturating_add(inner.height);

    for (_id, disp, active, data) in shown {
        if y >= max_y {
            break;
        }
        let sum24: u64 = data.iter().copied().sum();
        let mark = if active { "★" } else { "·" };
        if sum24 == 0 {
            let empty_line = format!("{mark} {disp} 近 24 小时无用量");
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(truncate(&empty_line, iw), muted()))),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
            continue;
        }
        let peak: u64 = data.iter().copied().max().unwrap_or(0);
        let last1h: u64 = data.last().copied().unwrap_or(0);
        let color = if active { STATUS_RUNNING } else { STATUS_QUEUED };
        let title_line = format!(
            "{mark} {disp}  共 {} · 近 1 小时 {} · 峰值 {}/小时",
            format_tokens_short(sum24),
            format_tokens_short(last1h),
            format_tokens_short(peak),
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate(&title_line, iw),
                if active { bold_primary() } else { primary() },
            ))),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;

        let avail_h = max_y.saturating_sub(y).min(spark_h);
        if avail_h > 0 {
            // Stretch 24 hourly points cleanly across available width (1 or 2 cols per hour).
            let repeat_x = if inner.width >= 48 { 2 } else { 1 };
            let mut expanded = Vec::with_capacity(data.len() * repeat_x);
            for &v in &data {
                for _ in 0..repeat_x {
                    expanded.push(v);
                }
            }
            let spark = Sparkline::default()
                .data(&expanded)
                .max(peak.max(1))
                .style(Style::default().fg(color))
                .bar_set(bar::NINE_LEVELS);
            frame.render_widget(spark, Rect::new(inner.x, y, inner.width.min(expanded.len() as u16), avail_h));
            y += avail_h;
        }

        if y < max_y {
            let axis_w = if inner.width >= 48 { 48 } else { 24 }.min(iw);
            let axis = format_sparkline_axis(axis_w);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(truncate(&axis, iw), muted()))),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }
    }
}

fn render_overview_barchart_panel(frame: &mut Frame, app: &App, area: Rect) {
    // Keybind `v 切图` is shown in the bottom bar; panel title states current mode only.
    let mode_label = match app.overview_bar_mode {
        OverviewBarMode::Purpose => "按用途",
        OverviewBarMode::Model => "按模型",
    };
    let block = rounded_block(
        Line::from(Span::styled(
            format!(" 近 7 天 Token 分布 · {mode_label} "),
            bold_primary(),
        )),
        false,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let bp = &app.snapshot.analytics.by_purpose;
    let total_p = bp.total();
    let models = &app.snapshot.analytics.by_model;
    let iw = inner.width as usize;

    let has_data = total_p > 0 || !models.is_empty();
    if !has_data {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("近 7 天暂无 Token 分布数据", muted()))),
            inner,
        );
        return;
    }

    let footer_h: u16 = if inner.height >= 7 { 1 } else { 0 };
    let chart_h = inner.height.saturating_sub(footer_h);
    let chart_area = Rect::new(inner.x, inner.y, inner.width, chart_h);

    match app.overview_bar_mode {
        OverviewBarMode::Purpose => {
            let items = [
                ("写", bp.write, STATUS_RUNNING),
                ("审查", bp.review, STATUS_QUEUED),
                ("返工", bp.rework, STATUS_PENDING),
                ("只读", bp.read_only, Color::DarkGray),
            ];
            let max_v = items.iter().map(|(_, v, _)| *v).max().unwrap_or(1).max(1);
            let bw: u16 = if inner.width >= 48 { 8 } else { 6 };
            let bg: u16 = if inner.width >= 48 { 4 } else { 2 };
            let bars: Vec<Bar> = items
                .iter()
                .map(|(lbl, val, color)| {
                    Bar::default()
                        .value(*val)
                        .label(Line::from(*lbl))
                        .text_value(format_tokens_short(*val))
                        .style(Style::default().fg(*color))
                })
                .collect();
            let chart = BarChart::default()
                .data(BarGroup::default().bars(&bars))
                .bar_width(bw)
                .bar_gap(bg)
                .max(max_v)
                .value_style(bold_primary())
                .label_style(primary());
            frame.render_widget(chart, chart_area);
        }
        OverviewBarMode::Model => {
            let take_n = if inner.width >= 48 { 4 } else { 3 };
            let shown: Vec<&ModelTokens> = models.iter().take(take_n).collect();
            let max_v = shown.iter().map(|m| m.tokens).max().unwrap_or(1).max(1);
            let bw: u16 = if inner.width >= 48 { 10 } else { 8 };
            let bg: u16 = 2;
            let colors = [STATUS_RUNNING, STATUS_QUEUED, STATUS_PENDING, Color::DarkGray];
            let bars: Vec<Bar> = shown
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    Bar::default()
                        .value(m.tokens)
                        .label(Line::from(truncate(&m.model, bw as usize)))
                        .text_value(format_tokens_short(m.tokens))
                        .style(Style::default().fg(colors[i % colors.len()]))
                })
                .collect();
            let chart = BarChart::default()
                .data(BarGroup::default().bars(&bars))
                .bar_width(bw)
                .bar_gap(bg)
                .max(max_v)
                .value_style(bold_primary())
                .label_style(primary());
            frame.render_widget(chart, chart_area);
        }
    }

    // Footer shows complementary percentages for the active chart mode without duplicating bar values.
    if footer_h >= 1 {
        let fy = inner.y + chart_h;
        let line = match app.overview_bar_mode {
            OverviewBarMode::Purpose => {
                let pct = |v: u64| -> u64 { (v * 100 + total_p / 2).checked_div(total_p).unwrap_or(0) };
                format!(
                    "占比: 写 {}% · 审查 {}% · 返工 {}% · 只读 {}%",
                    pct(bp.write),
                    pct(bp.review),
                    pct(bp.rework),
                    pct(bp.read_only),
                )
            }
            OverviewBarMode::Model => {
                let total_m: u64 = models.iter().map(|m| m.tokens).sum();
                let parts: Vec<String> = models
                    .iter()
                    .take(3)
                    .map(|m| {
                        let name = m.model.strip_suffix("-latest").unwrap_or(&m.model);
                        let p = (m.tokens * 100 + total_m / 2).checked_div(total_m).unwrap_or(0);
                        format!("{} {}%", truncate(name, 13), p)
                    })
                    .collect();
                if parts.is_empty() {
                    "暂无模型记录".to_string()
                } else {
                    format!("占比: {}", parts.join(" · "))
                }
            }
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(truncate(&line, iw), muted()))),
            Rect::new(inner.x, fy, inner.width, 1),
        );
    }
}

fn render_overview_trend_panel(frame: &mut Frame, app: &App, area: Rect) {
    let block = rounded_block(
        Line::from(Span::styled(" 近 7 天交付走势 ", bold_primary())),
        false,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let daily = &app.snapshot.analytics.daily_7d;
    let iw = inner.width as usize;
    if daily.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("近 7 天暂无交付记录", muted()))),
            inner,
        );
        return;
    }

    let halves = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);

    // Upper half: Dispatch -> Merge median duration (minutes)
    let mut all_medians: Vec<u64> = daily
        .iter()
        .filter_map(|d| d.median_merge_secs)
        .collect();
    all_medians.sort_unstable();
    let total_merged: u64 = daily.iter().map(|d| d.merged_count).sum();
    let overall_med_txt = if all_medians.is_empty() {
        "-".to_string()
    } else {
        format_minutes_zh((all_medians[all_medians.len() / 2] / 60).max(1))
    };
    let merge_hdr = format!("派发→合入中位耗时  {overall_med_txt} · 共 {total_merged} 单");
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(truncate(&merge_hdr, iw), primary()))),
        Rect::new(halves[0].x, halves[0].y, halves[0].width, 1),
    );
    if halves[0].height >= 3 {
        let chart_area = Rect::new(
            halves[0].x,
            halves[0].y + 1,
            halves[0].width,
            halves[0].height.saturating_sub(1),
        );
        let max_mins = daily
            .iter()
            .filter_map(|d| d.median_merge_secs)
            .map(|s| (s / 60).max(1))
            .max()
            .unwrap_or(1)
            .max(1);
        let bw: u16 = if halves[0].width >= 56 { 6 } else { 5 };
        let bg: u16 = if halves[0].width >= 56 { 2 } else { 1 };
        let bars: Vec<Bar> = daily
            .iter()
            .map(|d| {
                let mins = d.median_merge_secs.map(|s| (s / 60).max(1)).unwrap_or(0);
                let txt = if d.median_merge_secs.is_some() {
                    format!("{mins}分")
                } else {
                    "-".to_string()
                };
                Bar::default()
                    .value(mins)
                    .label(Line::from(d.date.clone()))
                    .text_value(txt)
                    .style(Style::default().fg(STATUS_QUEUED))
            })
            .collect();
        let chart = BarChart::default()
            .data(BarGroup::default().bars(&bars))
            .bar_width(bw)
            .bar_gap(bg)
            .max(max_mins)
            .value_style(primary())
            .label_style(muted());
        frame.render_widget(chart, chart_area);
    }

    // Lower half: First-round pass rate (%)
    let total_chains: u64 = daily.iter().map(|d| d.total_chains).sum();
    let total_pass: u64 = daily.iter().map(|d| d.first_round_pass).sum();
    let pass_summary = match (total_pass * 100 + total_chains / 2).checked_div(total_chains) {
        Some(pct) => format!("一轮通过率  {pct}% ({total_pass}/{total_chains})"),
        None => "一轮通过率  暂无样本".to_string(),
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(truncate(&pass_summary, iw), primary()))),
        Rect::new(halves[1].x, halves[1].y, halves[1].width, 1),
    );
    if halves[1].height >= 3 {
        let chart_area = Rect::new(
            halves[1].x,
            halves[1].y + 1,
            halves[1].width,
            halves[1].height.saturating_sub(1),
        );
        let bw: u16 = if halves[1].width >= 56 { 6 } else { 5 };
        let bg: u16 = if halves[1].width >= 56 { 2 } else { 1 };
        let bars: Vec<Bar> = daily
            .iter()
            .map(|d| {
                let val = d.first_round_pass_pct.unwrap_or(0);
                let txt = match d.first_round_pass_pct {
                    Some(p) => format!("{p}%"),
                    None => "-".to_string(),
                };
                let color = if d.first_round_pass_pct.is_none() {
                    Color::DarkGray
                } else if val >= 75 {
                    STATUS_RUNNING
                } else if val >= 50 {
                    STATUS_PENDING
                } else {
                    STATUS_FAILED
                };
                Bar::default()
                    .value(val)
                    .label(Line::from(d.date.clone()))
                    .text_value(txt)
                    .style(Style::default().fg(color))
            })
            .collect();
        let chart = BarChart::default()
            .data(BarGroup::default().bars(&bars))
            .bar_width(bw)
            .bar_gap(bg)
            .max(100)
            .value_style(primary())
            .label_style(muted());
        frame.render_widget(chart, chart_area);
    }
}

fn render_overview_narrow(frame: &mut Frame, app: &mut App, area: Rect) {
    let state = &app.snapshot.agy_accounts;
    let age_badge = if app.quota_refreshing {
        "刷新中…".to_string()
    } else if let Some(age) = state.quota_age_seconds {
        format_quota_updated(age)
    } else if let Some(age) = state
        .accounts
        .iter()
        .filter_map(|a| a.quota.as_ref().map(|q| q.age_seconds))
        .min()
    {
        format_quota_updated(age)
    } else {
        "未更新".to_string()
    };
    // Narrow terminals degrade charts to plain text without extra layout labels.
    let block = rounded_block(
        Line::from(Span::styled(format!(" 总览 · {age_badge} "), bold_primary())),
        true,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let iw = inner.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();

    // 1. Account Quota (text degraded; advisory only, never auto-switches accounts)
    lines.push(section_header("账号额度"));
    if state.accounts.is_empty() {
        lines.push(Line::from(Span::styled("  暂无 agy 账号", muted())));
    } else {
        for acct in &state.accounts {
            let mark = if acct.active { "★" } else { "·" };
            let disp = acct.display_name(app.is_account_revealed(&acct.id));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("{mark} "),
                    if acct.active {
                        Style::default()
                            .fg(STATUS_RUNNING)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        muted()
                    },
                ),
                Span::styled(truncate(&disp, iw.saturating_sub(4)), bold_primary()),
            ]));
            if let Some(q) = &acct.quota {
                if !q.ok {
                    push_indented_wrapped(
                        &mut lines,
                        "    ",
                        &format!("✗ {}", q.error.as_deref().unwrap_or("刷新失败")),
                        iw,
                        Style::default().fg(STATUS_FAILED),
                    );
                } else if q.entries.is_empty() {
                    lines.push(Line::from(Span::styled("    暂无额度数据", muted())));
                } else {
                    for e in &q.entries {
                        let r = format_reset_countdown_zh(e.reset_in_seconds);
                        let r_part = if r.is_empty() {
                            String::new()
                        } else {
                            format!(" · {r}")
                        };
                        let row = format!(
                            "{} · {}  剩余 {}{r_part}",
                            short_pool_name(&e.pool),
                            window_display_name(e),
                            format_pct_clean(e.remaining_pct_x100),
                        );
                        push_indented_wrapped(
                            &mut lines,
                            "    ",
                            &row,
                            iw,
                            Style::default().fg(quota_bar_color(e.remaining_pct_x100)),
                        );
                    }
                }
            } else if let Some(note) = &acct.quota_note {
                if let Some(fh) = note.five_hour_pct_x100 {
                    push_indented_wrapped(
                        &mut lines,
                        "    ",
                        &format!("手动 · 5 小时  剩余 {}", format_pct_clean(fh)),
                        iw,
                        primary(),
                    );
                }
                if let Some(w) = note.weekly_pct_x100 {
                    push_indented_wrapped(
                        &mut lines,
                        "    ",
                        &format!("手动 · 本周  剩余 {}", format_pct_clean(w)),
                        iw,
                        primary(),
                    );
                }
            } else {
                let msg = if age_badge == "未更新" { "    -" } else { "    未更新" };
                lines.push(Line::from(Span::styled(msg, muted())));
            }
            for (est_line, is_warn) in format_quota_estimate_lines(acct, true) {
                let style = if is_warn {
                    Style::default().fg(STATUS_PENDING)
                } else {
                    muted()
                };
                push_indented_wrapped(&mut lines, "    ", &est_line, iw, style);
            }
        }
    }

    // 2. 24h Hourly Token per account (text sparkline)
    lines.push(section_header("近 24 小时 Token（按账号）"));
    if state.accounts.is_empty() && app.snapshot.analytics.hourly_24h_by_account.is_empty() {
        lines.push(Line::from(Span::styled("  近 24 小时暂无记录", muted())));
    } else {
        for acct in &state.accounts {
            let h = if !acct.usage.hourly_24h.is_empty() {
                acct.usage.hourly_24h.as_slice()
            } else {
                app.snapshot
                    .analytics
                    .hourly_24h_by_account
                    .iter()
                    .find(|(k, _)| k == &acct.id)
                    .map(|(_, v)| v.as_slice())
                    .unwrap_or(&[])
            };
            let sum24: u64 = h.iter().copied().sum();
            let mark = if acct.active { "★" } else { "·" };
            let disp = acct.display_name(app.is_account_revealed(&acct.id));
            if sum24 == 0 {
                push_indented_wrapped(
                    &mut lines,
                    "  ",
                    &format!("{mark} {disp} 近 24 小时无用量"),
                    iw,
                    muted(),
                );
                continue;
            }
            let last1h = h.last().copied().unwrap_or(0);
            let peak = h.iter().copied().max().unwrap_or(0);
            let spark = inline_spark_text(h, 12);
            let txt = format!(
                "{mark} {disp}  共 {} · 近 1 小时 {} · 峰 {}/小时  {spark}",
                format_tokens_short(sum24),
                format_tokens_short(last1h),
                format_tokens_short(peak),
            );
            push_indented_wrapped(&mut lines, "  ", &txt, iw, primary());
        }
    }

    // 3. Purpose / Model Distribution
    lines.push(section_header("近 7 天 Token 分布"));
    let bp = &app.snapshot.analytics.by_purpose;
    let total_p = bp.total();
    if total_p > 0 {
        let pct = |v: u64| (v * 100 + total_p / 2) / total_p;
        let p_txt = format!(
            "用途: 写 {}%({}) · 审查 {}%({}) · 返工 {}%({}) · 只读 {}%({})",
            pct(bp.write),
            format_tokens_short(bp.write),
            pct(bp.review),
            format_tokens_short(bp.review),
            pct(bp.rework),
            format_tokens_short(bp.rework),
            pct(bp.read_only),
            format_tokens_short(bp.read_only),
        );
        push_indented_wrapped(&mut lines, "  ", &p_txt, iw, primary());
    }
    if !app.snapshot.analytics.by_model.is_empty() {
        let m_parts: Vec<String> = app
            .snapshot
            .analytics
            .by_model
            .iter()
            .take(4)
            .map(|m| format!("{} {}", m.model, format_tokens_short(m.tokens)))
            .collect();
        push_indented_wrapped(&mut lines, "  ", &format!("模型: {}", m_parts.join(" · ")), iw, muted());
    }

    // 4. 7d Merge Latency & First-Round Pass Rate
    lines.push(section_header("近 7 天交付走势"));
    let daily = &app.snapshot.analytics.daily_7d;
    if daily.is_empty() {
        lines.push(Line::from(Span::styled("  近 7 天暂无数据", muted())));
    } else {
        let merge_parts: Vec<String> = daily
            .iter()
            .map(|d| match d.median_merge_secs {
                Some(s) => format!("{}:{}", d.date, (s / 60).max(1)),
                None => format!("{}:-", d.date),
            })
            .collect();
        let pass_parts: Vec<String> = daily
            .iter()
            .map(|d| match d.first_round_pass_pct {
                Some(p) => format!("{}:{p}%", d.date),
                None => format!("{}:-", d.date),
            })
            .collect();
        let total_chains: u64 = daily.iter().map(|d| d.total_chains).sum();
        let total_pass: u64 = daily.iter().map(|d| d.first_round_pass).sum();
        let overall_pass = match (total_pass * 100 + total_chains / 2).checked_div(total_chains) {
            Some(pct) => format!("{pct}% ({total_pass}/{total_chains})"),
            None => "-".to_string(),
        };
        push_indented_wrapped(
            &mut lines,
            "  ",
            &format!("合入中位(分): {}", merge_parts.join(" ")),
            iw,
            primary(),
        );
        push_indented_wrapped(
            &mut lines,
            "  ",
            &format!("一轮通过 {overall_pass}: {}", pass_parts.join(" ")),
            iw,
            primary(),
        );
    }

    let view_h = inner.height as usize;
    let max_scroll = lines.len().saturating_sub(view_h);
    if app.overview_scroll > max_scroll {
        app.overview_scroll = max_scroll;
    }
    let visible: Vec<Line> = lines
        .into_iter()
        .skip(app.overview_scroll)
        .take(view_h)
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

// ---------------------------------------------------------------------------
// Agy Accounts View: list, active marker, detail pane, and in-place confirm modal
// ---------------------------------------------------------------------------

fn render_accounts_list_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::List && matches!(app.overlay, Overlay::None);
    let total = app.filtered_accounts.len();
    // Quota update timestamp and running agy process count are shown once in the detail pane.
    let title_text = if app.filter.is_empty() {
        format!(" agy 账号 ({total}) ")
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

    let show_usage = iw >= 42;
    let u5_w: usize = if show_usage { 9 } else { 0 };
    let u7_w: usize = if show_usage { 7 } else { 0 };
    let used_w: usize = if iw >= 64 { 11 } else { 0 };
    let status_w: usize = 6; // "★ 活跃" or "  备用"
    let extra_w = (if show_usage { 1 + u5_w + 1 + u7_w } else { 0 })
        + (if used_w > 0 { 1 + used_w } else { 0 });
    let id_w: usize = iw.saturating_sub(2 + status_w + 1 + extra_w).max(8);

    let mut lines: Vec<Line> = Vec::with_capacity(inner.height as usize);
    if show_header {
        let mut hdr = String::new();
        hdr.push_str("  ");
        hdr.push_str(&pad_right("状态", status_w));
        hdr.push(' ');
        hdr.push_str(&pad_right("账号", id_w));
        if show_usage {
            hdr.push(' ');
            hdr.push_str(&pad_left("5小时用量", u5_w));
            hdr.push(' ');
            hdr.push_str(&pad_left("7天用量", u7_w));
        }
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
        let disp_name = acct.display_name(app.is_account_revealed(&acct.id));
        let mut row_spans = vec![
            stripe,
            Span::styled(badge_txt, badge_style),
            Span::raw(" "),
            Span::styled(pad_right(&disp_name, id_w), id_style),
        ];
        if show_usage {
            let u5 = &acct.usage.five_hour;
            let u7 = &acct.usage.seven_day;
            let u5_txt = if u5.uncounted_running > 0 {
                format!("{}+", format_tokens_short(u5.total))
            } else {
                format_tokens_short(u5.total)
            };
            let u7_txt = format_tokens_short(u7.total);
            let u5_style = if u5.uncounted_running > 0 {
                Style::default().fg(STATUS_PENDING)
            } else if u5.total > 0 {
                primary()
            } else {
                muted()
            };
            let u7_style = if u7.total > 0 { primary() } else { muted() };
            row_spans.push(Span::raw(" "));
            row_spans.push(Span::styled(pad_left(&u5_txt, u5_w), u5_style));
            row_spans.push(Span::raw(" "));
            row_spans.push(Span::styled(pad_left(&u7_txt, u7_w), u7_style));
        }
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

fn format_tokens_short(n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let f = n as f64;
    if f >= 1_000_000_000.0 {
        format!("{:.2}B", f / 1_000_000_000.0)
    } else if f >= 1_000_000.0 {
        format!("{:.1}M", f / 1_000_000.0)
    } else if f >= 1_000.0 {
        format!("{:.1}K", f / 1_000.0)
    } else {
        format!("{n}")
    }
}

fn format_usage_window_line(label: &str, w: &AccountUsageWindow, include_total: bool) -> String {
    let mut text = if w.total == 0 {
        format!("{label}: 0")
    } else if include_total {
        format!(
            "{label}: {}（输入 {} / 输出 {} / 缓存 {} · 作者 {} / 审查 {}）",
            format_tokens_short(w.total),
            format_tokens_short(w.input),
            format_tokens_short(w.output),
            format_tokens_short(w.cache_read),
            format_tokens_short(w.author_total),
            format_tokens_short(w.reviewer_total),
        )
    } else {
        // Totals for 5h/7d are already displayed in the Accounts list columns; show breakdown only.
        format!(
            "{label}: 输入 {} / 输出 {} / 缓存 {} · 作者 {} / 审查 {}",
            format_tokens_short(w.input),
            format_tokens_short(w.output),
            format_tokens_short(w.cache_read),
            format_tokens_short(w.author_total),
            format_tokens_short(w.reviewer_total),
        )
    };
    if w.uncounted_running > 0 {
        text.push_str(&format!(" · 运行中未计 {}", w.uncounted_running));
    }
    text
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

fn format_running_agy_proc(p: &RunningAgyProc) -> String {
    if let Some(r) = p.run.as_deref().filter(|s| !s.is_empty()) {
        return format!("{} · {r}", p.pid);
    }
    // Non-delegate agy process: extract agent name (e.g. `--agent aia-lead`) or short tag, never dump cmdline.
    let tokens: Vec<&str> = p.cmdline.split_whitespace().collect();
    for (i, tok) in tokens.iter().enumerate() {
        if *tok == "--agent" {
            if let Some(name) = tokens.get(i + 1).filter(|s| !s.is_empty()) {
                return format!("{} · {name}", p.pid);
            }
        } else if let Some(name) = tok.strip_prefix("--agent=") {
            if !name.is_empty() {
                return format!("{} · {name}", p.pid);
            }
        }
    }
    if tokens.windows(2).any(|w| w[0] == "-p" && w[1] == "/usage") {
        return format!("{} · agy /usage", p.pid);
    }
    format!("{} · agy", p.pid)
}

fn render_account_detail_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::Detail && matches!(app.overlay, Overlay::None);
    let title = match app.selected_account_item() {
        Some(a) => {
            let disp = a.display_name(app.is_account_revealed(&a.id));
            if app.quota_refreshing {
                format!(" {disp} · 刷新中… ")
            } else if let Some(q) = &a.quota {
                format!(" {disp} · {} ", format_quota_updated(q.age_seconds))
            } else {
                format!(" {disp} ")
            }
        }
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

    // Wide visual mode with ratatui Gauge + Sparkline when the selected account has real quota.
    let has_real_quota = app
        .selected_account_item()
        .is_some_and(|a| a.quota.is_some());
    if has_real_quota && area.width >= 52 && inner.height >= 18 && app.account_switch_error.is_none() {
        render_account_detail_wide_visual(frame, app, inner);
        return;
    }

    let iw = inner.width as usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    let state = &app.snapshot.agy_accounts;

    if let Some(err) = &app.account_switch_error {
        out.push(Line::from(Span::styled(
            "▸ 切换失败（Esc 清除）",
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
        let revealed = app.is_account_revealed(&acct.id);
        // Hide label by default (may contain imported display name); reveal on `i`.
        if revealed && !acct.label.is_empty() {
            out.push(Line::from(vec![
                Span::styled("备注: ", muted()),
                Span::styled(acct.label.clone(), primary()),
            ]));
        }

        // Quota text-degraded section (used on narrow 80-col terminals).
        // Source: `agy -p /usage`; update timestamp is already shown in the pane title.
        if let Some(q) = &acct.quota {
            out.push(section_header("额度"));
            if !q.ok {
                let err_msg = q.error.as_deref().unwrap_or("刷新失败");
                push_indented_wrapped(
                    &mut out,
                    "  ",
                    &format!("✗ {err_msg}"),
                    iw,
                    Style::default().fg(STATUS_FAILED),
                );
            } else {
                for e in &q.entries {
                    let reset = format_reset_countdown_zh(e.reset_in_seconds);
                    let r_part = if reset.is_empty() {
                        String::new()
                    } else {
                        format!(" · {reset}")
                    };
                    let row = format!(
                        "{} · {}  剩余 {}{r_part}",
                        short_pool_name(&e.pool),
                        window_display_name(e),
                        format_pct_clean(e.remaining_pct_x100),
                    );
                    push_indented_wrapped(
                        &mut out,
                        "  ",
                        &row,
                        iw,
                        Style::default().fg(quota_bar_color(e.remaining_pct_x100)),
                    );
                }
                for unk in &q.unknown_lines {
                    push_indented_wrapped(&mut out, "  ", unk, iw, muted());
                }
            }
            for (est_line, is_warn) in format_quota_estimate_lines(acct, false) {
                let style = if is_warn {
                    Style::default().fg(STATUS_PENDING)
                } else {
                    muted()
                };
                push_indented_wrapped(&mut out, "  ", &est_line, iw, style);
            }
            if !acct.usage.hourly_24h.is_empty() {
                let sum24: u64 = acct.usage.hourly_24h.iter().copied().sum();
                let spark = inline_spark_text(&acct.usage.hourly_24h, 12);
                push_indented_wrapped(
                    &mut out,
                    "  ",
                    &format!("近 24 小时 Token: {}  {spark}", format_tokens_short(sum24)),
                    iw,
                    primary(),
                );
            }
            out.push(Line::raw(""));
        } else {
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
            // Credentials are stored locally with mode 0600 and tokens are never printed.
            out.push(Line::from(vec![
                Span::styled("凭据文件: ", muted()),
                Span::styled(format!("{} 份", acct.file_count), primary()),
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

        // Local token usage (computed from local logs without calling external APIs).
        // 5h/7d totals are already in the list columns, so detail shows breakdowns only.
        out.push(section_header("本地用量"));
        if acct.usage.one_hour.total > 0 {
            push_indented_wrapped(
                &mut out,
                "  ",
                &format_usage_window_line("近 1 小时", &acct.usage.one_hour, true),
                iw,
                primary(),
            );
        }
        push_indented_wrapped(
            &mut out,
            "  ",
            &format_usage_window_line("近 5 小时", &acct.usage.five_hour, false),
            iw,
            if acct.usage.five_hour.total > 0 { primary() } else { muted() },
        );
        push_indented_wrapped(
            &mut out,
            "  ",
            &format_usage_window_line("近 7 天", &acct.usage.seven_day, false),
            iw,
            if acct.usage.seven_day.total > 0 { primary() } else { muted() },
        );
        if let Some(note) = &acct.quota_note {
            let mut parts = Vec::new();
            if let Some(w) = note.weekly_pct_x100 {
                let ts = format_iso_compact(&note.weekly_at);
                parts.push(format!("本周剩余 {} ({ts})", format_pct_clean(w)));
            }
            if let Some(fh) = note.five_hour_pct_x100 {
                let ts = format_iso_compact(&note.five_hour_at);
                parts.push(format!("5 小时剩余 {} ({ts})", format_pct_clean(fh)));
            }
            if !parts.is_empty() {
                // Real quota takes precedence over manual `note` records when both exist.
                push_indented_wrapped(
                    &mut out,
                    "  ",
                    &format!("手动记录: {}", parts.join(" · ")),
                    iw,
                    if acct.quota.is_some() { muted() } else { primary() },
                );
            }
            if acct.quota.is_none() {
                for (est_line, is_warn) in format_quota_estimate_lines(acct, false) {
                    let style = if is_warn {
                        Style::default().fg(STATUS_PENDING)
                    } else {
                        muted()
                    };
                    push_indented_wrapped(&mut out, "  ", &est_line, iw, style);
                }
            }
        } else if acct.quota.is_none() {
            push_indented_wrapped(&mut out, "  ", "额度: 未更新", iw, muted());
        }
        out.push(Line::raw(""));
    }

    // Live environment & running agy processes section
    out.push(section_header("本机状态"));
    // Only show `~/.gemini` live email when drifted; mask unless revealed.
    if state.live_drifted && !state.live_email.is_empty() {
        let live_disp = if app
            .selected_account_item()
            .is_some_and(|a| app.is_account_revealed(&a.id))
        {
            state.live_email.clone()
        } else {
            mask_account_name(&state.live_email)
        };
        push_indented_wrapped(
            &mut out,
            "  ",
            &format!("~/.gemini 当前账号: {live_disp}（与活跃槽位不一致）"),
            iw,
            Style::default()
                .fg(STATUS_PENDING)
                .add_modifier(Modifier::BOLD),
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
    // Switching is blocked while any agy process is running to prevent token conflicts.
    let proc_summary = if run_cnt > 0 {
        format!("在跑 agy 进程: {run_cnt} 个（有进程时无法切换）")
    } else {
        "在跑 agy 进程: 0 个".to_string()
    };
    push_indented_wrapped(&mut out, "  ", &proc_summary, iw, proc_style);
    for p in state.running_agy.iter().take(6) {
        let line = format!("    {}", format_running_agy_proc(p));
        out.push(Line::from(Span::styled(truncate(&line, iw), muted())));
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

fn render_account_detail_wide_visual(frame: &mut Frame, app: &mut App, inner: Rect) {
    let Some(acct) = app.selected_account_item().cloned() else {
        return;
    };
    let state = &app.snapshot.agy_accounts;
    let iw = inner.width as usize;
    let mut y = inner.y;
    let max_y = inner.y.saturating_add(inner.height);

    // 1. Metadata not already visible in the left list row (hide label by default; reveal on `i`)
    if y < max_y {
        let revealed = app.is_account_revealed(&acct.id);
        let mut meta_parts = Vec::new();
        if revealed && !acct.label.is_empty() {
            meta_parts.push(acct.label.clone());
        }
        meta_parts.push(format!(
            "最后使用 {}",
            if acct.last_used_at.is_empty() {
                "-".to_string()
            } else {
                format_iso_compact(&acct.last_used_at)
            }
        ));
        meta_parts.push(format!("凭据 {} 份", acct.file_count));
        let meta_line = meta_parts.join(" · ");
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(truncate(&meta_line, iw), muted()))),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
    }

    // 2. Quota Gauges (collected via `agy -p /usage`; update time shown once in pane title)
    if let Some(q) = &acct.quota {
        if y < max_y {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled("▸ 额度", bold_primary()))),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }
        if !q.ok {
            if y < max_y {
                let err_msg = q.error.as_deref().unwrap_or("刷新失败");
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        truncate(&format!("  ✗ {err_msg}"), iw),
                        Style::default().fg(STATUS_FAILED),
                    ))),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                y += 1;
            }
        } else {
            for e in &q.entries {
                if y >= max_y.saturating_sub(4) {
                    break;
                }
                let lbl = format!(
                    "  {} · {}",
                    short_pool_name(&e.pool),
                    window_display_name(e)
                );
                render_quota_gauge_row(frame, Rect::new(inner.x, y, inner.width, 1), &lbl, e);
                y += 1;
            }
            for unk in &q.unknown_lines {
                if y >= max_y.saturating_sub(4) {
                    break;
                }
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        truncate(&format!("  {unk}"), iw),
                        muted(),
                    ))),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                y += 1;
            }
        }
        for (est_line, is_warn) in format_quota_estimate_lines(&acct, false) {
            let style = if is_warn {
                Style::default().fg(STATUS_PENDING)
            } else {
                muted()
            };
            for wrapped in wrap_text(&est_line, iw.saturating_sub(2).max(8)) {
                if y >= max_y.saturating_sub(4) {
                    break;
                }
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(format!("  {wrapped}"), style))),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                y += 1;
            }
        }
        if let Some(note) = &acct.quota_note {
            if y < max_y.saturating_sub(4) {
                let mut parts = Vec::new();
                if let Some(w) = note.weekly_pct_x100 {
                    parts.push(format!("本周剩余 {}", format_pct_clean(w)));
                }
                if let Some(fh) = note.five_hour_pct_x100 {
                    parts.push(format!("5 小时剩余 {}", format_pct_clean(fh)));
                }
                if !parts.is_empty() {
                    // Real quota takes precedence over manual `note` records when both exist.
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            truncate(&format!("  手动记录: {}", parts.join(" · ")), iw),
                            muted(),
                        ))),
                        Rect::new(inner.x, y, inner.width, 1),
                    );
                    y += 1;
                }
            }
        }
    }

    // 3. 24h Hourly Token Sparkline for this account
    if y + 4 < max_y {
        y += 1;
        let data: Vec<u64> = if !acct.usage.hourly_24h.is_empty() {
            acct.usage.hourly_24h.clone()
        } else {
            vec![0; 24]
        };
        let sum24: u64 = data.iter().copied().sum();
        let peak: u64 = data.iter().copied().max().unwrap_or(0);
        let hdr = format!(
            "▸ 近 24 小时 Token  {} · 近 1 小时 {} · 峰 {}/小时",
            format_tokens_short(sum24),
            format_tokens_short(acct.usage.one_hour.total),
            format_tokens_short(peak),
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(truncate(&hdr, iw), bold_primary()))),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;

        let spark_h = max_y.saturating_sub(y + 5).clamp(2, 3);
        let repeat_x = if inner.width >= 50 { 2 } else { 1 };
        let mut expanded = Vec::with_capacity(data.len() * repeat_x);
        for &v in &data {
            for _ in 0..repeat_x {
                expanded.push(v);
            }
        }
        let spark_color = if acct.active { STATUS_RUNNING } else { STATUS_QUEUED };
        let spark = Sparkline::default()
            .data(&expanded)
            .max(peak.max(1))
            .style(Style::default().fg(spark_color))
            .bar_set(bar::NINE_LEVELS);
        frame.render_widget(
            spark,
            Rect::new(inner.x + 2, y, inner.width.saturating_sub(2).min(expanded.len() as u16), spark_h),
        );
        y += spark_h;

        if y < max_y {
            let axis_w = expanded.len().min(iw.saturating_sub(2));
            let axis = format!("  {}", format_sparkline_axis(axis_w));
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(truncate(&axis, iw), muted()))),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }
    }

    // 4. Remaining space: host status + local usage breakdown (scrollable)
    if y < max_y {
        y += 1;
        let rem_h = max_y.saturating_sub(y) as usize;
        if rem_h > 0 {
            let mut out: Vec<Line<'static>> = Vec::new();
            out.push(section_header("本机状态与用量"));
            // Only show `~/.gemini` live email when drifted; mask unless revealed.
            if state.live_drifted && !state.live_email.is_empty() {
                let live_disp = if app.is_account_revealed(&acct.id) {
                    state.live_email.clone()
                } else {
                    mask_account_name(&state.live_email)
                };
                push_indented_wrapped(
                    &mut out,
                    "  ",
                    &format!("~/.gemini 当前账号: {live_disp}（与活跃槽位不一致）"),
                    iw,
                    Style::default().fg(STATUS_PENDING).add_modifier(Modifier::BOLD),
                );
            }
            let run_cnt = state.running_agy.len();
            // Switching is blocked while any agy process is running to prevent token conflicts.
            let proc_summary = if run_cnt > 0 {
                format!("在跑 agy 进程: {run_cnt} 个（有进程时无法切换）")
            } else {
                "在跑 agy 进程: 0 个".to_string()
            };
            push_indented_wrapped(
                &mut out,
                "  ",
                &proc_summary,
                iw,
                if run_cnt > 0 {
                    Style::default().fg(STATUS_PENDING).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(STATUS_RUNNING)
                },
            );
            for p in state.running_agy.iter().take(6) {
                let line = format!("    {}", format_running_agy_proc(p));
                out.push(Line::from(Span::styled(truncate(&line, iw), muted())));
            }
            push_indented_wrapped(
                &mut out,
                "  ",
                &format_usage_window_line("近 5 小时", &acct.usage.five_hour, false),
                iw,
                muted(),
            );
            push_indented_wrapped(
                &mut out,
                "  ",
                &format_usage_window_line("近 7 天", &acct.usage.seven_day, false),
                iw,
                muted(),
            );
            // Keybinds (Enter/u switch, Q refresh quota) are shown in the bottom bar;
            // account switching is always manual and never auto-rotates.
            let max_scroll = out.len().saturating_sub(rem_h);
            if app.detail_scroll > max_scroll {
                app.detail_scroll = max_scroll;
            }
            let visible: Vec<Line> = out.into_iter().skip(app.detail_scroll).take(rem_h).collect();
            frame.render_widget(
                Paragraph::new(visible),
                Rect::new(inner.x, y, inner.width, rem_h as u16),
            );
        }
    }
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

    // Always manual switch (never auto-rotates); saves refresh token before atomic credential swap.
    let note = if is_already_active {
        "该账号已是当前活跃账号；确认后回存当前令牌。"
    } else {
        "确认后回存当前账号令牌，并切换本地 agy 凭据。"
    };
    for l in wrap_text(note, iw) {
        lines.push(Line::from(Span::styled(l, primary())));
    }
    if running_count > 0 {
        lines.push(Line::raw(""));
        let warn = format!(
            "当前有 {running_count} 个在跑 agy 进程，确认后切换会被拒绝并列出进程。"
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

#[cfg(test)]
mod tests {
    use super::pace_prefix;

    #[test]
    fn pace_prefix_formats_all_burn_rate_sources() {
        assert_eq!(pace_prefix("24h"), "按最近 24 小时的平均用量");
        assert_eq!(pace_prefix("5h"), "按最近 5 小时的平均用量");
        assert_eq!(pace_prefix("7d"), "按最近 7 天的平均用量");
        assert_eq!(pace_prefix("since:3h"), "按最近 3 小时的平均用量");
        assert_eq!(pace_prefix("since:1h"), "按最近 1 小时的平均用量");
        assert_eq!(pace_prefix("none"), "按当前用量");
        assert_eq!(pace_prefix("since:0h"), "按当前用量");
    }
}
