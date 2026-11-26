#![forbid(unsafe_code)]
//! `4 额度` — Merged accounts + quota pools + 24h Sparkline + 7d horizontal Token distribution
//! (purpose + ALL models with viewport folding) + 7d horizontal delivery efficiency bars.
//!
//! Manual account switching is bound exclusively to `u` (with confirm modal); `Enter` only shifts
//! focus to detail and NEVER switches accounts.

use crate::app::{App, FocusPane, Overlay};
use crate::model::*;
use crate::ui::{
    bold_primary, build_horiz_bar, evaluate_window_outcome, format_eta_compact, format_iso_compact,
    format_quota_estimate_lines, format_quota_updated, format_running_agy_proc,
    format_sparkline_axis, format_tokens_short, inline_spark_text, muted, primary,
    push_indented_wrapped, push_wrapped, render_quota_gauge_row, rounded_block, section_header,
    short_pool_name, window_display_name, WindowEstimateOutcome, NARROW_BREAKPOINT, STATUS_FAILED,
    STATUS_PENDING, STATUS_QUEUED, STATUS_RUNNING,
};
use crate::ui_overview::summarize_7d_trend;
use crate::width::{display_width, pad_left, pad_right, truncate, wrap_text};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::bar;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Sparkline};
use ratatui::Frame;

pub(crate) fn render_quota_tab(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.width >= NARROW_BREAKPOINT {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
            .split(area);

        let left_h = cols[0].height as usize;
        let acct_rows = app.filtered_accounts.len().max(2);
        let proc_rows = if app.snapshot.agy_accounts.running_agy.is_empty() {
            0
        } else {
            2 + app.snapshot.agy_accounts.running_agy.len().div_ceil(2).min(3)
        };
        let top_left_need = (acct_rows + proc_rows + 4).clamp(7, (left_h * 48 / 100).max(7));
        let left_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(top_left_need as u16),
                Constraint::Min(8),
            ])
            .split(cols[0]);

        render_quota_accounts_pane(frame, app, left_chunks[0], true);
        render_quota_7d_distribution_pane(frame, app, left_chunks[1]);

        let right_h = cols[1].height as usize;
        let trend_h = 12usize.min((right_h * 40 / 100).max(8));
        let right_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(10),
                Constraint::Length(trend_h as u16),
            ])
            .split(cols[1]);

        render_quota_account_detail_pane(frame, app, right_chunks[0]);
        render_quota_7d_efficiency_pane(frame, app, right_chunks[1]);
    } else {
        // Narrow (< 110 cols): top account table (5..=8 rows), bottom scrollable combined detail.
        let top_h = (app.filtered_accounts.len() as u16)
            .saturating_add(3)
            .clamp(5, 8);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(top_h), Constraint::Min(8)])
            .split(area);
        render_quota_accounts_pane(frame, app, chunks[0], false);
        render_quota_narrow_detail(frame, app, chunks[1], area.height < 30);
    }
}

fn find_account_pool_pct(acct: &AgyAccountItem, pool_substr: &str, win_key: &str) -> Option<u32> {
    let q = acct.quota.as_ref()?;
    if !q.ok || q.identity_mismatch {
        return None;
    }
    q.entries
        .iter()
        .find(|e| e.window == win_key && e.pool.to_ascii_lowercase().contains(pool_substr))
        .or_else(|| q.entries.iter().find(|e| e.window == win_key))
        .map(|e| (e.remaining_pct_x100 + 50) / 100)
}

fn earliest_depletion_cell(acct: &AgyAccountItem) -> (String, Style) {
    let Some(est) = &acct.quota_estimate else {
        return ("-".to_string(), muted());
    };
    let mut best_alert: Option<(u64, &'static str, String)> = None;
    let mut has_safe = false;
    let mut has_idle = false;
    let mut has_insufficient = false;
    let mut check = |win_key: &str, lbl: &'static str, w: &QuotaWindowEstimate| {
        match evaluate_window_outcome(acct, win_key, w) {
            Some(WindowEstimateOutcome::RunsOutIn {
                fast_m,
                mid_m,
                slow_m,
                reset_secs,
                alert: true,
            }) => {
                if best_alert.as_ref().is_none_or(|(b, _, _)| mid_m < *b) {
                    let compact = format_eta_compact(fast_m, mid_m, slow_m, reset_secs);
                    best_alert = Some((mid_m, lbl, compact));
                }
            }
            Some(WindowEstimateOutcome::RunsOutIn { alert: false, .. })
            | Some(WindowEstimateOutcome::Insufficient) => {
                has_insufficient = true;
            }
            Some(WindowEstimateOutcome::WontRunOut) => {
                has_safe = true;
            }
            Some(WindowEstimateOutcome::Idle) => {
                has_idle = true;
            }
            None => {}
        }
    };
    for pe in &est.pools {
        if let Some(w) = &pe.weekly {
            check("weekly", "周", w);
        }
        if let Some(w) = &pe.five_hour {
            check("fiveHour", "5h", w);
        }
    }
    if let Some(w) = &est.weekly {
        check("weekly", "周", w);
    }
    if let Some(w) = &est.five_hour {
        check("fiveHour", "5h", w);
    }
    if let Some((_, lbl, compact)) = best_alert {
        let txt = format!("⚠ {lbl} {compact}");
        (
            txt,
            Style::default()
                .fg(STATUS_FAILED)
                .add_modifier(Modifier::BOLD),
        )
    } else if has_safe {
        ("重置前够用".to_string(), muted())
    } else if has_insufficient {
        ("待观察".to_string(), muted())
    } else if has_idle {
        ("空闲".to_string(), muted())
    } else {
        ("-".to_string(), muted())
    }
}

fn render_quota_accounts_pane(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    show_procs: bool,
) {
    let focused = app.focus == FocusPane::List && matches!(app.overlay, Overlay::None);
    let slot_count = app
        .filtered_accounts
        .iter()
        .filter(|r| matches!(r, AccountRow::Slot(_)))
        .count();
    let has_auto = app
        .filtered_accounts
        .iter()
        .any(|r| matches!(r, AccountRow::Auto));
    let count_lbl = if has_auto {
        format!("{slot_count} + auto")
    } else {
        format!("{slot_count}")
    };
    let title_text = if app.filter.is_empty() {
        if area.width >= 56 {
            format!(" agy 账号槽位 ({count_lbl} · 按 u 切号/选 auto) ")
        } else {
            format!(" agy 账号 ({count_lbl} · 按 u 切号) ")
        }
    } else {
        format!(" agy 账号 ({count_lbl}) · 过滤 ")
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

    let iw = inner.width as usize;
    if app.filtered_accounts.is_empty() {
        let an = &app.snapshot.analytics;
        let msg = if !app.filter.is_empty() {
            format!("无匹配“{}”的 agy 账号", app.filter)
        } else {
            format!(
                "未配置多账号 · 本地近7d账本 {} · 运行 delegate agy account add 接入",
                format_tokens_short(an.by_purpose.total())
            )
        };
        let lines: Vec<Line> = wrap_text(&msg, iw.saturating_sub(2))
            .into_iter()
            .map(|l| Line::from(Span::styled(format!(" {l}"), muted())))
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }

    let procs = &app.snapshot.agy_accounts.running_agy;
    let procs_per_row = if iw >= 96 { 2 } else { 1 };
    let proc_reserve = if show_procs && !procs.is_empty() && inner.height >= 7 {
        (2 + procs.len().div_ceil(procs_per_row)).min(6)
    } else {
        0
    };
    let show_header = inner.height >= 4;
    let header_h = usize::from(show_header);
    let view_h = (inner.height as usize)
        .saturating_sub(header_h + proc_reserve)
        .max(1);

    if let Some(sel) = app.selected_account {
        if sel < app.accounts_scroll {
            app.accounts_scroll = sel;
        } else if sel >= app.accounts_scroll + view_h {
            app.accounts_scroll = sel + 1 - view_h;
        }
    }
    let max_scroll = app.filtered_accounts.len().saturating_sub(view_h);
    if app.accounts_scroll > max_scroll {
        app.accounts_scroll = max_scroll;
    }

    let status_w = 6usize;
    let show_pct = iw >= 50;
    let pct_w = if show_pct { 6 } else { 0 }; // "周 41%" / "5h 97%"
    let warn_w = if iw >= 66 { 13 } else { 0 };
    let u24_w = if iw >= 42 { 7 } else { 0 };
    let used_w = if iw >= 76 { 8 } else { 0 };
    let right_w = (if show_pct { 1 + pct_w + 1 + pct_w } else { 0 })
        + (if warn_w > 0 { 1 + warn_w } else { 0 })
        + (if u24_w > 0 { 1 + u24_w } else { 0 })
        + (if used_w > 0 { 1 + used_w } else { 0 });
    let id_w = iw.saturating_sub(2 + status_w + 1 + right_w).max(6);

    let mut lines: Vec<Line> = Vec::with_capacity(inner.height as usize);
    if show_header {
        let mut hdr = format!(
            "  {} {}",
            pad_right("状态", status_w),
            pad_right("账号", id_w)
        );
        if show_pct {
            hdr.push_str(&format!(
                " {} {}",
                pad_left("周剩", pct_w),
                pad_left("5h剩", pct_w)
            ));
        }
        if warn_w > 0 {
            hdr.push_str(&format!(" {}", pad_right("耗尽预警", warn_w)));
        }
        if u24_w > 0 {
            hdr.push_str(&format!(" {}", pad_left("24h量", u24_w)));
        }
        if used_w > 0 {
            hdr.push_str(&format!(" {}", pad_left("最后用", used_w)));
        }
        lines.push(Line::from(Span::styled(truncate(&hdr, iw), muted())));
    }

    let is_auto_mode = app.snapshot.agy_accounts.auto_switch.enabled;
    for (pos, &row_kind) in app
        .filtered_accounts
        .iter()
        .enumerate()
        .skip(app.accounts_scroll)
        .take(view_h)
    {
        let selected = app.selected_account == Some(pos);
        let stripe = if selected {
            Span::styled(
                "▎ ",
                Style::default()
                    .fg(STATUS_QUEUED)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("  ")
        };
        let acct_idx = match row_kind {
            AccountRow::Auto => {
                let (badge_txt, badge_style) = if is_auto_mode {
                    (
                        pad_right("★ 自动", status_w),
                        Style::default()
                            .fg(STATUS_RUNNING)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    (pad_right("  自动", status_w), muted())
                };
                let asw = &app.snapshot.agy_accounts.auto_switch;
                let summary_txt = if !asw.pool_summary.is_empty() {
                    asw.pool_summary
                        .iter()
                        .map(|p| {
                            let target = if !p.selected.is_empty() {
                                &p.selected
                            } else {
                                &p.current
                            };
                            format!("{}→{}({})", p.label, target, p.tier_label)
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                } else if !asw.pool_current.is_empty() {
                    asw.pool_current
                        .iter()
                        .map(|(k, v)| format!("{k}→{v}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    format!("5h≥{}% 7d≥{}% · {}m", asw.five_hour_min_pct, asw.weekly_min_pct, asw.refresh_minutes)
                };
                let rest_w = iw.saturating_sub(2 + status_w + 1);
                let row_text = format!("auto  {summary_txt}");
                lines.push(Line::from(vec![
                    stripe,
                    Span::styled(badge_txt, badge_style),
                    Span::raw(" "),
                    Span::styled(
                        truncate(&row_text, rest_w),
                        if selected || is_auto_mode {
                            bold_primary()
                        } else {
                            primary()
                        },
                    ),
                ]));
                continue;
            }
            AccountRow::Slot(idx) => idx,
        };
        let acct = &app.snapshot.agy_accounts.accounts[acct_idx];
        let (badge_txt, badge_style) = if acct.active && !is_auto_mode {
            (
                pad_right("★ 活跃", status_w),
                Style::default()
                    .fg(STATUS_RUNNING)
                    .add_modifier(Modifier::BOLD),
            )
        } else if acct.active && is_auto_mode {
            (
                pad_right("  实盘", status_w),
                Style::default().fg(STATUS_QUEUED),
            )
        } else {
            (pad_right("  备用", status_w), muted())
        };
        let disp_name = acct.display_name(app.is_account_revealed(&acct.id));
        let mut spans = vec![
            stripe,
            Span::styled(badge_txt, badge_style),
            Span::raw(" "),
            Span::styled(
                pad_right(&truncate(&disp_name, id_w), id_w),
                if selected || acct.active {
                    bold_primary()
                } else {
                    primary()
                },
            ),
        ];
        if show_pct {
            let wk = find_account_pool_pct(acct, "gemini", "weekly")
                .map(|p| format!("{p}%"))
                .unwrap_or_else(|| "-".to_string());
            let fh = find_account_pool_pct(acct, "gemini", "fiveHour")
                .map(|p| format!("{p}%"))
                .unwrap_or_else(|| "-".to_string());
            spans.push(Span::raw(" "));
            spans.push(Span::styled(pad_left(&wk, pct_w), primary()));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(pad_left(&fh, pct_w), primary()));
        }
        if warn_w > 0 {
            let (w_txt, w_st) = earliest_depletion_cell(acct);
            spans.push(Span::raw(" "));
            spans.push(Span::styled(pad_right(&truncate(&w_txt, warn_w), warn_w), w_st));
        }
        if u24_w > 0 {
            let u24: u64 = acct.usage.hourly_24h.iter().sum();
            let u_txt = if u24 > 0 {
                format_tokens_short(u24)
            } else {
                format_tokens_short(acct.usage.five_hour.total)
            };
            spans.push(Span::raw(" "));
            spans.push(Span::styled(pad_left(&u_txt, u24_w), primary()));
        }
        if used_w > 0 {
            let used = format_iso_compact(&acct.last_used_at, &app.snapshot.today_ymd);
            spans.push(Span::raw(" "));
            spans.push(Span::styled(pad_left(&truncate(&used, used_w), used_w), muted()));
        }
        let line = if selected {
            Line::from(spans).style(Style::default().add_modifier(Modifier::BOLD))
        } else {
            Line::from(spans)
        };
        lines.push(line);
    }

    if proc_reserve > 0 {
        lines.push(Line::raw(""));
        lines.push(section_header(&format!("本机在跑 agy 进程 ({})", procs.len())));
        for pair in procs.chunks(procs_per_row) {
            if lines.len() >= inner.height as usize {
                break;
            }
            let txt = pair
                .iter()
                .map(format_running_agy_proc)
                .collect::<Vec<_>>()
                .join("   ");
            lines.push(Line::from(Span::styled(
                truncate(&format!("  {txt}"), iw),
                muted(),
            )));
        }
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_quota_7d_distribution_pane(frame: &mut Frame, app: &App, area: Rect) {
    let an = &app.snapshot.analytics;
    let total_7d = an.by_purpose.total();
    let title = format!(
        " 近 7 天 Token 分布（总计 {} · 横向条直读） ",
        format_tokens_short(total_7d)
    );
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let block = rounded_block(
        Line::from(Span::styled(truncate(&title, inner_w.max(8)), muted())),
        false,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let lines = build_7d_distribution_lines(app, inner_w, inner_h);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn build_7d_distribution_lines(app: &App, width: usize, max_rows: usize) -> Vec<Line<'static>> {
    let an = &app.snapshot.analytics;
    let bp = &an.by_purpose;
    let total_7d = bp.total();
    let mut lines: Vec<Line<'static>> = Vec::new();

    let bar_w = if width >= 54 { 16 } else { 10 };
    let val_w = 14usize; // " 2.54B  (73%)"

    lines.push(section_header("按用途"));
    let purposes = [
        ("写任务", bp.write, STATUS_RUNNING),
        ("自动审查", bp.review, STATUS_QUEUED),
        ("退回返工", bp.rework, STATUS_PENDING),
        ("只读侦察", bp.read_only, Color::DarkGray),
    ];
    let max_p = purposes.iter().map(|(_, v, _)| *v).max().unwrap_or(0).max(1);
    let p_lbl_w = 10usize;
    for (lbl, val, col) in purposes {
        let ratio = val as f64 / max_p as f64;
        let pct = (val * 100).checked_div(total_7d).unwrap_or(0);
        let (fill, empty) = build_horiz_bar(ratio, bar_w);
        let val_str = format!("{:>7} ({:>2}%)", format_tokens_short(val), pct);
        lines.push(Line::from(vec![
            Span::styled(format!("  {} ", pad_right(lbl, p_lbl_w)), primary()),
            Span::styled(fill, Style::default().fg(col)),
            Span::styled(empty, muted()),
            Span::styled(format!(" {}", truncate(&val_str, val_w)), muted()),
        ]));
    }

    lines.push(Line::from(Span::styled("─".repeat(width), muted())));
    lines.push(section_header(&format!(
        "按模型 (共 {} 个)",
        an.by_model.len()
    )));

    let has_breakdown = an.token_input_7d + an.token_output_7d + an.token_cache_read_7d > 0;
    let footer_reserve = if has_breakdown { 2 } else { 0 };
    let avail_model_rows = max_rows.saturating_sub(lines.len() + footer_reserve);

    if an.by_model.is_empty() {
        lines.push(Line::from(Span::styled("  暂无近 7 天模型用量记录", muted())));
    } else {
        let total_m: u64 = an.by_model.iter().map(|m| m.tokens).sum::<u64>().max(1);
        let max_m = an.by_model.iter().map(|m| m.tokens).max().unwrap_or(0).max(1);
        let m_lbl_w = width.saturating_sub(2 + bar_w + 1 + val_w + 2).clamp(10, 20);
        let m_bar_w = width.saturating_sub(2 + m_lbl_w + 1 + val_w + 1).clamp(6, bar_w);

        let should_fold =
            !app.quota_models_expanded && avail_model_rows > 0 && an.by_model.len() > avail_model_rows;
        let show_n = if should_fold {
            avail_model_rows.saturating_sub(1).max(1)
        } else {
            an.by_model.len()
        };

        for m in an.by_model.iter().take(show_n) {
            let ratio = m.tokens as f64 / max_m as f64;
            let pct = m.tokens * 100 / total_m;
            let (fill, empty) = build_horiz_bar(ratio, m_bar_w);
            let val_str = format!("{:>7} ({:>2}%)", format_tokens_short(m.tokens), pct);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {} ", pad_right(&truncate(&m.model, m_lbl_w), m_lbl_w)),
                    primary(),
                ),
                Span::styled(fill, Style::default().fg(STATUS_QUEUED)),
                Span::styled(empty, muted()),
                Span::styled(format!(" {}", truncate(&val_str, val_w)), muted()),
            ]));
        }
        if should_fold {
            let hidden = an.by_model.len().saturating_sub(show_n);
            lines.push(Line::from(Span::styled(
                truncate(
                    &format!("  ▸ 其余 {hidden} 个模型（按 Space 展开全部）"),
                    width,
                ),
                Style::default().fg(STATUS_QUEUED),
            )));
        }
    }

    if has_breakdown {
        let total_io = an
            .token_input_7d
            .saturating_add(an.token_output_7d)
            .saturating_add(an.token_cache_read_7d)
            .max(1);
        let hit_x10 = an.token_cache_read_7d.saturating_mul(1000) / total_io;
        let summary = format!(
            "本地账本：近7d 输入 {} / 输出 {} / 缓存命中 {} (命中率 {}.{}%)",
            format_tokens_short(an.token_input_7d),
            format_tokens_short(an.token_output_7d),
            format_tokens_short(an.token_cache_read_7d),
            hit_x10 / 10,
            hit_x10 % 10
        );
        lines.push(Line::from(Span::styled("─".repeat(width), muted())));
        lines.push(Line::from(Span::styled(truncate(&summary, width), muted())));
    }

    lines
}

fn format_pct_x100(x100: u32) -> String {
    let whole = x100 / 100;
    let frac = x100 % 100;
    if frac == 0 {
        format!("{whole}%")
    } else if frac.is_multiple_of(10) {
        format!("{whole}.{}%", frac / 10)
    } else {
        format!("{whole}.{frac:02}%")
    }
}

fn render_quota_auto_detail_pane(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    let asw = &app.snapshot.agy_accounts.auto_switch;
    let status_lbl = if asw.enabled { "已启用" } else { "未启用（按 u 切换为 auto）" };
    let title = format!(" {} auto 自动选号 · {status_lbl} ", if asw.enabled { "★" } else { "·" });
    let block = rounded_block(
        Line::from(Span::styled(
            truncate(&title, area.width.saturating_sub(2) as usize),
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
    let live_slot = app
        .snapshot
        .agy_accounts
        .active
        .as_deref()
        .unwrap_or("无");
    push_wrapped(
        &mut out,
        &format!(
            "阈值：5h≥{}% 7d≥{}% · 刷新 ≥{}m · 在跑写手预扣 {}% · 实盘 ~/.gemini 保持 {live_slot}",
            asw.five_hour_min_pct,
            asw.weekly_min_pct,
            asw.refresh_minutes,
            asw.per_run_reserve_pct,
        ),
        iw,
        muted(),
    );
    if asw.pool_summary.is_empty() {
        for (pool, cur) in &asw.pool_current {
            push_wrapped(&mut out, &format!("  {pool}：当前指针 {cur}"), iw, primary());
        }
    } else {
        for ps in &asw.pool_summary {
            let tier_style = match ps.tier.as_str() {
                "green" => Style::default().fg(STATUS_RUNNING).add_modifier(Modifier::BOLD),
                "yellow" => Style::default().fg(STATUS_PENDING).add_modifier(Modifier::BOLD),
                "red" => Style::default().fg(STATUS_FAILED).add_modifier(Modifier::BOLD),
                _ => primary(),
            };
            let target = if !ps.selected.is_empty() {
                &ps.selected
            } else {
                &ps.current
            };
            let head = format!(
                "{}：当前指针 {} → 下次新会话 {}（{}档）",
                ps.label,
                if ps.current.is_empty() { "-" } else { &ps.current },
                if target.is_empty() { "-" } else { target },
                if ps.tier_label.is_empty() { "-" } else { &ps.tier_label },
            );
            push_wrapped(&mut out, &head, iw, tier_style);
            if !ps.reason.is_empty() {
                push_indented_wrapped(&mut out, "  理由：", &ps.reason, iw, muted());
            }
        }
    }
    frame.render_widget(Paragraph::new(out), inner);
}

fn render_quota_account_detail_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::Detail && matches!(app.overlay, Overlay::None);
    if matches!(app.selected_account_row(), Some(AccountRow::Auto)) {
        render_quota_auto_detail_pane(frame, app, area, focused);
        return;
    }
    let Some(acct) = app.selected_account_item().cloned() else {
        let block = rounded_block(Line::from(Span::styled(" 账号额度池 ", muted())), focused);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " 暂无选中的 agy 账号（按 Q 刷新额度）",
                muted(),
            ))),
            inner,
        );
        return;
    };

    let revealed = app.is_account_revealed(&acct.id);
    let name = acct.display_name(revealed);
    let prefix = if acct.active { "★ " } else { "· " };
    let upd = if app.quota_refreshing {
        "刷新中…".to_string()
    } else if let Some(q) = &acct.quota {
        format_quota_updated(q.age_seconds)
    } else if let Some(age) = app.snapshot.agy_accounts.quota_age_seconds {
        format_quota_updated(age)
    } else {
        "未刷新".to_string()
    };
    let title = format!(" {prefix}{name} 额度池与 24h 走势 · {upd} ");
    let block = rounded_block(
        Line::from(Span::styled(
            truncate(&title, area.width.saturating_sub(2) as usize),
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
    let mut y = inner.y;
    let bottom = inner.y + inner.height;

    // Header meta line
    let used_at = format_iso_compact(&acct.last_used_at, &app.snapshot.today_ymd);
    let proc_n = app.snapshot.agy_accounts.running_agy.len();
    let label_part = if revealed && !acct.label.is_empty() {
        format!("标签 {} · ", acct.label)
    } else {
        String::new()
    };
    let asw = &app.snapshot.agy_accounts.auto_switch;
    let auto_switch_part = if asw.enabled {
        format!(
            " · 自动选号：开（5h≥{}% 7d≥{}% · {}m）",
            asw.five_hour_min_pct, asw.weekly_min_pct, asw.refresh_minutes
        )
    } else {
        " · 自动选号：关".to_string()
    };
    let meta_txt = format!(
        "{label_part}最后使用 {used_at} · 凭据 {} 份 · 在跑 agy 进程 {proc_n} 个{auto_switch_part}（按 u 手动切号）",
        acct.file_count
    );
    if y < bottom {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(truncate(&meta_txt, iw), muted()))),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
    }
    if let Some(err) = &app.account_switch_error {
        if y < bottom {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    truncate(&format!("⚠ 切换失败：{err}"), iw),
                    Style::default().fg(STATUS_FAILED),
                ))),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }
    }

    // Pool gauge rows
    if let Some(q) = &acct.quota {
        if q.identity_mismatch {
            if y < bottom {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        truncate("⚠ 身份不符，未更新（本机登录态与该槽位不匹配）", iw),
                        Style::default().fg(STATUS_PENDING),
                    ))),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                y += 1;
            }
        } else if q.ok && !q.entries.is_empty() {
            for e in &q.entries {
                if y >= bottom {
                    break;
                }
                let lbl = format!("{} · {}", short_pool_name(&e.pool), window_display_name(e));
                render_quota_gauge_row(frame, Rect::new(inner.x, y, inner.width, 1), &lbl, e);
                y += 1;
            }
            for ul in &q.unknown_lines {
                if y >= bottom {
                    break;
                }
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        truncate(&format!("  {ul}"), iw),
                        muted(),
                    ))),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                y += 1;
            }
        } else if let Some(err) = &q.error {
            if y < bottom {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        truncate(&format!("额度查询失败：{err}"), iw),
                        Style::default().fg(STATUS_FAILED),
                    ))),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                y += 1;
            }
        }
    } else if y < bottom {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("暂无额度快照（按 Q 刷新）", muted()))),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
    }

    // Manual quota note if present
    if let Some(note) = &acct.quota_note {
        let mut parts = Vec::new();
        if let Some(w) = note.weekly_pct_x100 {
            parts.push(format!("本周剩余 {}", format_pct_x100(w)));
        }
        if let Some(f) = note.five_hour_pct_x100 {
            parts.push(format!("5 小时剩余 {}", format_pct_x100(f)));
        }
        if !parts.is_empty() && y < bottom {
            let line_txt = format!("  手动记录: {}", parts.join(" · "));
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(truncate(&line_txt, iw), muted()))),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }
    }

    // Depletion estimate conclusion lines (wrap cleanly without mid-word truncation)
    for (line_txt, is_warn) in format_quota_estimate_lines(&acct, false) {
        let st = if is_warn {
            Style::default()
                .fg(STATUS_FAILED)
                .add_modifier(Modifier::BOLD)
        } else {
            muted()
        };
        for wl in wrap_text(&line_txt, iw.saturating_sub(2).max(12)) {
            if y >= bottom {
                break;
            }
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(format!("  {wl}"), st))),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }
    }

    // Local usage breakdown (5h / 7d) when non-zero
    let u5 = &acct.usage.five_hour;
    let u7 = &acct.usage.seven_day;
    if (u5.total > 0 || u7.total > 0) && y + 2 <= bottom {
        let running_note = if u5.uncounted_running > 0 {
            format!(" · 运行中未计 {}", u5.uncounted_running)
        } else {
            String::new()
        };
        let l5 = format!(
            "  近 5 小时: 输入 {} · 输出 {} · 缓存 {}{running_note}",
            format_tokens_short(u5.input),
            format_tokens_short(u5.output),
            format_tokens_short(u5.cache_read),
        );
        let l7 = format!(
            "  近 7 天: 输入 {} · 输出 {} · 缓存 {} · {} 次",
            format_tokens_short(u7.input),
            format_tokens_short(u7.output),
            format_tokens_short(u7.cache_read),
            u7.runs,
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(truncate(&l5, iw), muted()))),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
        if y < bottom {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(truncate(&l7, iw), muted()))),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }
    }

    // 24h Sparkline
    if y + 3 <= bottom {
        y += 1;
        let data = &acct.usage.hourly_24h;
        let sum24: u64 = data.iter().sum();
        let peak = data.iter().copied().max().unwrap_or(0);
        let hdr = format!(
            "▸ 近 24 小时每小时 Token 走势（共 {} · 近1h {} · 峰值 {}/h）",
            format_tokens_short(sum24),
            format_tokens_short(acct.usage.one_hour.total),
            format_tokens_short(peak)
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(truncate(&hdr, iw), bold_primary()))),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;

        let spark_h = bottom.saturating_sub(y + 1).clamp(1, 4);
        if !data.is_empty() && spark_h > 0 {
            let scaled: Vec<u64> = data
                .iter()
                .flat_map(|&v| std::iter::repeat_n(v, 2))
                .collect();
            let sw = (scaled.len() as u16).min(inner.width);
            let spark = Sparkline::default()
                .data(&scaled)
                .max(peak.max(1))
                .bar_set(bar::NINE_LEVELS)
                .style(Style::default().fg(if acct.active {
                    STATUS_RUNNING
                } else {
                    STATUS_QUEUED
                }));
            frame.render_widget(spark, Rect::new(inner.x, y, sw, spark_h));
            y += spark_h;
            if y < bottom {
                let axis = format_sparkline_axis(sw as usize);
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(truncate(&axis, iw), muted()))),
                    Rect::new(inner.x, y, inner.width, 1),
                );
            }
        }
    }
}

/// Side-by-side horizontal daily efficiency bars (zero `213分█` vertical-bar collision!).
fn render_quota_7d_efficiency_pane(frame: &mut Frame, app: &App, area: Rect) {
    let days = &app.snapshot.analytics.daily_7d;
    let (merged_7d, median_s, pass_pct) = summarize_7d_trend(days);
    let title = format!(
        " 近 7 天交付效率走势（合入 {merged_7d} 单 · 派发→合入中位 {} · 一轮通过率 {}） ",
        median_s
            .map(|s| format_duration(Some(s)))
            .unwrap_or_else(|| "-".to_string()),
        pass_pct
            .map(|p| format!("{p}%"))
            .unwrap_or_else(|| "-".to_string()),
    );
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let block = rounded_block(
        Line::from(Span::styled(truncate(&title, inner_w.max(8)), muted())),
        false,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let lines = build_7d_efficiency_lines(days, &app.snapshot.today_ymd, inner_w);
    let visible: Vec<Line> = lines.into_iter().take(inner_h).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn build_7d_efficiency_lines(
    days: &[DailyTrendPoint],
    today_ymd: &str,
    width: usize,
) -> Vec<Line<'static>> {
    if days.is_empty() {
        return vec![Line::from(Span::styled(" 暂无近 7 天交付记录", muted()))];
    }
    let max_mins = days
        .iter()
        .filter_map(|d| d.median_merge_secs)
        .map(|s| s.div_ceil(60))
        .max()
        .unwrap_or(1)
        .max(1);

    let side_by_side = width >= 66;
    let mut lines = Vec::new();
    if side_by_side {
        let half = (width.saturating_sub(3)) / 2;
        let bar_w = half.saturating_sub(17).clamp(6, 18);
        let hdr = format!(
            "{} │ {}",
            pad_right(" 派发→合入中位耗时", half),
            pad_right("一轮审查通过率", half),
        );
        lines.push(Line::from(Span::styled(truncate(&hdr, width), muted())));

        for d in days {
            let is_today = !today_ymd.is_empty() && d.ymd == today_ymd;
            let mins_val = d.median_merge_secs.map(|s| s.div_ceil(60));
            let (l_fill, l_empty) = match mins_val {
                Some(m) => build_horiz_bar(m as f64 / max_mins as f64, bar_w),
                None => (String::new(), "░".repeat(bar_w)),
            };
            let l_txt = match mins_val {
                Some(m) if is_today => format!("{m:>4}m*"),
                Some(m) => format!("{m:>4}m "),
                None => "    - ".to_string(),
            };
            let left_col_used = 1 + 5 + 1 + bar_w + 1 + display_width(&l_txt);
            let left_pad = half.saturating_sub(left_col_used);

            let (r_fill, r_empty) = match d.first_round_pass_pct {
                Some(p) => build_horiz_bar((p as f64 / 100.0).clamp(0.0, 1.0), bar_w),
                None => (String::new(), "░".repeat(bar_w)),
            };
            let r_txt = match d.first_round_pass_pct {
                Some(p) if is_today => format!("{p:>3}% (今日)"),
                Some(p) => format!("{p:>3}%"),
                None => "  -".to_string(),
            };
            let pass_col = match d.first_round_pass_pct {
                Some(p) if p >= 75 => STATUS_RUNNING,
                Some(p) if p >= 50 => STATUS_PENDING,
                Some(_) => STATUS_FAILED,
                None => Color::DarkGray,
            };

            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", pad_right(&d.date, 5)), muted()),
                Span::styled(l_fill, Style::default().fg(STATUS_QUEUED)),
                Span::styled(l_empty, muted()),
                Span::styled(format!(" {l_txt}"), if is_today { bold_primary() } else { primary() }),
                Span::raw(" ".repeat(left_pad)),
                Span::styled(" │ ", muted()),
                Span::styled(format!("{} ", pad_right(&d.date, 5)), muted()),
                Span::styled(r_fill, Style::default().fg(pass_col)),
                Span::styled(r_empty, muted()),
                Span::styled(format!(" {r_txt}"), if is_today { bold_primary() } else { primary() }),
            ]));
        }
    } else {
        // Single-column compact efficiency bars for narrow screens
        let bar_w = width.saturating_sub(24).clamp(6, 14);
        lines.push(Line::from(Span::styled(
            " 日期   耗时(中位)     一轮通过率",
            muted(),
        )));
        for d in days {
            let mins_txt = d
                .median_merge_secs
                .map(|s| format!("{}m", s.div_ceil(60)))
                .unwrap_or_else(|| "-".to_string());
            let (fill, empty) = match d.first_round_pass_pct {
                Some(p) => build_horiz_bar((p as f64 / 100.0).clamp(0.0, 1.0), bar_w),
                None => (String::new(), "░".repeat(bar_w)),
            };
            let pct_txt = d
                .first_round_pass_pct
                .map(|p| format!("{p:>3}%"))
                .unwrap_or_else(|| "  -".to_string());
            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", pad_right(&d.date, 5)), muted()),
                Span::styled(format!("{} ", pad_left(&mins_txt, 5)), primary()),
                Span::styled(fill, Style::default().fg(STATUS_RUNNING)),
                Span::styled(empty, muted()),
                Span::styled(format!(" {pct_txt}"), primary()),
            ]));
        }
    }
    lines
}

/// Narrow-screen (`< 110` cols) combined detail pane for `4 额度`:
/// selected account pools + 1-line 24h sparkline (when height < 30) + 7d distribution + 7d efficiency.
fn render_quota_narrow_detail(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    compact_spark: bool,
) {
    let focused = app.focus == FocusPane::Detail && matches!(app.overlay, Overlay::None);
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;

    let mut out: Vec<Line<'static>> = Vec::new();
    if matches!(app.selected_account_row(), Some(AccountRow::Auto)) {
        let asw = &app.snapshot.agy_accounts.auto_switch;
        let badge = if asw.enabled { " ★ auto 已启用 " } else { " · auto 未启用 " };
        out.push(Line::from(vec![
            Span::styled(
                badge,
                Style::default()
                    .fg(if asw.enabled { STATUS_RUNNING } else { STATUS_QUEUED })
                    .add_modifier(Modifier::REVERSED | Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                format!(
                    "5h≥{}% 7d≥{}% · {}m",
                    asw.five_hour_min_pct, asw.weekly_min_pct, asw.refresh_minutes
                ),
                muted(),
            ),
        ]));
        for ps in &asw.pool_summary {
            push_wrapped(
                &mut out,
                &format!(
                    "  {}：{} → {}（{}档） {}",
                    ps.label, ps.current, ps.selected, ps.tier_label, ps.reason
                ),
                inner_w,
                primary(),
            );
        }
    } else if let Some(acct) = app.selected_account_item().cloned() {
        let name = acct.display_name(app.is_account_revealed(&acct.id));
        let prefix = if acct.active { "★ 活跃" } else { "· 备用" };
        let upd = if app.quota_refreshing {
            "刷新中…".to_string()
        } else if let Some(q) = &acct.quota {
            format_quota_updated(q.age_seconds)
        } else if let Some(age) = app.snapshot.agy_accounts.quota_age_seconds {
            format_quota_updated(age)
        } else {
            "未刷新".to_string()
        };
        out.push(Line::from(vec![
            Span::styled(
                format!(" {prefix} "),
                Style::default()
                    .fg(if acct.active { STATUS_RUNNING } else { STATUS_QUEUED })
                    .add_modifier(Modifier::REVERSED | Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(name, bold_primary()),
            Span::styled(format!(" · {upd}"), muted()),
        ]));

        if let Some(q) = &acct.quota {
            if q.identity_mismatch {
                push_wrapped(
                    &mut out,
                    "⚠ 身份不符，未更新（本机登录态与该槽位不匹配）",
                    inner_w,
                    Style::default().fg(STATUS_PENDING),
                );
            } else if q.ok {
                let bar_w = inner_w.saturating_sub(44).clamp(8, 16);
                for e in &q.entries {
                    let ratio = (e.remaining_pct_x100 as f64 / 10000.0).clamp(0.0, 1.0);
                    let (fill, empty) = build_horiz_bar(ratio, bar_w);
                    let pct = (e.remaining_pct_x100 + 50) / 100;
                    let reset = crate::ui::format_reset_countdown_zh(e.reset_in_seconds);
                    let full_lbl =
                        format!("{} · {}", short_pool_name(&e.pool), window_display_name(e));
                    let lbl = pad_right(&truncate(&full_lbl, 20), 20);
                    out.push(Line::from(vec![
                        Span::styled(format!(" {lbl} "), muted()),
                        Span::styled(fill, Style::default().fg(crate::ui::quota_bar_color(e.remaining_pct_x100))),
                        Span::styled(empty, muted()),
                        Span::styled(format!(" 剩余 {pct:>3}% · {reset}"), primary()),
                    ]));
                }
            }
        }
        for (txt, is_warn) in format_quota_estimate_lines(&acct, true) {
            let st = if is_warn {
                Style::default()
                    .fg(STATUS_FAILED)
                    .add_modifier(Modifier::BOLD)
            } else {
                muted()
            };
            push_indented_wrapped(&mut out, "  ", &txt, inner_w, st);
        }

        let u24: u64 = acct.usage.hourly_24h.iter().sum();
        let spark = inline_spark_text(&acct.usage.hourly_24h, if compact_spark { 12 } else { 18 });
        push_wrapped(
            &mut out,
            &format!(
                "24h 用量: {}  {spark}  近1h {}",
                format_tokens_short(u24),
                format_tokens_short(acct.usage.one_hour.total)
            ),
            inner_w,
            primary(),
        );
        out.push(Line::raw(""));
    }

    out.extend(build_7d_distribution_lines(app, inner_w, 40));
    out.push(Line::raw(""));
    out.push(section_header("近 7 天交付效率"));
    out.extend(build_7d_efficiency_lines(
        &app.snapshot.analytics.daily_7d,
        &app.snapshot.today_ymd,
        inner_w,
    ));

    let max_scroll = out.len().saturating_sub(inner_h);
    if app.detail_scroll > max_scroll {
        app.detail_scroll = max_scroll;
    }
    let scroll_hint = if out.len() > inner_h && inner_h > 0 {
        format!(" {}/{} ", app.detail_scroll + 1, out.len())
    } else {
        String::new()
    };
    let mut block = rounded_block(
        Line::from(Span::styled(
            " 额度详情与 7d 分布/效率 ",
            if focused { bold_primary() } else { muted() },
        )),
        focused,
    );
    if !scroll_hint.is_empty() {
        block = block.title_bottom(Line::from(Span::styled(scroll_hint, muted())));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let visible: Vec<Line> = out
        .into_iter()
        .skip(app.detail_scroll)
        .take(inner_h)
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);
}
