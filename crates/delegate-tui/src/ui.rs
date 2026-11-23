use crate::app::{App, CollapseKind, FocusPane, ListRow, Overlay, ViewTab};
use crate::markdown::render_markdown;
use crate::model::*;
use crate::width::{display_width, pad_left, pad_right, truncate, wrap_text};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

const NARROW_BREAKPOINT: u16 = 100;

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

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(area);

    render_top_bar(frame, app, chunks[0]);

    if area.width >= NARROW_BREAKPOINT {
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
        }
    }

    render_bottom_bar(frame, app, chunks[2]);

    match &app.overlay {
        Overlay::None => {}
        Overlay::FullResult { .. } => render_text_overlay(frame, app, chunks[1], true),
        Overlay::DiffStat { .. } => render_text_overlay(frame, app, chunks[1], false),
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

    let full_name = if let (GroupKind::Running, Some(act)) = (run.group, run.activity.as_ref()) {
        let snippet = match act.kind.as_str() {
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
    out.push(Line::from(head_spans));

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

    if area.width >= 120 {
        if let Some(active) = lane.local.iter().chain(lane.remote.iter()).find(|e| e.running) {
            left_spans.push(Span::styled(
                format!("  {}", truncate(&active.label, 28)),
                muted(),
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
                    " jk 选择  1/2/e 任务·事件  Tab 切栏  Enter 结论  d diff  / 过滤  r 刷新  a/c/s/p 命令  ? 帮助  q 退出"
                } else {
                    " jk选 Tab栏 1/2/e事 Enter结论 d diff /滤 r刷 a/c/s/p令 ?助 q退"
                }
            }
            ViewTab::Events => {
                if area.width >= 100 {
                    " jk 浏览事件  f 切换跟随  1/2/e 返回任务  Tab 切栏  / 过滤  r 刷新  ? 帮助  q 退出"
                } else {
                    " jk览 f跟随 1/2/e任务 Tab栏 /滤 r刷 ?助 q退"
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
    let w = area.width.saturating_sub(6).clamp(40, 72);
    let h = 16u16.min(area.height.saturating_sub(2));
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
        ("↑↓ / j k", "选择任务/事件 / 滚动详情"),
        ("1 / 2 / e", "切换「任务」与「事件」标签页"),
        ("f", "事件页切换自动跟随"),
        ("Tab", "切换焦点栏"),
        ("Enter", "完整结论（P 交 $PAGER）"),
        ("d", "diff --stat 改动统计"),
        ("/", "过滤（Esc 清除）"),
        ("r", "立即刷新"),
        ("a / c / s / p", "生成 apply / clean / stop / reply 命令"),
        ("y", "复制推荐命令"),
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
        GroupKind::Pending => match run.state.as_str() {
            "rejected" | "failed" | "timeout" | "stalled" | "malformed" | "crashed" | "killed" => {
                ("✕", Some(STATUS_FAILED))
            }
            _ => ("◆", Some(STATUS_PENDING)),
        },
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
        "delivered" | "applied" | "queue_dispatched" => Some(STATUS_RUNNING),
        "question" | "stall_warning" | "loop_review" => Some(STATUS_PENDING),
        "rejected" | "failed" | "queue_paused" => Some(STATUS_FAILED),
        "started" | "queued" | "lane_acquired" => Some(STATUS_QUEUED),
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
