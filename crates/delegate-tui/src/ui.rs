use crate::app::{
    App, CollapseKind, FocusPane, ListRow, Overlay, RunsGroupMode, ViewTab, WorkFilterMode,
    WorkFoldKind, WorkRow,
};
use crate::data::read_work_brief_markdown;
use crate::markdown::render_markdown;
use crate::model::*;
use crate::ui_overview::{format_lines_compact, render_overview_pane};
use crate::ui_quota::render_quota_tab;
use crate::width::{display_width, pad_left, pad_right, truncate, wrap_text};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

pub(crate) const NARROW_BREAKPOINT: u16 = 110;
pub(crate) const COMPACT_BREAKPOINT: u16 = 70;

// Strict semantic palette: 4 status colors + adaptive grayscale (Color::Reset / Color::DarkGray).
// Works cleanly on both dark and light terminals without hard-coded RGB backgrounds.
pub(crate) const STATUS_PENDING: Color = Color::Yellow;
pub(crate) const STATUS_RUNNING: Color = Color::Green;
pub(crate) const STATUS_QUEUED: Color = Color::Cyan;
pub(crate) const STATUS_FAILED: Color = Color::Red;

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.last_terminal_width = area.width;
    if area.width < 20 || area.height < 8 {
        frame.render_widget(Paragraph::new("窗口过小"), area);
        return;
    }

    let alerts = app.snapshot.effective_alerts();
    let alert_h: u16 = compute_alert_strip_height(&alerts, area.width);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(alert_h),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(area);

    render_top_bar(frame, app, chunks[0]);
    if alert_h > 0 {
        render_alert_strip(frame, &alerts, chunks[1]);
    }

    let body_area = chunks[2];
    match app.tab {
        ViewTab::Overview => render_overview_pane(frame, app, body_area),
        ViewTab::Work => render_work_tab(frame, app, body_area),
        ViewTab::Runs => render_runs_tab(frame, app, body_area),
        ViewTab::Quota => render_quota_tab(frame, app, body_area),
        ViewTab::Events => render_events_tab(frame, app, body_area),
    }

    render_bottom_bar(frame, app, chunks[3]);

    match &app.overlay {
        Overlay::None => {}
        Overlay::FullResult { .. } => render_text_overlay(frame, app, body_area, true),
        Overlay::DiffStat { .. } => render_text_overlay(frame, app, body_area, false),
        Overlay::WorkCard { .. } => render_work_card_overlay(frame, app, body_area),
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

fn compute_alert_strip_height(alerts: &[ActiveAlertItem], width: u16) -> u16 {
    if alerts.is_empty() {
        return 0;
    }
    if width < NARROW_BREAKPOINT {
        return 1;
    }
    if alerts.len() <= 1 {
        return 1;
    }
    let prefix = format!(" ▲ 告警({})  ", alerts.len());
    let first = format!("[{}] {}", alerts[0].label, alerts[0].message);
    let second = format!("[{}] {}", alerts[1].label, alerts[1].message);
    let extra = if alerts.len() > 2 {
        format!("  + 另有 {} 条", alerts.len() - 2)
    } else {
        String::new()
    };
    let single_w = display_width(&prefix)
        + display_width(&first)
        + 5 // "  │  "
        + display_width(&second)
        + display_width(&extra);
    if single_w <= width as usize {
        1
    } else {
        2
    }
}

fn render_alert_strip(frame: &mut Frame, alerts: &[ActiveAlertItem], area: Rect) {
    if alerts.is_empty() || area.height == 0 || area.width == 0 {
        return;
    }
    let w = area.width as usize;
    let has_p0 = alerts.iter().any(|a| a.level == AlertLevel::P0);
    let banner_color = if has_p0 { STATUS_FAILED } else { STATUS_PENDING };
    let badge_style = Style::default()
        .fg(banner_color)
        .add_modifier(Modifier::REVERSED | Modifier::BOLD);

    let prefix_txt = format!(" ▲ 告警({}) ", alerts.len());
    let prefix_w = display_width(&prefix_txt);

    let format_item = |a: &ActiveAlertItem| -> (String, Color) {
        let c = if a.level == AlertLevel::P0 {
            STATUS_FAILED
        } else {
            STATUS_PENDING
        };
        (format!("[{}] {}", a.label, a.message), c)
    };

    if area.height == 1 {
        let mut spans = vec![Span::styled(prefix_txt, badge_style), Span::raw(" ")];
        let mut used = prefix_w + 1;
        if alerts.len() == 1 {
            let (txt, c) = format_item(&alerts[0]);
            spans.push(Span::styled(
                truncate(&txt, w.saturating_sub(used)),
                Style::default().fg(c).add_modifier(Modifier::BOLD),
            ));
        } else if area.width < NARROW_BREAKPOINT {
            let extra = format!(" +另有{}条", alerts.len() - 1);
            let extra_w = display_width(&extra);
            let (txt, c) = format_item(&alerts[0]);
            let avail = w.saturating_sub(used + extra_w);
            spans.push(Span::styled(
                truncate(&txt, avail),
                Style::default().fg(c).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(extra, muted()));
        } else {
            let (t1, c1) = format_item(&alerts[0]);
            let (t2, c2) = format_item(&alerts[1]);
            let extra = if alerts.len() > 2 {
                format!("  + 另有 {} 条", alerts.len() - 2)
            } else {
                String::new()
            };
            let sep = "  │  ";
            let need = display_width(&t1)
                + display_width(sep)
                + display_width(&t2)
                + display_width(&extra);
            if used + need <= w {
                spans.push(Span::styled(
                    t1,
                    Style::default().fg(c1).add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(sep, muted()));
                spans.push(Span::styled(
                    t2,
                    Style::default().fg(c2).add_modifier(Modifier::BOLD),
                ));
                if !extra.is_empty() {
                    spans.push(Span::styled(extra, muted()));
                }
            } else {
                let tail = format!("  + 另有 {} 条", alerts.len() - 1);
                let tail_w = display_width(&tail);
                spans.push(Span::styled(
                    truncate(&t1, w.saturating_sub(used + tail_w)),
                    Style::default().fg(c1).add_modifier(Modifier::BOLD),
                ));
                used += display_width(&t1).min(w.saturating_sub(used + tail_w));
                if used + tail_w <= w {
                    spans.push(Span::styled(tail, muted()));
                }
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    // 2-line wide alert strip: item 1 on line 1, item 2 (+ overflow count) on line 2.
    let (t1, c1) = format_item(&alerts[0]);
    let line1 = Line::from(vec![
        Span::styled(prefix_txt, badge_style),
        Span::raw(" "),
        Span::styled(
            truncate(&t1, w.saturating_sub(prefix_w + 1)),
            Style::default().fg(c1).add_modifier(Modifier::BOLD),
        ),
    ]);
    let (t2, c2) = format_item(&alerts[1]);
    let extra = if alerts.len() > 2 {
        format!("  + 另有 {} 条", alerts.len() - 2)
    } else {
        String::new()
    };
    let indent = " ".repeat(prefix_w + 1);
    let avail2 = w.saturating_sub(prefix_w + 1 + display_width(&extra));
    let mut spans2 = vec![
        Span::raw(indent),
        Span::styled(
            truncate(&t2, avail2),
            Style::default().fg(c2).add_modifier(Modifier::BOLD),
        ),
    ];
    if !extra.is_empty() {
        spans2.push(Span::styled(extra, muted()));
    }
    let line2 = Line::from(spans2);
    frame.render_widget(Paragraph::new(vec![line1, line2]), area);
}

pub(crate) fn muted() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub(crate) fn primary() -> Style {
    Style::default().fg(Color::Reset)
}

pub(crate) fn bold_primary() -> Style {
    Style::default()
        .fg(Color::Reset)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn rounded_block<'a>(title: Line<'a>, focused: bool) -> Block<'a> {
    let border_style = if focused { primary() } else { muted() };
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
}

/// Build the unified 5-tab strip (`1 总览 / 2 工作 / 3 运行 / 4 额度 / 5 事件`) used across all
/// 5 pages so the top bar never jumps or changes structure when switching views.
fn build_top_bar_tabs(app: &App, pulse: &SnapshotPulse, width: u16) -> Vec<Span<'static>> {
    let wide = width >= NARROW_BREAKPOINT;
    let active_style = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);

    let overview_label = {
        let base = if wide { "1 总览" } else { "1总览" };
        if pulse.p0_alert || pulse.owner_open > 0 {
            format!(" {base}(!) ")
        } else {
            format!(" {base} ")
        }
    };
    let overview_style = if app.tab == ViewTab::Overview {
        active_style
    } else if pulse.p0_alert {
        Style::default()
            .fg(STATUS_FAILED)
            .add_modifier(Modifier::BOLD)
    } else if pulse.owner_open > 0 {
        Style::default()
            .fg(STATUS_PENDING)
            .add_modifier(Modifier::BOLD)
    } else {
        muted()
    };

    let work_label = {
        let base = if wide { "2 工作" } else { "2工作" };
        if pulse.owner_open > 0 && pulse.work_active_pool > 0 {
            if wide {
                format!(" {base}(等你{}·推进{}) ", pulse.owner_open, pulse.work_active_pool)
            } else {
                format!(" {base}(等{}·{}) ", pulse.owner_open, pulse.work_active_pool)
            }
        } else if pulse.owner_open > 0 {
            format!(" {base}(等你{}) ", pulse.owner_open)
        } else if pulse.work_active_pool > 0 {
            format!(" {base}({}) ", pulse.work_active_pool)
        } else {
            format!(" {base} ")
        }
    };
    let work_style = if app.tab == ViewTab::Work {
        active_style
    } else if pulse.owner_open > 0 {
        Style::default()
            .fg(STATUS_PENDING)
            .add_modifier(Modifier::BOLD)
    } else {
        muted()
    };

    let runs_label = {
        let base = if wide { "3 运行" } else { "3运行" };
        let excl = if pulse.pending_has_urgent { "!" } else { "" };
        match (pulse.controller_pending > 0, pulse.running_runs > 0) {
            (true, true) => {
                if wide {
                    format!(
                        " {base}(待{}{excl}·跑{}) ",
                        pulse.controller_pending, pulse.running_runs
                    )
                } else {
                    format!(
                        " {base}({}{excl}/{}) ",
                        pulse.controller_pending, pulse.running_runs
                    )
                }
            }
            (true, false) => format!(" {base}(待{}{excl}) ", pulse.controller_pending),
            (false, true) => format!(" {base}(跑{}) ", pulse.running_runs),
            (false, false) => format!(" {base} "),
        }
    };
    let runs_style = if app.tab == ViewTab::Runs {
        active_style
    } else if pulse.pending_has_urgent {
        Style::default()
            .fg(STATUS_PENDING)
            .add_modifier(Modifier::BOLD)
    } else {
        muted()
    };

    let quota_label = {
        let base = if wide { "4 额度" } else { "4额度" };
        if pulse.quota_alert {
            format!(" {base}(!) ")
        } else {
            format!(" {base} ")
        }
    };
    let quota_style = if app.tab == ViewTab::Quota {
        active_style
    } else if pulse.quota_alert {
        Style::default()
            .fg(STATUS_PENDING)
            .add_modifier(Modifier::BOLD)
    } else {
        muted()
    };

    let events_label = if wide { " 5 事件 " } else { " 5事件 " };
    let events_style = if app.tab == ViewTab::Events {
        active_style
    } else {
        muted()
    };

    let mut spans = Vec::with_capacity(8);
    if wide {
        spans.push(Span::styled(" delegate ", active_style));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(overview_label, overview_style));
    spans.push(Span::styled(work_label, work_style));
    spans.push(Span::styled(runs_label, runs_style));
    spans.push(Span::styled(quota_label, quota_style));
    spans.push(Span::styled(events_label, events_style));
    spans
}

/// Active account short weekly summary (`★nov… 周41%` or `★auto`) for the fixed top bar.
fn top_bar_active_account_summary(state: &AgyAccountsState) -> Option<(String, Style)> {
    if state.auto_switch.enabled {
        let gem_cur = state
            .auto_switch
            .pool_current
            .iter()
            .find(|(k, _)| k == "gemini")
            .map(|(_, v)| v.as_str());
        let acct = gem_cur
            .and_then(|id| state.accounts.iter().find(|a| a.id == id))
            .or_else(|| state.accounts.iter().find(|a| a.active));
        let weekly_pct = acct
            .and_then(|a| a.quota.as_ref())
            .filter(|q| q.ok && !q.identity_mismatch)
            .and_then(|q| {
                q.entries
                    .iter()
                    .find(|e| e.window == "weekly" && e.pool.to_ascii_lowercase().contains("gemini"))
                    .or_else(|| q.entries.iter().find(|e| e.window == "weekly"))
            })
            .map(|e| (e.remaining_pct_x100 + 50) / 100);
        let txt = match weekly_pct {
            Some(pct) => format!("★auto 周{pct}%"),
            None => "★auto".to_string(),
        };
        return Some((txt, Style::default().fg(STATUS_RUNNING)));
    }
    let active = state.accounts.iter().find(|a| a.active)?;
    let short = mask_account_name(&active.id);
    let weekly_pct = active
        .quota
        .as_ref()
        .filter(|q| q.ok && !q.identity_mismatch)
        .and_then(|q| {
            q.entries
                .iter()
                .find(|e| e.window == "weekly" && e.pool.to_ascii_lowercase().contains("gemini"))
                .or_else(|| q.entries.iter().find(|e| e.window == "weekly"))
        })
        .map(|e| (e.remaining_pct_x100 + 50) / 100);
    let txt = match weekly_pct {
        Some(pct) => format!("★{short} 周{pct}%"),
        None => format!("★{short}"),
    };
    let style = match weekly_pct {
        Some(p) if p < 15 => Style::default()
            .fg(STATUS_FAILED)
            .add_modifier(Modifier::BOLD),
        Some(p) if p < 45 => Style::default().fg(STATUS_PENDING),
        _ => muted(),
    };
    Some((txt, style))
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
    let pulse = app.snapshot.effective_pulse();
    let mut spans = build_top_bar_tabs(app, &pulse, area.width);
    let ultra_compact = area.width < 92;

    let mut counts: Vec<(String, Style)> = Vec::new();
    if ultra_compact {
        counts.push((
            format!(
                "等{}·待{}·跑{}",
                pulse.owner_open, pulse.controller_pending, pulse.running_runs
            ),
            if pulse.owner_open > 0 || pulse.controller_pending > 0 {
                Style::default()
                    .fg(STATUS_PENDING)
                    .add_modifier(Modifier::BOLD)
            } else if pulse.running_runs > 0 {
                Style::default().fg(STATUS_RUNNING)
            } else {
                muted()
            },
        ));
    } else {
        // Ordered left-to-right by priority so rightmost items trim first under narrow widths:
        // 等你 -> 待主控 -> 在跑 -> 排队 -> 本波 -> ★账号周% -> 今日合入
        counts.push((
            format!("等你 {}", pulse.owner_open),
            if pulse.owner_open > 0 {
                Style::default()
                    .fg(STATUS_PENDING)
                    .add_modifier(Modifier::BOLD)
            } else {
                muted()
            },
        ));
        counts.push((
            format!("待主控 {}", pulse.controller_pending),
            if pulse.controller_pending > 0 {
                Style::default()
                    .fg(STATUS_PENDING)
                    .add_modifier(Modifier::BOLD)
            } else {
                muted()
            },
        ));
        counts.push((
            format!("在跑 {}", pulse.running_runs),
            if pulse.running_runs > 0 {
                Style::default()
                    .fg(STATUS_RUNNING)
                    .add_modifier(Modifier::BOLD)
            } else {
                muted()
            },
        ));
        counts.push((
            format!("排队 {}", pulse.queued_runs),
            if pulse.queued_runs > 0 {
                Style::default()
                    .fg(STATUS_QUEUED)
                    .add_modifier(Modifier::BOLD)
            } else {
                muted()
            },
        ));
        if let Some(ref clock) = app.snapshot.eta.wave.wave_p50_clock {
            counts.push((format!("本波 {clock}"), Style::default().fg(STATUS_QUEUED)));
        }
        if let Some(acct_item) = top_bar_active_account_summary(&app.snapshot.agy_accounts) {
            counts.push(acct_item);
        }
        counts.push((
            format!("今日合入 {}", pulse.merged_today),
            if pulse.merged_today > 0 {
                Style::default().fg(STATUS_RUNNING)
            } else {
                muted()
            },
        ));
    }

    let filter_badge = if !app.filter.is_empty() {
        let max_f = if area.width < NARROW_BREAKPOINT { 8 } else { 16 };
        Some(Span::styled(
            format!(" 滤:{} ", truncate(&app.filter, max_f)),
            Style::default()
                .fg(STATUS_PENDING)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ))
    } else {
        None
    };

    let sep = if ultra_compact { " " } else { " · " };
    append_fitting_counts(&mut spans, area.width as usize, counts, sep, filter_badge);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_runs_tab(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.width >= NARROW_BREAKPOINT {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
            .split(area);
        render_list_pane(frame, app, body[0]);
        render_detail_pane(frame, app, body[1]);
    } else {
        let (top_p, bot_p) = if area.height < 28 { (42, 58) } else { (45, 55) };
        let body = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(top_p), Constraint::Percentage(bot_p)])
            .split(area);
        render_list_pane(frame, app, body[0]);
        render_detail_pane(frame, app, body[1]);
    }
}

pub(crate) fn short_repo_display(path: &std::path::Path, fallback: &str) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

#[derive(Clone, Copy)]
struct ColWidths {
    show_state: bool,
    state_w: usize,
    repo_w: usize,
    name_w: usize,
    model_w: usize,
    time_w: usize,
    eta_w: usize,
    files_w: usize,
    bar_w: usize,
    protect_w: usize,
    accept_w: usize,
}

impl ColWidths {
    fn compute(
        width: usize,
        show_repo_col: bool,
        has_any_eta: bool,
        has_any_protect: bool,
        max_state_w: usize,
        narrow_screen: bool,
    ) -> Self {
        let show_state = width >= 46;
        let state_w = if show_state {
            max_state_w.clamp(6, 10)
        } else {
            0
        };
        let mut repo_w = if show_repo_col && width >= 54 {
            if width >= 76 { 10 } else { 8 }
        } else {
            0
        };
        let mut model_w = if narrow_screen || width < 58 { 0 } else { 7 };
        let mut time_w = if width >= 66 { 8 } else if width >= 54 { 6 } else { 0 };
        let mut eta_w = if has_any_eta && width >= 52 { 6 } else { 0 };
        let mut files_w = if !narrow_screen && width >= 60 { 4 } else { 0 };
        let mut bar_w = if !narrow_screen && width >= 66 { 4 } else { 0 };
        let mut protect_w = if has_any_protect && width >= 62 { 3 } else { 0 };
        let mut accept_w = if !narrow_screen && width >= 58 { 3 } else { 0 };

        let calc_left = |rw: usize| {
            2 + 2 + if show_state { state_w + 1 } else { 0 } + if rw > 0 { rw + 1 } else { 0 }
        };
        let calc_right = |mw: usize, tw: usize, ew: usize, fw: usize, bw: usize, pw: usize, aw: usize| {
            (if mw > 0 { 1 + mw } else { 0 })
                + (if tw > 0 { 1 + tw } else { 0 })
                + (if ew > 0 { 1 + ew } else { 0 })
                + (if fw > 0 { 1 + fw } else { 0 })
                + (if bw > 0 { 1 + bw } else { 0 })
                + (if pw > 0 { 1 + pw } else { 0 })
                + (if aw > 0 { 1 + aw } else { 0 })
        };

        let min_name_w = if width >= 40 { 16 } else { 10 };
        if width.saturating_sub(calc_left(repo_w) + calc_right(model_w, time_w, eta_w, files_w, bar_w, protect_w, accept_w)) < min_name_w {
            bar_w = 0;
        }
        if width.saturating_sub(calc_left(repo_w) + calc_right(model_w, time_w, eta_w, files_w, bar_w, protect_w, accept_w)) < min_name_w {
            protect_w = 0;
        }
        if width.saturating_sub(calc_left(repo_w) + calc_right(model_w, time_w, eta_w, files_w, bar_w, protect_w, accept_w)) < min_name_w {
            files_w = 0;
        }
        if width.saturating_sub(calc_left(repo_w) + calc_right(model_w, time_w, eta_w, files_w, bar_w, protect_w, accept_w)) < min_name_w {
            model_w = 0;
        }
        if width.saturating_sub(calc_left(repo_w) + calc_right(model_w, time_w, eta_w, files_w, bar_w, protect_w, accept_w)) < min_name_w {
            accept_w = 0;
        }
        if width.saturating_sub(calc_left(repo_w) + calc_right(model_w, time_w, eta_w, files_w, bar_w, protect_w, accept_w)) < min_name_w {
            time_w = 0;
        }
        if width.saturating_sub(calc_left(repo_w) + calc_right(model_w, time_w, eta_w, files_w, bar_w, protect_w, accept_w)) < min_name_w {
            eta_w = 0;
        }
        if width.saturating_sub(calc_left(repo_w) + calc_right(model_w, time_w, eta_w, files_w, bar_w, protect_w, accept_w)) < min_name_w {
            repo_w = 0;
        }

        let left = calc_left(repo_w);
        let right = calc_right(model_w, time_w, eta_w, files_w, bar_w, protect_w, accept_w);
        let name_w = width.saturating_sub(left + right).max(6);
        Self {
            show_state,
            state_w,
            repo_w,
            name_w,
            model_w,
            time_w,
            eta_w,
            files_w,
            bar_w,
            protect_w,
            accept_w,
        }
    }
}

fn render_list_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::List && matches!(app.overlay, Overlay::None);
    let pulse = app.snapshot.effective_pulse();
    let active_total = pulse.controller_pending + pulse.running_runs + pulse.queued_runs;
    let group_hint = match app.runs_group_mode {
        RunsGroupMode::ByStatus => "按状态聚合 · 按 v 切按仓",
        RunsGroupMode::ByRepo => "按仓库 · 按 v 切按状态",
    };
    let title_text = if app.filter.is_empty() {
        if area.width >= 56 {
            format!(" 运行列表 (活跃 {active_total} · {group_hint}) ")
        } else {
            format!(" 运行 ({active_total}) ")
        }
    } else {
        format!(" 运行列表 · 滤:{} ", truncate(&app.filter, 14))
    };
    let title = Line::from(Span::styled(
        truncate(&title_text, area.width.saturating_sub(2) as usize),
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
            format!("无匹配“{}”的运行", app.filter)
        } else if !app.snapshot.warnings.is_empty() {
            truncate(&app.snapshot.warnings[0], inner.width as usize)
        } else {
            "暂无委派运行".to_string()
        };
        let line = Line::from(Span::styled(
            format!(" {}", truncate(&msg, inner.width.saturating_sub(1) as usize)),
            muted(),
        ));
        frame.render_widget(Paragraph::new(line), inner);
        return;
    }

    let inner_w = inner.width as usize;
    let show_repo_col = app.runs_group_mode == RunsGroupMode::ByStatus;
    let narrow_screen = app.last_terminal_width < NARROW_BREAKPOINT;
    let has_any_eta = app.rows.iter().any(|row| {
        if let ListRow::Run { repo_idx, run_idx } = row {
            app.snapshot.repos[*repo_idx].runs[*run_idx]
                .eta_badge()
                .is_some()
        } else {
            false
        }
    });
    let has_any_protect = app.rows.iter().any(|row| {
        if let ListRow::Run { repo_idx, run_idx } = row {
            let r = &app.snapshot.repos[*repo_idx].runs[*run_idx];
            r.source_write_violation
                || !r.protect_violation.is_empty()
                || !r.prompt_protected_paths.is_empty()
        } else {
            false
        }
    });
    let max_state_w = app
        .rows
        .iter()
        .filter_map(|row| match row {
            ListRow::Run { repo_idx, run_idx } => Some(display_width(
                &app.snapshot.repos[*repo_idx].runs[*run_idx].state_badge(),
            )),
            _ => None,
        })
        .max()
        .unwrap_or(6);
    let cols = ColWidths::compute(
        inner_w,
        show_repo_col,
        has_any_eta,
        has_any_protect,
        max_state_w,
        narrow_screen,
    );
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
                let repo = &app.snapshot.repos[repo_idx];
                let repo_total = app
                    .snapshot
                    .line_stats
                    .as_ref()
                    .and_then(|ls| ls.total_lines.as_ref())
                    .and_then(|tl| {
                        tl.by_repo
                            .iter()
                            .find(|r| !r.path.is_empty() && r.path == repo.path)
                            .or_else(|| tl.by_repo.iter().find(|r| r.repo == repo.label))
                            .map(|r| r.counts.total)
                    });
                render_repo_header(repo, repo_total, inner_w)
            }
            ListRow::GroupHeader {
                repo_idx,
                group,
                count,
            } => render_group_header(group, count, repo_idx == usize::MAX, inner_w),
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
            ListRow::MergeEntry { idx } => {
                render_merge_fold_entry(&app.snapshot.merges[idx], selected, inner_w)
            }
        };
        lines.push(line);
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_merge_fold_entry(m: &MergeRunItem, selected: bool, width: usize) -> Line<'static> {
    let (badge, color) = if m.applied {
        ("已合回", Some(STATUS_RUNNING))
    } else if !m.merge_accept_ok && !m.merge_accept_skipped {
        ("验✗", Some(STATUS_FAILED))
    } else {
        ("待合回", Some(STATUS_PENDING))
    };
    let text = format!(
        "[{}] {} · {} ({}合/{}跳)",
        badge, m.repo_label, m.name, m.merged_count, m.skipped_count
    );
    if selected {
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        let accent = color.map(|c| Style::default().fg(c).add_modifier(Modifier::BOLD)).unwrap_or_else(bold_primary);
        let clipped = truncate(&text, width.saturating_sub(2));
        let pad = width.saturating_sub(2 + display_width(&clipped));
        Line::from(vec![
            Span::styled("▎", accent),
            Span::styled(" ", rev),
            Span::styled(clipped, rev.add_modifier(Modifier::BOLD)),
            Span::styled(" ".repeat(pad), rev),
        ])
    } else {
        let style = color.map(|c| Style::default().fg(c)).unwrap_or_else(primary);
        Line::from(Span::styled(truncate(&format!("   {text}"), width), style))
    }
}

fn render_col_header(cols: ColWidths, width: usize) -> Line<'static> {
    let mut s = String::new();
    s.push_str("    ");
    if cols.show_state {
        s.push_str(&pad_right("状态", cols.state_w));
        s.push(' ');
    }
    if cols.repo_w > 0 {
        s.push_str(&pad_right("仓库", cols.repo_w));
        s.push(' ');
    }
    s.push_str(&pad_right("任务", cols.name_w));
    if cols.model_w > 0 {
        s.push(' ');
        s.push_str(&pad_right("模型", cols.model_w));
    }
    if cols.time_w > 0 {
        s.push(' ');
        s.push_str(&pad_left("时间", cols.time_w));
    }
    if cols.eta_w > 0 {
        s.push(' ');
        s.push_str(&pad_left("预估", cols.eta_w));
    }
    if cols.files_w > 0 {
        s.push(' ');
        let diff_w = cols.files_w + if cols.bar_w > 0 { 1 + cols.bar_w } else { 0 };
        s.push_str(&pad_left("改动", diff_w));
    }
    if cols.protect_w > 0 {
        s.push(' ');
        s.push_str(&pad_right("护", cols.protect_w));
    }
    if cols.accept_w > 0 {
        s.push(' ');
        s.push_str(&pad_right("验", cols.accept_w));
    }
    Line::from(Span::styled(truncate(&s, width), muted()))
}

fn render_repo_header(repo: &RepoGroup, total_lines: Option<u64>, width: usize) -> Line<'static> {
    let counts = format!(
        "待{} 运{} 排{} 合{}",
        repo.count_group(GroupKind::Pending),
        repo.count_group(GroupKind::Running),
        repo.count_group(GroupKind::Queued),
        repo.count_group(GroupKind::MergedUncleaned)
    );
    let counts_w = display_width(&counts);
    let prefix = "▌ ";
    let lines_note = total_lines
        .filter(|&n| n > 0)
        .map(|n| format!(" · 总 {}", format_lines_compact(n)))
        .unwrap_or_default();
    let note_w = display_width(&lines_note);
    let avail_without_note = width.saturating_sub(display_width(prefix) + counts_w + 2);
    let show_note = note_w > 0 && avail_without_note > note_w + 4;
    let used_note = if show_note { lines_note } else { String::new() };
    let used_note_w = display_width(&used_note);
    let avail_label = avail_without_note.saturating_sub(used_note_w);
    let label = truncate(&repo.label, avail_label);
    let gap = width.saturating_sub(
        display_width(prefix) + display_width(&label) + used_note_w + counts_w,
    );
    let mut spans = vec![
        Span::styled(prefix, muted()),
        Span::styled(label, bold_primary()),
    ];
    if !used_note.is_empty() {
        spans.push(Span::styled(used_note, muted()));
    }
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(Span::styled(counts, muted()));
    Line::from(spans)
}

fn render_group_header(group: GroupKind, count: usize, cross_repo: bool, width: usize) -> Line<'static> {
    let style = match group_status_color(group) {
        Some(c) => Style::default().fg(c).add_modifier(Modifier::BOLD),
        None => muted().add_modifier(Modifier::BOLD),
    };
    let name = if cross_repo {
        match group {
            GroupKind::Pending => "待主控处理",
            GroupKind::Running => "运行与返工",
            GroupKind::Queued => "排队与待续派",
            _ => group.label(),
        }
    } else {
        group.label()
    };
    let text = format!(" ▾ {name} ({count})");
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
        .filter(|&s| s != u64::MAX)
        .map(|s| format!("（最新 {}）", format_relative_time(Some(s))))
        .unwrap_or_default();
    let label = match kind {
        CollapseKind::StalePending => {
            if standalone {
                format!("{arrow} 待主控 · 更早陈旧 {count} 条{ago}")
            } else {
                format!("  {arrow} 更早陈旧待处理 {count} 条{ago}")
            }
        }
        CollapseKind::Merges => format!("{arrow} 合入批次 {count} 条"),
        CollapseKind::Collected => format!("{arrow} 近期已收取 {count} 条{ago}"),
        CollapseKind::MergedUncleaned => format!("{arrow} 已合未清 {count} 条{ago}"),
        CollapseKind::Stopped => format!("{arrow} 已停止 {count} 条{ago}"),
        CollapseKind::DispatchedQueue => format!("{arrow} 已派队列 {count} 条{ago}"),
        CollapseKind::StaleRepos => format!("{arrow} 陈旧仓库 {count} 个（超过 7 天无动静）"),
    };

    if selected {
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        let hint = if expanded { "Space 折叠 " } else { "Space 展开 " };
        let hint_w = display_width(hint);
        let accent_style = match kind {
            CollapseKind::StalePending | CollapseKind::Merges => Style::default()
                .fg(STATUS_PENDING)
                .add_modifier(Modifier::BOLD),
            CollapseKind::Collected
            | CollapseKind::MergedUncleaned
            | CollapseKind::Stopped
            | CollapseKind::DispatchedQueue
            | CollapseKind::StaleRepos => bold_primary(),
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
        CollapseKind::Merges => Style::default().fg(STATUS_QUEUED),
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

    let active_remote_snippet = run.remote_exec.as_ref().and_then(|re| {
        if !re.active {
            return None;
        }
        Some(if re.running {
            let cmd_s = re
                .cmd
                .as_deref()
                .map(shorten_running_cmd)
                .filter(|s| !s.is_empty());
            let dur_s = re.elapsed_seconds.map(|s| format_duration(Some(s)));
            match (cmd_s, dur_s) {
                (Some(c), Some(d)) => format!(" [@{} {c} {d}]", re.host),
                (Some(c), None) => format!(" [@{} {c}]", re.host),
                (None, Some(d)) => format!(" [@{} {d}]", re.host),
                (None, None) => format!(" [@{}]", re.host),
            }
        } else {
            format!(" [@{} 排队]", re.host)
        })
    });
    let idle_remote_tag = run.remote_exec.as_ref().and_then(|re| {
        (!re.active).then(|| format!(" [@{}]", re.host))
    });

    let round_suffix = if run.chain_round > 1 {
        format!("（第{}轮）", run.chain_round)
    } else {
        String::new()
    };

    let full_name = if matches!(run.group, GroupKind::Running | GroupKind::Queued) {
        let mut snippet = if let Some(ref rs) = active_remote_snippet {
            rs.clone()
        } else if !review_snippet.is_empty() {
            review_snippet
        } else if let Some(act) = run.activity.as_ref() {
            match act.kind.as_str() {
                "running_command" => {
                    if let Some(cmd) = &act.cmd {
                        let short = shorten_running_cmd(cmd);
                        if short.is_empty() {
                            String::new()
                        } else {
                            format!(" [{short}]")
                        }
                    } else {
                        String::new()
                    }
                }
                "quota" | "error" => format!(" [{}]", act.label),
                _ if run.stall_warning.is_some() => " [卡住提醒]".to_string(),
                _ => String::new(),
            }
        } else {
            String::new()
        };
        if active_remote_snippet.is_none() {
            if let Some(ref tag) = idle_remote_tag {
                if snippet.is_empty() {
                    snippet = tag.clone();
                } else if cols.name_w >= display_width(&snippet) + display_width(tag) + 10 {
                    snippet.push_str(tag);
                }
            }
        }
        if !snippet.is_empty() && cols.name_w > 10 {
            let base = format!("{}{round_suffix}", run.display_name);
            let snip_w = display_width(&snippet);
            if cols.name_w >= snip_w + 8 {
                format!("{}{snippet}", truncate(&base, cols.name_w - snip_w))
            } else {
                truncate(&format!("{base}{snippet}"), cols.name_w)
            }
        } else if !round_suffix.is_empty() {
            let sw = display_width(&round_suffix);
            if cols.name_w > sw + 2 {
                format!(
                    "{}{round_suffix}",
                    truncate(&run.display_name, cols.name_w - sw)
                )
            } else {
                truncate(&run.display_name, cols.name_w)
            }
        } else {
            truncate(&run.display_name, cols.name_w)
        }
    } else if !row_tag_snippet.is_empty() && cols.name_w > 10 {
        let base = format!("{}{round_suffix}", run.display_name);
        let tag_w = display_width(&row_tag_snippet);
        if cols.name_w >= tag_w + 8 {
            let name_avail = cols.name_w - tag_w;
            format!("{}{row_tag_snippet}", truncate(&base, name_avail))
        } else {
            truncate(&format!("{base}{row_tag_snippet}"), cols.name_w)
        }
    } else if !round_suffix.is_empty() {
        let sw = display_width(&round_suffix);
        if cols.name_w > sw + 2 {
            format!(
                "{}{round_suffix}",
                truncate(&run.display_name, cols.name_w - sw)
            )
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
            spans.push(Span::styled(
                format!("{} ", pad_right(&run.state_badge(), cols.state_w)),
                rev,
            ));
        }
        if cols.repo_w > 0 {
            let short_repo = short_repo_display(&run.repo_path, &run.repo_label);
            spans.push(Span::styled(
                format!("{} ", pad_right(&truncate(&short_repo, cols.repo_w), cols.repo_w)),
                rev,
            ));
        }
        spans.push(Span::styled(padded_name, rev_bold));
        if cols.model_w > 0 {
            spans.push(Span::styled(" ", rev));
            spans.push(Span::styled(
                pad_right(&format_model_cell(run, cols.model_w), cols.model_w),
                rev,
            ));
        }
        if cols.time_w > 0 {
            spans.push(Span::styled(" ", rev));
            spans.push(Span::styled(pad_left(&time_text, cols.time_w), rev));
        }
        if cols.eta_w > 0 {
            let eta_text = run
                .eta_badge()
                .map(|(b, _)| truncate(b, cols.eta_w))
                .unwrap_or_else(|| "-".to_string());
            spans.push(Span::styled(" ", rev));
            spans.push(Span::styled(pad_left(&eta_text, cols.eta_w), rev));
        }
        if cols.files_w > 0 {
            spans.push(Span::styled(" ", rev));
            spans.push(Span::styled(pad_left(&files_text, cols.files_w), rev));
        }
        if cols.bar_w > 0 {
            spans.push(Span::styled(" ", rev));
            let filled = add_n + del_n + neu_n;
            let bar = format!("{}{}", "▪".repeat(filled), "·".repeat(empty_n));
            spans.push(Span::styled(bar, rev));
        }
        if cols.protect_w > 0 {
            spans.push(Span::styled(" ", rev));
            if let Some((p_text, _, _)) = protect_info {
                spans.push(Span::styled(pad_right(p_text, cols.protect_w), rev_bold));
            } else {
                spans.push(Span::styled(" ".repeat(cols.protect_w), rev));
            }
        }
        if cols.accept_w > 0 {
            spans.push(Span::styled(" ", rev));
            spans.push(Span::styled(pad_right(accept_badge, cols.accept_w), rev));
        }

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
            format!("{} ", pad_right(&run.state_badge(), cols.state_w)),
            state_style,
        ));
    }
    if cols.repo_w > 0 {
        let short_repo = short_repo_display(&run.repo_path, &run.repo_label);
        spans.push(Span::styled(
            format!("{} ", pad_right(&truncate(&short_repo, cols.repo_w), cols.repo_w)),
            muted(),
        ));
    }
    spans.push(Span::styled(padded_name, name_style));
    if cols.model_w > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            pad_right(&format_model_cell(run, cols.model_w), cols.model_w),
            muted(),
        ));
    }
    if cols.time_w > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(pad_left(&time_text, cols.time_w), muted()));
    }
    if cols.eta_w > 0 {
        let (eta_text, eta_style) = match run.eta_badge() {
            Some((b, true)) => {
                let severe = run
                    .eta
                    .as_ref()
                    .and_then(|e| e.blocker_kind.as_deref())
                    .is_some_and(|k| {
                        matches!(
                            k,
                            "premerge_conflict"
                                | "premerge_failed"
                                | "mainline_blocked"
                                | "stall_warning"
                        )
                    });
                let col = if severe { STATUS_FAILED } else { STATUS_PENDING };
                (
                    truncate(b, cols.eta_w),
                    Style::default().fg(col).add_modifier(Modifier::BOLD),
                )
            }
            Some((b, false)) => (truncate(b, cols.eta_w), Style::default().fg(STATUS_QUEUED)),
            None => ("-".to_string(), muted()),
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(pad_left(&eta_text, cols.eta_w), eta_style));
    }
    if cols.files_w > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            pad_left(&files_text, cols.files_w),
            if files > 0 { primary() } else { muted() },
        ));
    }
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
    if cols.protect_w > 0 {
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
    }
    if cols.accept_w > 0 {
        spans.push(Span::raw(" "));
        let acc_style = match accept_color {
            Some(c) => Style::default().fg(c),
            None => muted(),
        };
        spans.push(Span::styled(pad_right(accept_badge, cols.accept_w), acc_style));
    }

    Line::from(spans)
}

fn render_detail_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::Detail && matches!(app.overlay, Overlay::None);
    if let Some(m) = app.selected_merge_row().cloned() {
        render_merge_detail_item(frame, app, &m, area, focused);
        return;
    }
    if let Some(q) = app.selected_dag_queue().cloned() {
        render_dag_queue_detail_pane(frame, app, &q, area, focused);
        return;
    }
    let Some((repo_idx, run_idx)) = app.preview_run_indices() else {
        let block = rounded_block(Line::from(Span::styled(" 审查卡详情 ", muted())), focused);
        frame.render_widget(block, area);
        return;
    };
    let run = app.snapshot.repos[repo_idx].runs[run_idx].clone();
    let card_l1 = app.get_or_load_run_card(repo_idx, run_idx).cloned();

    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let lines = build_detail_lines(app, &run, card_l1.as_ref(), inner_w);
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
    let card_tag = if card_l1.is_some()
        || (run.card_l0.is_some() && run.group != GroupKind::Running)
    {
        " [L1 审查卡 · 按 d 看 Diff]"
    } else {
        ""
    };
    let max_title_w = inner_w.saturating_sub(display_width(&scroll_hint) + 4);
    let title_core = truncate(
        &format!("{}{round_tag}{card_tag}", run.display_name),
        max_title_w,
    );

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

fn build_detail_lines(
    app: &App,
    run: &RunItem,
    card_l1: Option<&RunCardL1>,
    width: usize,
) -> Vec<Line<'static>> {
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
    if !run.applied {
        if let Some(eta) = &run.eta {
            if eta.stage != "done" && !eta.detail_text.is_empty() {
                let eta_style = if eta.stopped_clock {
                    let severe = eta.blocker_kind.as_deref().is_some_and(|k| {
                        matches!(
                            k,
                            "premerge_conflict"
                                | "premerge_failed"
                                | "mainline_blocked"
                                | "stall_warning"
                        )
                    });
                    Style::default()
                        .fg(if severe { STATUS_FAILED } else { STATUS_PENDING })
                        .add_modifier(Modifier::BOLD)
                } else if eta.anomaly {
                    Style::default().fg(STATUS_PENDING)
                } else {
                    Style::default().fg(STATUS_QUEUED)
                };
                push_wrapped(&mut out, &eta.detail_text, width, eta_style);
            }
        }
    }

    let tier_part = run
        .tier
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(|t| format!(" · {t}"))
        .unwrap_or_default();
    let mode_part = if run.mode.is_empty() || run.tier.as_deref() == Some(run.mode.as_str()) {
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
    if run.appended_total > 0 {
        timing_parts.push(format!(
            "已追加 {} 条，待投递 {} 条",
            run.appended_total, run.appended_pending
        ));
    } else if run.pending_reply {
        timing_parts.push("有待续追问".to_string());
    }
    if !timing_parts.is_empty() {
        push_wrapped(&mut out, &timing_parts.join(" · "), width, muted());
    }

    // Three-gate horizontal bar (验收 │ 审查 │ 预合并)
    let accept_part = match &run.accept {
        AcceptStatus::Passed { .. } => {
            if let Some(c) = card_l1.filter(|c| c.accept_layers_total > 0) {
                format!("验收 ✓ ({}/{}步)", c.accept_layers_passed, c.accept_layers_total)
            } else {
                "验收 ✓".to_string()
            }
        }
        AcceptStatus::Failed { exit_code, .. } => match exit_code {
            Some(code) => format!("验收 ✗(exit={code})"),
            None => "验收 ✗".to_string(),
        },
        AcceptStatus::NotExecuted { .. } => "验收 未执行".to_string(),
        AcceptStatus::None => "验收 -".to_string(),
    };
    let review_part = run
        .review
        .as_ref()
        .map(ReviewInfo::detail_label)
        .filter(|s| !s.is_empty())
        .map(|s| format!("审查 {s}"))
        .unwrap_or_else(|| "审查 -".to_string());
    let premerge_part = run
        .premerge
        .as_ref()
        .map(PremergeInfo::full_label)
        .filter(|s| !s.is_empty())
        .map(|s| format!("预合并 {s}"))
        .unwrap_or_else(|| "预合并 -".to_string());
    push_wrapped(
        &mut out,
        &format!("门禁: {accept_part}  │  {review_part}  │  {premerge_part}"),
        width,
        primary(),
    );

    // L1 Review Card risks & reviewer focus (loaded on demand from `<runsDir>/<run>/card.json`,
    // with graceful fallback to L0 `status["card"]`).
    if let Some(card) = card_l1 {
        out.push(Line::raw(""));
        let test_delta = if card.tests_added > 0 || card.tests_modified > 0 || card.tests_deleted > 0
        {
            format!(
                " · 测试 +{}/~{}/-{}",
                card.tests_added, card.tests_modified, card.tests_deleted
            )
        } else {
            String::new()
        };
        let risk_hdr = if card.risks.is_empty() {
            format!("审查卡风险与焦点 (L1{test_delta})")
        } else {
            format!(
                "审查卡风险与焦点 (L1 · 命中 {} 项风险{test_delta})",
                card.risks.len()
            )
        };
        out.push(section_header(&risk_hdr));
        if card.risks.is_empty() {
            push_indented_wrapped(
                &mut out,
                "  ",
                "✓ 0 风险项（规则与审查员均未检出高风险改动）",
                width,
                Style::default().fg(STATUS_RUNNING),
            );
        } else {
            for r in &card.risks {
                let sev_low = r.severity.to_ascii_lowercase();
                let sev_zh = match sev_low.as_str() {
                    "critical" => "严重",
                    "high" => "高",
                    "medium" => "中",
                    "low" => "低",
                    other if !other.is_empty() => other,
                    _ => "风险",
                };
                let sev_color = match sev_low.as_str() {
                    "critical" | "high" => STATUS_FAILED,
                    "medium" => STATUS_PENDING,
                    _ => STATUS_QUEUED,
                };
                let loc = match r.line {
                    Some(l) if !r.path.is_empty() => format!("{}:{l}", r.path),
                    _ => r.path.clone(),
                };
                let delta = if r.added > 0 || r.deleted > 0 {
                    format!(" (+{}/-{})", r.added, r.deleted)
                } else {
                    String::new()
                };
                let cat = if r.category.is_empty() {
                    sev_zh.to_string()
                } else {
                    format!("{sev_zh} · {}", r.category)
                };
                push_indented_wrapped(
                    &mut out,
                    "  ",
                    &format!("[{cat}] {loc}{delta}"),
                    width,
                    Style::default()
                        .fg(sev_color)
                        .add_modifier(Modifier::BOLD),
                );
                if !r.reason.is_empty() {
                    push_indented_wrapped(&mut out, "    └ ", &r.reason, width, muted());
                }
            }
        }
        if !card.review_summary.is_empty() {
            let round_prefix = if card.review_round > 0 {
                format!("Reviewer 第{}轮结论: ", card.review_round)
            } else {
                "Reviewer 结论: ".to_string()
            };
            push_indented_wrapped(
                &mut out,
                "  ",
                &format!("{round_prefix}{}", card.review_summary),
                width,
                primary(),
            );
        }
    } else if let Some(l0) = &run.card_l0 {
        out.push(Line::raw(""));
        out.push(section_header("审查卡摘要 (L0)"));
        if l0.risk_count == 0 {
            push_indented_wrapped(
                &mut out,
                "  ",
                "✓ 0 风险项",
                width,
                Style::default().fg(STATUS_RUNNING),
            );
        } else {
            let sev_part = l0
                .max_severity
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| format!("（最高 {s}）"))
                .unwrap_or_default();
            let sum_part = if l0.summary.is_empty() {
                String::new()
            } else {
                format!(" · {}", l0.summary)
            };
            push_indented_wrapped(
                &mut out,
                "  ",
                &format!("命中 {} 项风险{sev_part}{sum_part}", l0.risk_count),
                width,
                Style::default().fg(STATUS_PENDING),
            );
        }
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
    if let Some(sw) = &run.agy_account_switch {
        push_wrapped(
            &mut out,
            &sw.format_line(false),
            width,
            Style::default().fg(STATUS_PENDING),
        );
    }
    if let Some(sw) = &run.review_agy_account_switch {
        push_wrapped(
            &mut out,
            &sw.format_line(true),
            width,
            Style::default().fg(STATUS_PENDING),
        );
    }

    // Remote execution details & output tail (no ssh required)
    if let Some(re) = &run.remote_exec {
        let queue_zh = match re.queue.as_deref() {
            Some("remote-light") => "轻队列",
            Some("remote-heavy") => "重队列",
            _ => "远端",
        };
        let header_line = if re.active {
            let st_zh = if re.running { "运行中" } else { "排队中" };
            let dur_s = re
                .elapsed_seconds
                .map(|s| format!(" · 已跑 {}", format_duration_zh(Some(s))))
                .unwrap_or_default();
            format!("远端执行: @{} · {queue_zh} · {st_zh}{dur_s}", re.host)
        } else {
            format!("远端机器: @{}", re.host)
        };
        let re_style = if re.active && re.running {
            Style::default()
                .fg(STATUS_RUNNING)
                .add_modifier(Modifier::BOLD)
        } else if re.active {
            Style::default()
                .fg(STATUS_QUEUED)
                .add_modifier(Modifier::BOLD)
        } else {
            muted()
        };
        push_wrapped(&mut out, &header_line, width, re_style);
        if let Some(cmd) = &re.cmd {
            push_indented_wrapped(&mut out, "  命令: ", cmd, width, primary());
        }
        if !re.output_tail.is_empty() {
            out.push(Line::raw(""));
            out.push(section_header(&format!(
                "远端输出尾部 (最近 {} 行)",
                re.output_tail.len()
            )));
            for line in &re.output_tail {
                push_indented_wrapped(&mut out, "  ", line, width, muted());
            }
        }
    }

    // Reply / rework / append rounds (aligned with `delegate card` each-round original pointers)
    if run.chain_rounds.len() > 1 {
        let total_rounds = run.chain_rounds.len().max(run.chain_round);
        let multi_run = run
            .chain_rounds
            .iter()
            .map(|r| r.run.as_str())
            .filter(|id| !id.is_empty())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            > 1;
        out.push(Line::raw(""));
        out.push(section_header(&format!(
            "会话轮次与各轮原文 (共 {total_rounds} 轮)"
        )));
        for r in &run.chain_rounds {
            let kind_zh = match r.kind.as_str() {
                "rework" => "返工",
                "append" => "追加",
                "reply" => "reply",
                _ => "初稿",
            };
            let st_zh = if r.question {
                "提问"
            } else {
                match r.state.as_str() {
                    "running" => "运行中",
                    "scouting" => "探路中",
                    "waiting" => "等待中",
                    "starting" => "启动中",
                    "delivered" => "已交付",
                    "answered" => "已答复",
                    "rejected" => "未通过",
                    "stopped" => "已停止",
                    "failed" => "失败",
                    "archived" => "已归档",
                    _ => "",
                }
            };
            let mut tags = Vec::new();
            if multi_run && !r.run.is_empty() {
                tags.push(r.run.clone());
            }
            tags.push(kind_zh.to_string());
            if !st_zh.is_empty() {
                tags.push(st_zh.to_string());
            }
            let tag_str = if r.current {
                format!("{}，当前", tags.join(" · "))
            } else {
                tags.join(" · ")
            };
            let dur = r
                .elapsed_seconds
                .map(|s| format!(" · {}", format_duration_zh(Some(s))))
                .unwrap_or_default();
            let header = format!("第 {} 轮（{tag_str}）{dur}", r.round);
            let style = if r.current { primary() } else { muted() };
            push_indented_wrapped(&mut out, "  - ", &header, width, style);
            if let Some(ref p) = r.path {
                let stats = match (r.total_lines, r.total_chars) {
                    (Some(l), Some(c)) => format!("（共 {l} 行 / {c} 字）"),
                    (None, Some(c)) => format!("（共 {c} 字）"),
                    _ => String::new(),
                };
                push_indented_wrapped(&mut out, "    └ ", &format!("{p}{stats}"), width, muted());
            }
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

    // Acceptance (only expand when there is command/failure detail beyond the `门禁:` bar)
    match &run.accept {
        AcceptStatus::Passed { command } if !command.is_empty() => {
            push_wrapped(
                &mut out,
                &format!("验收 ✓ {command}"),
                width,
                Style::default().fg(STATUS_RUNNING),
            );
        }
        AcceptStatus::Passed { .. } => {}
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

    // Review status (only expand when actionable or carrying extra detail beyond `门禁:`)
    if let Some(rev) = &run.review {
        let label = rev.detail_label();
        let has_extra = rev.upgraded_by.is_some()
            || !rev.controller_paths.is_empty()
            || rev.sign_hash.is_some()
            || (rev.needs_attention && rev.verdict.as_deref() != Some("PASS"));
        let is_plain_pass = rev.verdict.as_deref() == Some("PASS")
            && rev.status != "reviewing"
            && rev.status != "reworking"
            && !has_extra;
        if !label.is_empty() && !is_plain_pass {
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

    // Premerge / fast-forward status (only expand on stale/conflict/failed; ready/running is already in `门禁:`)
    if !run.applied {
        if let Some(pm) = &run.premerge {
            let lbl = pm.full_label();
            if !lbl.is_empty() && matches!(pm.status.as_str(), "stale" | "conflict" | "failed") {
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

pub(crate) fn section_header(title: &str) -> Line<'static> {
    Line::from(Span::styled(format!("▸ {title}"), bold_primary()))
}

pub(crate) fn push_wrapped(out: &mut Vec<Line<'static>>, text: &str, width: usize, style: Style) {
    for line in wrap_text(text, width.max(4)) {
        out.push(Line::from(Span::styled(line, style)));
    }
}

pub(crate) fn push_indented_wrapped(
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
        if area.width >= 115 {
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
        // Bottom bar lists only keys that actually act on the current page.
        let keys = match app.tab {
            ViewTab::Overview => {
                if area.width >= NARROW_BREAKPOINT {
                    " 1-5/[] 切页  jk 选择  Tab 切区  Enter 跳详情/看结论  Space 折叠  Q 刷额度  r 刷新  ? 帮助  q 退出"
                } else {
                    " 1-5切页 jk滚 Tab切区 Enter下钻 Q刷额度 r刷 ?助 q退"
                }
            }
            ViewTab::Work => {
                if area.width >= NARROW_BREAKPOINT {
                    " 1-5 切页  jk 选  Space 折叠  Tab 滚右卡  t 跳运行  o 等你  x 已否  p 挑候选  / 过滤  y 复制  ? 帮助  q 退出"
                } else if area.width >= COMPACT_BREAKPOINT {
                    " 1-5切页 jk选 Enter卡 Space折 t跳运行 o等你 x已否 /滤 y复 ?助 q退"
                } else {
                    " ↑↓选 Enter卡 t运行 o等你 x已否 1-5页 q退"
                }
            }
            ViewTab::Runs => {
                if area.width >= NARROW_BREAKPOINT {
                    " 1-5 切页  jk 选  Tab 切栏  v 按态/按仓  Enter 全文  d diff  w 跳工作  a/c/s/p 命令  y 复制  ? 帮助  q 退出"
                } else {
                    " 1-5切页 jk选 Tab栏 v分组 Enter文 d diff w工作 a/c/s/p ?助 q退"
                }
            }
            ViewTab::Quota => {
                if area.width >= NARROW_BREAKPOINT {
                    " 1-5 切页  jk 选账号  u 手动切号  i 显隐全称  Space 展开模型  Q 刷新额度  y 复制  Tab 切栏  ? 帮助  q 退出"
                } else {
                    " 1-5切页 jk选 u切号 i全称 Space模型 Q刷额度 Tab栏 ?助 q退"
                }
            }
            ViewTab::Events => {
                if area.width >= NARROW_BREAKPOINT {
                    " 1-5 切页  jk 选  f 跟随  l 显隐槽位  t/Enter 跳运行  Tab 切栏  / 过滤  r 刷新  ? 帮助  q 退出"
                } else {
                    " 1-5切页 jk选 f跟随 l槽位 t/Enter运行 Tab栏 /滤 ?助 q退"
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
    let w = area.width.saturating_sub(6).clamp(44, 78);
    let h = 19u16.min(area.height.saturating_sub(2));
    let modal = centered_rect(area, w, h);
    frame.render_widget(Clear, modal);

    let block = rounded_block(
        Line::from(Span::styled(" 快捷键（5 标签统一键位） ", bold_primary())),
        true,
    )
    .title_bottom(Line::from(Span::styled(" Esc/q 关闭 ", muted())));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let rows = [
        ("1–5 / [ ]", "切换 5 个标签页（总览 / 工作 / 运行 / 额度 / 事件）"),
        ("↑↓ / j k / g G", "移动选择 / 滚动焦点区 / 跳顶跳底"),
        ("Tab / Shift+Tab", "切换焦点栏（总览在 4 张交互卡间循环）"),
        ("Space", "展开 / 收起折叠组（额度页展开全部模型）"),
        ("Enter", "纯浏览下钻：跳详情 / 开全文（绝不切号或写盘）"),
        ("/ / Esc", "过滤 / 清除过滤或关闭浮层"),
        ("y / r / Q", "复制推荐命令 / 刷新快照 / 刷新额度"),
        ("t / w", "跳关联运行（工作/事件页） / 跳关联工作（运行页）"),
        ("o / x / p", "工作页：仅看等你 / 仅看已否 / 挑候选草稿"),
        ("v / d", "运行页：切按状态·按仓库分组 / 打开 Diff 浮层"),
        ("a / c / s / p", "运行页：apply / clean / stop / reply 命令弹窗"),
        ("u / i", "额度页：手动切号确认（绝不自动换号） / 显隐全称"),
        ("f / l", "事件页：开关跟随最新 / 显隐槽位事件"),
        ("? / q / Ctrl-C", "帮助 / 关闭浮层或退出"),
    ];
    let iw = inner.width as usize;
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, desc)| {
            Line::from(vec![
                Span::styled(pad_right(k, 17), bold_primary()),
                Span::raw(" "),
                Span::styled(truncate(desc, iw.saturating_sub(18)), muted()),
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

pub(crate) fn run_glyph_color(run: &RunItem) -> (&'static str, Option<Color>) {
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

fn short_event_time(at: &str, today_ymd: &str) -> String {
    if let Some(loc) = format_iso_local_short(at, today_ymd) {
        return loc;
    }
    if at.len() >= 16 {
        at[11..16].to_string()
    } else {
        at.to_string()
    }
}

fn render_events_tab(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.width >= NARROW_BREAKPOINT {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        render_events_list_pane(frame, app, body[0]);
        render_event_detail_pane(frame, app, body[1]);
    } else {
        let body = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area);
        render_events_list_pane(frame, app, body[0]);
        render_event_detail_pane(frame, app, body[1]);
    }
}

fn render_events_list_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::List && matches!(app.overlay, Overlay::None);
    let follow_tag = if app.events_follow { " · 跟随最新" } else { " · 暂停跟随" };
    let lane_tag = if app.hidden_lane_events > 0 {
        format!(" · 已隐{}槽位", app.hidden_lane_events)
    } else if app.show_lane_events {
        " · 含槽位".to_string()
    } else {
        String::new()
    };
    let filter_tag = if app.filter.is_empty() { "" } else { " · 过滤" };
    let title = Line::from(Span::styled(
        truncate(
            &format!(
                " 事件时间线 (显示 {} 条{}{}{}) ",
                app.filtered_events.len(),
                lane_tag,
                follow_tag,
                filter_tag
            ),
            area.width.saturating_sub(2) as usize,
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
            "暂无事件记录".to_string()
        } else {
            format!(
                "无匹配“{}”的事件（Esc 清除过滤，l 显示槽位事件）",
                app.filter
            )
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {}", truncate(&msg, inner.width.saturating_sub(1) as usize)),
                muted(),
            ))),
            inner,
        );
        return;
    }

    let inner_w = inner.width as usize;
    let show_earlier_hint = inner.height >= 3
        && (app.snapshot.events_has_more
            || app.snapshot.events_earliest_seq.is_some_and(|s| s > 1));
    let view_h = (inner.height as usize).saturating_sub(usize::from(show_earlier_hint));
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

    let mut lines: Vec<Line> = Vec::with_capacity(inner.height as usize);
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
        let ev_time = short_event_time(&ev.at, &app.snapshot.today_ymd);
        let prefix = format!(
            "#{:<4} {} {} ",
            ev.seq,
            ev_time,
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
                Span::styled(format!("{ev_time} "), muted()),
                Span::styled(format!("{} ", pad_right(ev.kind_badge(), 4)), badge_style),
                Span::styled(rest, primary()),
            ]));
        }
    }
    if show_earlier_hint {
        while lines.len() < view_h {
            lines.push(Line::raw(""));
        }
        let earliest = app
            .snapshot
            .events_earliest_seq
            .or_else(|| app.snapshot.events.first().map(|e| e.seq))
            .unwrap_or(1);
        let hint_seq = earliest.saturating_sub(200);
        let hint = format!(" 更早的事件：delegate events --since {hint_seq}");
        lines.push(Line::from(Span::styled(truncate(&hint, inner_w), muted())));
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
        Span::styled(ev.kind.clone(), bold_primary()),
    ]));
    let local_at = format_iso_local_hms(&ev.at, &app.snapshot.today_ymd)
        .or_else(|| format_iso_local_short(&ev.at, &app.snapshot.today_ymd))
        .unwrap_or_else(|| ev.at.clone());
    push_wrapped(&mut out, &format!("时间: {local_at}（本地时间）"), inner_w, muted());
    if !ev.repo.is_empty() {
        push_wrapped(&mut out, &format!("仓库: {}", ev.repo), inner_w, muted());
    }
    if !ev.run.is_empty() {
        let run_hint = if app.snapshot.find_run_by_id(&ev.run).is_some() {
            format!("运行: {}（按 t/Enter 跳到「3 运行」）", ev.run)
        } else {
            format!("运行: {}（已清理 · delegate show {}）", ev.run, ev.run)
        };
        push_wrapped(&mut out, &run_hint, inner_w, primary());
    }
    if !ev.summary.is_empty() {
        out.push(Line::raw(""));
        out.push(section_header("事件摘要"));
        push_indented_wrapped(&mut out, "  ", &ev.summary, inner_w, primary());
    }
    if !ev.run.is_empty() {
        let chain_events: Vec<&EventItem> = app
            .snapshot
            .events
            .iter()
            .filter(|e| e.run == ev.run && !matches!(e.kind.as_str(), "lane_acquired" | "lane_released"))
            .collect();
        if !chain_events.is_empty() {
            out.push(Line::raw(""));
            out.push(section_header(&format!("同一任务近期事件链 ({} 条)", chain_events.len())));
            for ce in chain_events.iter().rev().take(8).rev() {
                let t = short_event_time(&ce.at, &app.snapshot.today_ymd);
                let mark = if ce.seq == ev.seq { "▸" } else { "•" };
                let line_txt = format!("{mark} {t} #{} {} {}", ce.seq, ce.kind_badge(), ce.summary);
                let st = if ce.seq == ev.seq { bold_primary() } else { muted() };
                push_indented_wrapped(&mut out, "  ", &line_txt, inner_w, st);
            }
        }
    }
    out.push(Line::raw(""));
    out.push(section_header("原始载荷 (Payload)"));
    for line in ev.payload_pretty.lines() {
        push_indented_wrapped(&mut out, "  ", line, inner_w, muted());
    }

    let max_scroll = out.len().saturating_sub(inner_h);
    if app.detail_scroll > max_scroll {
        app.detail_scroll = max_scroll;
    }
    let title = Line::from(Span::styled(
        format!(" 事件 #{} · {} ", ev.seq, ev.kind_badge()),
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

fn render_merge_detail_item(
    frame: &mut Frame,
    app: &mut App,
    m: &MergeRunItem,
    area: Rect,
    focused: bool,
) {
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

fn render_candidate_detail_item(
    frame: &mut Frame,
    app: &mut App,
    c: &RadarCandidateItem,
    area: Rect,
    focused: bool,
) {
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
// `2 工作` View: 54:46 Dual-Pane (Left Dependency Tree + Right Live Work Card)
// ---------------------------------------------------------------------------

fn work_bucket_color(bucket: WorkBucket) -> Option<Color> {
    match bucket {
        WorkBucket::Owner => Some(STATUS_FAILED),
        WorkBucket::NeedsAction => Some(STATUS_PENDING),
        WorkBucket::InProgress => Some(STATUS_RUNNING),
        WorkBucket::Ready => Some(STATUS_QUEUED),
        WorkBucket::Waiting
        | WorkBucket::TimedWait
        | WorkBucket::Deferred
        | WorkBucket::Done
        | WorkBucket::Dropped => None,
    }
}

pub(crate) fn work_item_glyph_color(w: &WorkItem) -> (&'static str, Option<Color>) {
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
        "blocked" => ("◌", Some(STATUS_QUEUED)),
        "deferred" => ("·", None),
        _ => ("·", None),
    }
}

fn render_work_tab(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.width >= NARROW_BREAKPOINT {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(54), Constraint::Percentage(46)])
            .split(area);
        render_work_tree_pane(frame, app, body[0], true);
        render_work_detail_pane(frame, app, body[1]);
    } else {
        render_work_tree_pane(frame, app, area, false);
    }
}

fn render_work_detail_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::Detail && matches!(app.overlay, Overlay::None);
    if let Some(cand) = app.selected_work_candidate().cloned() {
        render_candidate_detail_item(frame, app, &cand, area, focused);
        return;
    }
    let Some(item) = app.selected_work_item().cloned() else {
        let block = rounded_block(Line::from(Span::styled(" 工作卡 ", muted())), focused);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " 选择左侧工作项或展开折叠组查看实时工作卡",
                muted(),
            ))),
            inner,
        );
        return;
    };

    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let lines = if item.ephemeral {
        build_ephemeral_work_card_lines(app, &item, inner_w)
    } else {
        build_work_card_lines(app, &item, inner_w)
    };
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
    let repo_part = if item.repo_label.is_empty() {
        String::new()
    } else {
        format!(" [{}]", item.repo_label)
    };
    let title_str = format!(" 工作卡 · {}{repo_part} ", item.id);
    let mut block = rounded_block(
        Line::from(Span::styled(
            truncate(&title_str, inner_w.saturating_sub(2)),
            if focused { bold_primary() } else { muted() },
        )),
        focused,
    );
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

fn build_ephemeral_work_card_lines(
    app: &App,
    item: &WorkItem,
    width: usize,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    out.push(Line::from(vec![
        Span::styled(
            format!(" {} ", item.state_badge()),
            Style::default()
                .fg(STATUS_RUNNING)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(item.title.clone(), bold_primary()),
    ]));
    push_wrapped(
        &mut out,
        &format!("未挂工作项运行 · 仓库: {}", item.repo_label),
        width,
        muted(),
    );
    if let Some((_, _, run)) = app.snapshot.find_run_for_work(item) {
        out.push(Line::raw(""));
        out.push(section_header("关联运行（按 t 跳转审查卡与 Diff）"));
        let (f, add, del) = run.display_changes();
        let chg = if f > 0 {
            format!(" · {f}f +{add}/-{del}")
        } else {
            String::new()
        };
        push_indented_wrapped(
            &mut out,
            "  ",
            &format!("{} · {}{chg}", run.run, run.state_badge()),
            width,
            primary(),
        );
        if let Some(act) = &run.activity {
            let detail = act
                .cmd
                .as_deref()
                .map(shorten_running_cmd)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| act.label.clone());
            push_indented_wrapped(&mut out, "  ", &format!("当前动作: {detail}"), width, muted());
        }
        if let Some(eta) = &run.eta {
            if !eta.detail_text.is_empty() {
                push_indented_wrapped(
                    &mut out,
                    "  ",
                    &eta.detail_text,
                    width,
                    Style::default().fg(STATUS_QUEUED),
                );
            }
        }
    } else if !item.focus.is_empty() {
        out.push(Line::raw(""));
        push_wrapped(&mut out, &item.focus, width, primary());
    }
    if !item.next.is_empty() {
        out.push(Line::raw(""));
        out.push(section_header("下一步（按 y 复制命令）"));
        push_indented_wrapped(
            &mut out,
            "  ",
            &format!("delegate {}", item.next),
            width,
            Style::default().fg(STATUS_QUEUED),
        );
    }
    out
}

fn render_work_tree_pane(frame: &mut Frame, app: &mut App, area: Rect, dual_pane: bool) {
    let focused = (!dual_pane || app.focus == FocusPane::List) && matches!(app.overlay, Overlay::None);
    let compact = !dual_pane && area.width < COMPACT_BREAKPOINT;
    let c = app.snapshot.work_counts();
    let pulse = app.snapshot.effective_pulse();
    let active_pool = pulse.work_active_pool;
    let timed_or_def = c.timed + c.deferred;
    let stale_suffix = if c.stale > 0 {
        format!(" · 老化 {}", c.stale)
    } else {
        String::new()
    };
    let work_eta_suffix = if !compact {
        let we = &app.snapshot.eta.work;
        let clock = we
            .earliest_finish_p50_clock
            .as_deref()
            .map(|cl| format!(" · 活跃链预计 {cl}"))
            .unwrap_or_default();
        if timed_or_def > 0 {
            format!(" · 定时/暂缓 {timed_or_def}{clock}")
        } else {
            clock
        }
    } else {
        String::new()
    };
    let mode_tag = match app.work_filter_mode {
        WorkFilterMode::All => {
            if app.filter.is_empty() {
                if compact {
                    format!(" 工作表 (推进 {active_pool}{stale_suffix}) ")
                } else {
                    format!(
                        " 工作表 (推进 {active_pool}{stale_suffix} · 今日完成 {} / 共 {}{work_eta_suffix}) ",
                        c.done_today, c.done_total
                    )
                }
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
            "当前无开放工作项（用 delegate work add 新建）".to_string()
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

    let has_any_open = active_pool > 0 || c.timed > 0 || c.deferred > 0;
    let empty_open_banner = !has_any_open
        && app.work_filter_mode == WorkFilterMode::All
        && app.filter.is_empty()
        && c.done_total > 0;

    let show_col_header = !compact && inner_h >= 4;
    let header_h: usize = usize::from(show_col_header) + usize::from(empty_open_banner);
    let view_h = inner_h.saturating_sub(header_h);

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
    let max_id_w = app
        .work_rows
        .iter()
        .filter_map(|r| match r {
            WorkRow::Item { work_idx, .. } => {
                Some(display_width(&app.snapshot.work[*work_idx].id))
            }
            _ => None,
        })
        .max()
        .unwrap_or(4);
    let cols = WorkColWidths::compute(inner_w, max_id_w);
    if show_col_header {
        lines.push(render_work_col_header(cols, inner_w));
    }
    if empty_open_banner {
        let banner = format!(
            " 当前无开放工作项（已完成 {} 项，按 Space 展开）",
            c.done_total
        );
        lines.push(Line::from(Span::styled(truncate(&banner, inner_w), muted())));
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
            WorkRow::GroupHeader {
                bucket,
                own_count,
                attached_downstream,
                ephemeral_count,
            } => vec![render_work_group_header(
                *bucket,
                *own_count,
                *attached_downstream,
                *ephemeral_count,
                inner_w,
            )],
            WorkRow::FoldToggle {
                kind,
                count,
                expanded,
            } => vec![render_work_fold_row(
                *kind,
                *count,
                *expanded,
                selected,
                app.snapshot.eta.work.earliest_deferred_clock.as_deref(),
                c.done_today,
                inner_w,
            )],
            WorkRow::Candidate { cand_idx } => vec![render_work_candidate_row(
                &app.snapshot.candidates[*cand_idx],
                selected,
                inner_w,
            )],
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

fn render_work_candidate_row(
    c: &RadarCandidateItem,
    selected: bool,
    width: usize,
) -> Line<'static> {
    let (glyph, color) = if !c.picked_at.is_empty() {
        ("✓", None)
    } else if c.brief_ready {
        ("◆", Some(STATUS_RUNNING))
    } else {
        ("◇", Some(STATUS_PENDING))
    };
    let tag = if !c.picked_at.is_empty() {
        "已挑"
    } else if c.brief_ready {
        "可直派"
    } else {
        "需细化"
    };
    let text = format!("{} [{}] [{}] {}", c.id, c.repo_label, tag, c.title);
    if selected {
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        let accent = color
            .map(|col| Style::default().fg(col).add_modifier(Modifier::BOLD))
            .unwrap_or_else(bold_primary);
        let clipped = truncate(&text, width.saturating_sub(4));
        let pad = width.saturating_sub(4 + display_width(&clipped));
        Line::from(vec![
            Span::styled("▎", accent),
            Span::styled(format!(" {glyph} "), rev.add_modifier(Modifier::BOLD)),
            Span::styled(clipped, rev),
            Span::styled(" ".repeat(pad), rev),
        ])
    } else {
        let st = color
            .map(|col| Style::default().fg(col).add_modifier(Modifier::BOLD))
            .unwrap_or_else(muted);
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{glyph} "), st),
            Span::styled(truncate(&text, width.saturating_sub(4)), primary()),
        ])
    }
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
    fn compute(width: usize, max_id_w: usize) -> Self {
        let id_w = if width >= 86 {
            max_id_w.clamp(4, 12)
        } else {
            max_id_w.clamp(4, 8)
        };
        let repo_w = if width >= 88 { 10 } else if width >= 72 { 8 } else { 0 };
        let state_w = 6;
        let status_w = if width >= 110 {
            34
        } else if width >= 90 {
            30
        } else if width >= 76 {
            20
        } else {
            16
        };
        let repo_extra = if repo_w > 0 { repo_w + 1 } else { 0 };
        let fixed = 4 + id_w + 1 + repo_extra + 1 + state_w + 1 + status_w;
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
    let mut s = format!("    {} ", pad_right("ID", cols.id_w));
    if cols.repo_w > 0 {
        s.push_str(&pad_right("仓库", cols.repo_w));
        s.push(' ');
    }
    s.push_str(&format!(
        "{} {} {}",
        pad_right("标题", cols.title_w),
        pad_right("状态", cols.state_w),
        pad_right("此刻在干什么 / 预估", cols.status_w),
    ));
    Line::from(Span::styled(truncate(&s, width), muted()))
}

fn render_work_group_header(
    bucket: WorkBucket,
    own_count: usize,
    attached_downstream: usize,
    ephemeral_count: usize,
    width: usize,
) -> Line<'static> {
    let style = match work_bucket_color(bucket) {
        Some(c) => Style::default().fg(c).add_modifier(Modifier::BOLD),
        None => muted().add_modifier(Modifier::BOLD),
    };
    let mut details = vec![format!("{own_count}")];
    if attached_downstream > 0 {
        details.push(format!("阻下游 {attached_downstream}"));
    }
    if ephemeral_count > 0 && bucket == WorkBucket::InProgress {
        details.push(format!("含未挂工作项运行 {ephemeral_count}"));
    }
    if bucket == WorkBucket::TimedWait {
        details.push("不计入当前推进 ETA".to_string());
    }
    let label = format!(" ▾ {} ({})", bucket.label(), details.join(" · "));
    Line::from(Span::styled(truncate(&label, width), style))
}

fn render_work_fold_row(
    kind: WorkFoldKind,
    count: usize,
    expanded: bool,
    selected: bool,
    earliest_deferred: Option<&str>,
    done_today: usize,
    width: usize,
) -> Line<'static> {
    let arrow = if expanded { "▾" } else { "▸" };
    let label = match kind {
        WorkFoldKind::Deferred => match earliest_deferred {
            Some(cl) => format!("{arrow} 暂缓 {count} 条（最早 {cl} · Space 展开）"),
            None => format!("{arrow} 暂缓 {count} 条（Space 展开）"),
        },
        WorkFoldKind::Candidates => format!("{arrow} 侦察候选 {count} 条（Space 展开 · p 挑草稿）"),
        WorkFoldKind::Done => {
            if done_today > 0 {
                format!("{arrow} 已完成 {count} 条（今日 {done_today} 条）")
            } else {
                format!("{arrow} 已完成 {count} 条")
            }
        }
        WorkFoldKind::Dropped => format!("{arrow} 已否/已放弃 {count} 条"),
    };
    if selected {
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        let hint = if width >= COMPACT_BREAKPOINT as usize {
            if expanded { "Space 折叠 " } else { "Space 展开 " }
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
    let style = match kind {
        WorkFoldKind::Candidates => Style::default().fg(STATUS_QUEUED),
        _ => muted(),
    };
    Line::from(Span::styled(truncate(&format!(" {label}"), width), style))
}

pub(crate) fn format_tree_prefix_wide(depth: usize, branch_guides: &[bool], is_last_sibling: bool) -> String {
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

fn work_item_eta_tag(item: &WorkItem, compact: bool) -> Option<(String, Color)> {
    if item.state == "done" {
        return None;
    }
    let eta = item.eta.as_ref()?;
    let raw = if compact {
        eta.compact_badge.trim()
    } else if !eta.focus_badge.trim().is_empty() {
        eta.focus_badge.trim()
    } else {
        eta.compact_badge.trim()
    };
    if raw.is_empty() || raw == "-" || raw == "完成" {
        return None;
    }
    let badge = if compact {
        truncate(raw, 6)
    } else {
        raw.to_string()
    };
    let color = if eta.stopped_clock {
        let severe = eta.blocker_kind.as_deref().is_some_and(|k| {
            matches!(
                k,
                "premerge_conflict" | "premerge_failed" | "mainline_blocked" | "stall_warning"
            )
        });
        if severe {
            STATUS_FAILED
        } else {
            STATUS_PENDING
        }
    } else {
        STATUS_QUEUED
    };
    Some((format!(" {badge}"), color))
}

/// Combine `live_status` and `eta_tag` cleanly without repeating upstream targets
/// (e.g. `等 W119 关闭` + `等W119(~04:40)` -> `等 W119 关闭` + ` (~04:40)`;
/// `等 阶段 2 · T3.1 合入` + `等 阶段 2 · T3.1（交付 ~03:47，合入 ~03:55）` -> single sentence).
pub(crate) fn merge_work_live_and_eta(
    item: &WorkItem,
    live_status: &str,
    compact: bool,
) -> (String, Option<(String, Color)>) {
    let Some((raw_tag, color)) = work_item_eta_tag(item, compact) else {
        return (live_status.to_string(), None);
    };
    let badge = raw_tag.trim();
    if live_status.contains(badge) {
        return (live_status.to_string(), None);
    }
    if (live_status.starts_with("暂缓至 ") || live_status.starts_with("等到 "))
        && badge.starts_with('等')
    {
        return (live_status.to_string(), None);
    }
    // Upstream run wait badge already includes full name + delivery/apply times: replace verbose wait status.
    if item.state == "blocked"
        && live_status.starts_with("等 ")
        && badge.starts_with("等 ")
    {
        return (badge.to_string(), None);
    }
    // Deduplicate `等 W119 关闭` + `等W119(~04:40)` into `等 W119 关闭` + ` (~04:40)`.
    if let Some(after_wait) = badge.strip_prefix('等') {
        if let Some(paren_pos) = after_wait.find('(') {
            let target = after_wait[..paren_pos].trim();
            let clock_part = &after_wait[paren_pos..];
            if !target.is_empty() && live_status.contains(target) {
                return (live_status.to_string(), Some((format!(" {clock_part}"), color)));
            }
        }
    }
    if item.state == "ready" && badge.starts_with("预计耗时") && !live_status.is_empty() {
        return (live_status.to_string(), Some((format!(" · {badge}"), color)));
    }
    (live_status.to_string(), Some((raw_tag, color)))
}

fn work_item_tail_tags(
    item: &WorkItem,
    eta_tag: Option<(String, Color)>,
    stale_info: Option<(u64, bool)>,
) -> Vec<(String, Color)> {
    let mut tags = Vec::new();
    if let Some(t) = eta_tag {
        tags.push(t);
    }
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
/// `可派发 {age}` / `就绪 {age}` / `待闭环 {age}` so the same duration is never shown twice on one row.
fn dedup_stale_live_status(live_status: &str, has_stale_tag: bool) -> &str {
    if !has_stale_tag {
        return live_status;
    }
    if let Some((head, tail)) = live_status.rsplit_once(' ') {
        let is_age_token = tail.len() >= 2
            && matches!(tail.as_bytes().last(), Some(b's' | b'm' | b'h' | b'd'))
            && tail[..tail.len() - 1].bytes().all(|b| b.is_ascii_digit());
        if is_age_token
            && (head.ends_with("可派发")
                || head.ends_with("就绪")
                || head.ends_with("待闭环"))
        {
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
    let (merged_live, eta_tag) = merge_work_live_and_eta(item, live_status, false);
    let tail_tags = work_item_tail_tags(item, eta_tag, stale_info);
    let live_status = dedup_stale_live_status(&merged_live, stale_info.is_some());
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
        if cols.repo_w > 0 {
            spans.push(Span::styled(repo_col, rev));
            spans.push(Span::styled(" ", rev));
        }
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
    if cols.repo_w > 0 {
        spans.push(Span::styled(repo_col, muted()));
        spans.push(Span::raw(" "));
    }
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
    let (merged_live, eta_tag) = merge_work_live_and_eta(item, live_status, true);
    let tail_tags = work_item_tail_tags(item, eta_tag, stale_info);
    let live_status = dedup_stale_live_status(&merged_live, stale_info.is_some());
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
    if item.state != "done" {
        if let Some(eta) = &item.eta {
            if !eta.short_text.is_empty() && eta.short_text != "-" {
                let eta_style = if eta.stopped_clock {
                    Style::default()
                        .fg(STATUS_PENDING)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(STATUS_QUEUED)
                };
                push_wrapped(&mut out, &format!("预估: {}", eta.short_text), width, eta_style);
            }
        }
    }

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

pub(crate) fn quota_bar_color(pct_x100: u32) -> Color {
    if pct_x100 < 2000 {
        STATUS_FAILED
    } else if pct_x100 < 5000 {
        STATUS_PENDING
    } else {
        STATUS_RUNNING
    }
}

pub(crate) fn format_quota_updated(age_secs: u64) -> String {
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

pub(crate) fn format_reset_countdown_zh(secs: Option<u64>) -> String {
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

pub(crate) fn format_minutes_zh(mins: u64) -> String {
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

pub(crate) fn short_pool_name(pool: &str) -> String {
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

pub(crate) fn window_display_name(e: &QuotaPoolEntry) -> String {
    match e.window.as_str() {
        "weekly" => "本周".to_string(),
        "fiveHour" => "5 小时".to_string(),
        _ => truncate(&e.window_label, 8),
    }
}

pub(crate) fn format_pct_clean(pct_x100: u32) -> String {
    if pct_x100.is_multiple_of(100) {
        format!("{}%", pct_x100 / 100)
    } else if pct_x100.is_multiple_of(10) {
        format!("{:.1}%", pct_x100 as f64 / 100.0)
    } else {
        format!("{:.2}%", pct_x100 as f64 / 100.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowEstimateOutcome {
    Insufficient,
    Idle,
    WontRunOut,
    RunsOutIn {
        fast_m: u64,
        mid_m: u64,
        slow_m: Option<u64>,
        reset_secs: Option<u64>,
        alert: bool,
    },
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

fn round_eta_minutes(mins: u64) -> u64 {
    if mins == 0 {
        return 0;
    }
    if mins < 60 {
        (((mins + 2) / 5) * 5).max(5)
    } else if mins < 6 * 60 {
        ((mins + 7) / 15) * 15
    } else if mins < 48 * 60 {
        ((mins + 30) / 60) * 60
    } else {
        ((mins + 720) / 1440).max(2) * 1440
    }
}

fn format_hours_half(mins: u64) -> Option<String> {
    if !(60..48 * 60).contains(&mins) || !mins.is_multiple_of(30) {
        return None;
    }
    let h = mins / 60;
    if mins.is_multiple_of(60) {
        Some(format!("{h}"))
    } else {
        Some(format!("{h}.5"))
    }
}

fn format_duration_mins_compact(mins: u64) -> String {
    if mins < 60 {
        format!("{mins}m")
    } else if mins < 48 * 60 {
        let h = mins / 60;
        let m = mins % 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        }
    } else {
        let d = (mins + 720) / 1440;
        format!("{d}d")
    }
}

pub(crate) fn format_eta_compact(
    fast_m: u64,
    mid_m: u64,
    slow_m: Option<u64>,
    reset_secs: Option<u64>,
) -> String {
    let reset_mins = reset_secs.map(|s| (s / 60).max(1)).unwrap_or(u64::MAX);
    if mid_m == 0 || reset_mins <= 1 {
        return "即将用完".to_string();
    }
    let max_m = reset_mins.saturating_sub(1).max(1);
    let slow_eff = slow_m.unwrap_or(reset_mins);
    let is_wide = (slow_eff.saturating_sub(fast_m)) as f64 / (mid_m as f64) > 0.25;
    if is_wide {
        let fast_r = round_eta_minutes(fast_m).max(5).min(max_m).max(1);
        let slow_r_opt = slow_m.map(round_eta_minutes).filter(|&sr| sr < reset_mins);
        if let Some(slow_r) = slow_r_opt {
            if slow_r > fast_r {
                if fast_r < 60 && slow_r < 60 {
                    return format!("~{fast_r}-{slow_r}m");
                }
                if let (Some(fh), Some(sh)) = (format_hours_half(fast_r), format_hours_half(slow_r)) {
                    return format!("~{fh}-{sh}h");
                }
                if fast_r >= 48 * 60 && slow_r >= 48 * 60 && fast_r.is_multiple_of(1440) && slow_r.is_multiple_of(1440) {
                    return format!("~{}-{}d", fast_r / 1440, slow_r / 1440);
                }
            }
        }
        return format!("~{}", format_duration_mins_compact(fast_r));
    }
    let mid_r = round_eta_minutes(mid_m).min(max_m).max(1);
    format!("~{}", format_duration_mins_compact(mid_r))
}

pub(crate) fn format_eta_zh_phrase(
    fast_m: u64,
    mid_m: u64,
    slow_m: Option<u64>,
    reset_secs: Option<u64>,
) -> String {
    let reset_mins = reset_secs.map(|s| (s / 60).max(1)).unwrap_or(u64::MAX);
    if mid_m == 0 || reset_mins <= 1 {
        return "即将用完".to_string();
    }
    let max_m = reset_mins.saturating_sub(1).max(1);
    let slow_eff = slow_m.unwrap_or(reset_mins);
    let is_wide = (slow_eff.saturating_sub(fast_m)) as f64 / (mid_m as f64) > 0.25;
    if is_wide {
        let fast_r = round_eta_minutes(fast_m).max(5).min(max_m).max(1);
        let slow_r_opt = slow_m.map(round_eta_minutes).filter(|&sr| sr < reset_mins);
        match slow_r_opt {
            None => {
                if let Some(fh) = format_hours_half(fast_r) {
                    format!("约 {fh} 小时后到重置前之间用完")
                } else {
                    format!("约 {}后到重置前之间用完", format_minutes_zh(fast_r))
                }
            }
            Some(slow_r) if slow_r <= fast_r => {
                format!("约 {}后用完", format_minutes_zh(fast_r))
            }
            Some(slow_r) => {
                if fast_r < 60 && slow_r < 60 {
                    format!("约 {fast_r}–{slow_r} 分钟后用完")
                } else if let (Some(fh), Some(sh)) =
                    (format_hours_half(fast_r), format_hours_half(slow_r))
                {
                    format!("约 {fh}–{sh} 小时后用完")
                } else if fast_r >= 48 * 60 && slow_r >= 48 * 60 && fast_r.is_multiple_of(1440) && slow_r.is_multiple_of(1440) {
                    format!("约 {}–{} 天后用完", fast_r / 1440, slow_r / 1440)
                } else {
                    format!(
                        "约 {}–{}后用完",
                        format_minutes_zh(fast_r),
                        format_minutes_zh(slow_r)
                    )
                }
            }
        }
    } else {
        let mid_r = round_eta_minutes(mid_m).min(max_m).max(1);
        format!("约 {}后用完", format_minutes_zh(mid_r))
    }
}

fn pace_prefix(est: &QuotaWindowEstimate) -> String {
    let base = match est.basis.as_str() {
        "30m" => "按近 30 分钟额度变化",
        "1h" => "按近 1 小时额度变化",
        "24h" => "按近 24 小时额度变化",
        "window" => "按本周期额度变化",
        "passive" => "已触发额度耗尽",
        _ => "按当前额度变化",
    };
    if est.running_count > 0 && est.basis != "passive" {
        format!("{base}（在跑 {}）", est.running_count)
    } else {
        base.to_string()
    }
}

pub(crate) fn evaluate_window_outcome(
    acct: &AgyAccountItem,
    window_key: &str,
    est: &QuotaWindowEstimate,
) -> Option<WindowEstimateOutcome> {
    match est.status {
        QuotaEtaStatus::Insufficient => Some(WindowEstimateOutcome::Insufficient),
        QuotaEtaStatus::Idle => Some(WindowEstimateOutcome::Idle),
        QuotaEtaStatus::Safe => Some(WindowEstimateOutcome::WontRunOut),
        QuotaEtaStatus::Depletes => {
            let mid_m = est.mid_minutes?;
            let fast_m = est.fast_minutes.unwrap_or(mid_m).min(mid_m);
            let reset_secs = est
                .reset_in_seconds
                .filter(|&s| s > 0)
                .or_else(|| find_reset_seconds(acct, window_key, est.pool.as_deref()));
            if let Some(rs) = reset_secs {
                if mid_m.saturating_mul(60) >= rs {
                    return Some(WindowEstimateOutcome::WontRunOut);
                }
            }
            Some(WindowEstimateOutcome::RunsOutIn {
                fast_m,
                mid_m,
                slow_m: est.slow_minutes,
                reset_secs,
                alert: est.alert,
            })
        }
    }
}

fn format_single_window_conclusion(
    est: &QuotaWindowEstimate,
    outcome: WindowEstimateOutcome,
    win_label: &str,
    multi_pool: bool,
) -> (String, bool) {
    let subject = if multi_pool {
        match est.pool.as_deref() {
            Some(p) if !p.is_empty() => format!("{} {win_label}", short_pool_name(p)),
            _ => win_label.to_string(),
        }
    } else {
        win_label.to_string()
    };
    match outcome {
        WindowEstimateOutcome::Insufficient => {
            let text = match est.reason.as_str() {
                "too_early" => {
                    let elapsed = est
                        .elapsed_minutes
                        .map(format_minutes_zh)
                        .unwrap_or_else(|| "不久".to_string());
                    if let Some(pct_x100) = est.remaining_pct_x100 {
                        format!(
                            "{subject}刚重置 {elapsed}（剩 {}），数据不足暂不外推",
                            format_pct_clean(pct_x100)
                        )
                    } else {
                        format!("{subject}刚重置 {elapsed}，数据不足暂不外推")
                    }
                }
                "not_started" => format!("{subject}额度尚未使用（剩 100%），暂不外推"),
                "stale" => format!("{subject}额度缓存已过期，暂不外推"),
                "reset_passed" => format!("{subject}额度已过重置时间，待下次刷新"),
                _ => format!("{subject}额度数据不足，暂不外推"),
            };
            (text, false)
        }
        WindowEstimateOutcome::Idle => (format!("当前无在跑任务，{subject}额度保持不变"), false),
        WindowEstimateOutcome::WontRunOut => {
            let pace = pace_prefix(est);
            (format!("{pace}，{subject}额度在重置前用不完"), false)
        }
        WindowEstimateOutcome::RunsOutIn {
            fast_m,
            mid_m,
            slow_m,
            reset_secs,
            alert,
        } => {
            let pace = pace_prefix(est);
            let eta = format_eta_zh_phrase(fast_m, mid_m, slow_m, reset_secs);
            let reset_tail = reset_secs
                .filter(|&s| s > 0)
                .map(|s| format!("（{}）", format_reset_countdown_zh(Some(s))))
                .unwrap_or_default();
            (format!("{pace}，{subject}额度{eta}{reset_tail}"), alert)
        }
    }
}

/// Build plain-language depletion conclusion line(s) for an account.
/// Uses `/usage` same-cycle slope estimates only (no local token calibration).
/// Returns `(text, is_warning)` where `is_warning` is true when an active alert triggers.
pub(crate) fn format_quota_estimate_lines(
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
        (Some((_, WindowEstimateOutcome::Idle)), Some((_, WindowEstimateOutcome::Idle))) => {
            vec![("当前无在跑任务，5 小时与本周额度保持不变".to_string(), false)]
        }
        (
            Some((fe, WindowEstimateOutcome::WontRunOut)),
            Some((we, WindowEstimateOutcome::WontRunOut)),
        ) => {
            if fe.basis == we.basis && fe.running_count == we.running_count || single_sentence {
                let pace = pace_prefix(fe);
                vec![(format!("{pace}，5 小时与本周额度在重置前用不完"), false)]
            } else {
                vec![
                    format_single_window_conclusion(fe, WindowEstimateOutcome::WontRunOut, "5 小时", multi_pool),
                    format_single_window_conclusion(we, WindowEstimateOutcome::WontRunOut, "本周", multi_pool),
                ]
            }
        }
        (
            Some((fe, fo @ WindowEstimateOutcome::RunsOutIn { alert: fa, mid_m: fm, .. })),
            Some((we, wo @ WindowEstimateOutcome::RunsOutIn { alert: wa, mid_m: wm, .. })),
        ) => {
            let line_fh = format_single_window_conclusion(fe, fo, "5 小时", multi_pool);
            let line_wk = format_single_window_conclusion(we, wo, "本周", multi_pool);
            if single_sentence {
                if (fa && !wa) || (fa == wa && fm <= wm) {
                    vec![line_fh]
                } else {
                    vec![line_wk]
                }
            } else {
                vec![line_fh, line_wk]
            }
        }
        (Some((fe, fo @ WindowEstimateOutcome::RunsOutIn { .. })), Some((we, wo))) => {
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
        (Some((fe, fo)), Some((we, wo @ WindowEstimateOutcome::RunsOutIn { .. }))) => {
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
        (Some((fe, fo)), Some((we, wo))) => {
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
        (Some((fe, fo)), None) => {
            vec![format_single_window_conclusion(fe, fo, "5 小时", multi_pool)]
        }
        (None, Some((we, wo))) => {
            vec![format_single_window_conclusion(we, wo, "本周", multi_pool)]
        }
        (None, None) => Vec::new(),
    }
}

pub(crate) fn format_sparkline_axis(axis_w: usize) -> String {
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

pub(crate) fn inline_spark_text(values: &[u64], buckets: usize) -> String {
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

pub(crate) fn render_quota_bar_spans(ratio: f64, bar_w: usize, color: Color) -> Vec<Span<'static>> {
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

pub(crate) fn render_quota_gauge_row(frame: &mut Frame, area: Rect, label: &str, entry: &QuotaPoolEntry) {
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
    let label_str = pad_right(label, label_w);
    let needs_sep = !label_str.ends_with(' ');
    spans.push(Span::styled(label_str, primary()));
    if bar_w > 0 {
        if needs_sep {
            spans.push(Span::raw(" "));
        }
        spans.extend(render_quota_bar_spans(
            ratio,
            bar_w,
            quota_bar_color(entry.remaining_pct_x100),
        ));
        spans.push(Span::raw(" "));
        let sep_w = usize::from(needs_sep);
        let text_budget = avail.saturating_sub(bar_w + 1 + sep_w);
        spans.push(Span::styled(truncate(&gauge_lbl, text_budget), bold_primary()));
    } else {
        if needs_sep {
            spans.push(Span::raw(" "));
        }
        let text_budget = avail.saturating_sub(usize::from(needs_sep));
        spans.push(Span::styled(
            truncate(&gauge_lbl, text_budget),
            Style::default().fg(quota_bar_color(entry.remaining_pct_x100)),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub(crate) fn build_horiz_bar(ratio: f64, width: usize) -> (String, String) {
    if width == 0 {
        return (String::new(), String::new());
    }
    let clamped = ratio.clamp(0.0, 1.0);
    let eighths = (clamped * (width as f64) * 8.0).round() as usize;
    let full = (eighths / 8).min(width);
    let rem = if full < width { eighths % 8 } else { 0 };
    let partial = match rem {
        1 => "▏",
        2 => "▎",
        3 => "▍",
        4 => "▌",
        5 => "▋",
        6 => "▊",
        7 => "▉",
        _ => "",
    };
    let mut filled = "█".repeat(full);
    if !partial.is_empty() {
        filled.push_str(partial);
    }
    let empty_n = width.saturating_sub(full + usize::from(!partial.is_empty()));
    let empty = "░".repeat(empty_n);
    (filled, empty)
}


pub(crate) fn format_tokens_short(n: u64) -> String {
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

#[allow(dead_code)]
pub(crate) fn format_usage_window_line(label: &str, w: &AccountUsageWindow, include_total: bool) -> String {
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

pub(crate) fn format_iso_compact(iso: &str, today_ymd: &str) -> String {
    let t = iso.trim();
    if t.is_empty() {
        return "-".to_string();
    }
    if let Some(loc) = format_iso_local_short(t, today_ymd) {
        return loc;
    }
    if t.len() >= 16 && t.as_bytes()[10] == b'T' {
        return format!("{} {}", &t[5..10], &t[11..16]);
    }
    truncate(t, 16)
}

pub(crate) fn format_running_agy_proc(p: &RunningAgyProc) -> String {
    let name = p
        .run_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| p.run.as_deref().filter(|s| !s.is_empty()).map(strip_run_id_timestamp));
    if let Some(n) = name {
        let role_tag = match (p.role.as_deref(), p.round.as_deref()) {
            (Some("reviewer"), Some(rd)) if !rd.is_empty() => format!("（审查 {rd}）"),
            (Some("reviewer"), _) => "（审查）".to_string(),
            (Some("author"), _) => "（作者）".to_string(),
            _ => String::new(),
        };
        return format!("{} · {n}{role_tag}", p.pid);
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

    let title_style = Style::default()
        .fg(STATUS_PENDING)
        .add_modifier(Modifier::BOLD);
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
    let target_desc = if target_id == "auto" {
        "auto（按池自动选号，不改 ~/.gemini 实盘凭据）".to_string()
    } else if target_email.is_empty() {
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
        let note_running = format!(
            "当前有 {running_count} 个在跑 agy 进程：在跑的进程与已开始的会话继续用原账号，新 run 用新账号。"
        );
        for l in wrap_text(&note_running, iw) {
            lines.push(Line::from(Span::styled(
                l,
                Style::default()
                    .fg(STATUS_PENDING)
                    .add_modifier(Modifier::BOLD),
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::{format_eta_compact, format_eta_zh_phrase, pace_prefix};
    use crate::model::QuotaWindowEstimate;

    #[test]
    fn pace_prefix_and_eta_interval_formatting() {
        let mk = |basis: &str, running: usize| QuotaWindowEstimate {
            basis: basis.to_string(),
            running_count: running,
            ..QuotaWindowEstimate::default()
        };
        assert_eq!(pace_prefix(&mk("30m", 2)), "按近 30 分钟额度变化（在跑 2）");
        assert_eq!(pace_prefix(&mk("1h", 0)), "按近 1 小时额度变化");
        assert_eq!(pace_prefix(&mk("24h", 1)), "按近 24 小时额度变化（在跑 1）");
        assert_eq!(pace_prefix(&mk("window", 0)), "按本周期额度变化");
        assert_eq!(pace_prefix(&mk("passive", 1)), "已触发额度耗尽");
        assert_eq!(pace_prefix(&mk("", 0)), "按当前额度变化");

        assert_eq!(
            format_eta_zh_phrase(60, 75, Some(90), Some(14_400)),
            "约 1–1.5 小时后用完"
        );
        assert_eq!(
            format_eta_zh_phrase(90, 120, None, Some(14_400)),
            "约 1.5 小时后到重置前之间用完"
        );
        assert_eq!(
            format_eta_compact(60, 75, Some(90), Some(14_400)),
            "~1-1.5h"
        );
        // Tiny reset window (< 5m) must never round above resetAt.
        assert_eq!(format_eta_zh_phrase(2, 2, Some(2), Some(240)), "约 3 分钟后用完");
        assert_eq!(format_eta_zh_phrase(2, 2, Some(3), Some(240)), "约 3 分钟后到重置前之间用完");
        assert_eq!(format_eta_compact(2, 2, Some(3), Some(240)), "~3m");
        assert_eq!(format_eta_zh_phrase(1, 1, None, Some(90)), "即将用完");
    }
}
