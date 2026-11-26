#![forbid(unsafe_code)]
//! `1 总览` — Global 5-card dashboard answering the owner's 5 questions on one screen:
//! ① 需要关注 (owner decisions + fresh controller pending + stale fold)
//! ② 正在运行与波次预估 (running/queued runs + wave p50/p75 ETA + dag queue)
//! ③ 工作表推进链 (active work chain tree + timed/deferred footnote)
//! ④ 额度与消耗 (active/backup quota bars + depletion warnings + 24h mini spark + 7d summary)
//! ⑤ 最近交付 (all 7d merged deliveries from history, including cleaned runs, scrollable)
//!
//! Wide (`>= 110`): 53% left (①②③) + 47% right (④⑤) with dynamic height donation.
//! Narrow (`< 110`): single-column scrollable stream in priority order.

use crate::app::{App, OverviewAttentionEntry, OverviewFocus, Overlay, WorkRow};
use crate::model::*;
use crate::ui::{
    bold_primary, build_horiz_bar, evaluate_window_outcome, format_quota_updated,
    format_reset_countdown_zh, format_tokens_short, format_tree_prefix_wide, inline_spark_text,
    muted, primary, quota_bar_color, render_quota_bar_spans, rounded_block, run_glyph_color,
    short_pool_name, short_repo_display, work_item_glyph_color, WindowEstimateOutcome,
    NARROW_BREAKPOINT, STATUS_FAILED, STATUS_PENDING, STATUS_QUEUED, STATUS_RUNNING,
};
use crate::width::{display_width, pad_left, pad_right, truncate};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

pub(crate) fn render_overview_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.width < NARROW_BREAKPOINT {
        render_overview_narrow(frame, app, area);
        return;
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(53), Constraint::Percentage(47)])
        .split(area);

    let h = area.height as usize;

    // Gather card contents ahead of layout so heights donate cleanly without blank space.
    let attention_entries = app.overview_attention_entries();
    let running_entries = app.overview_running_entries();
    let dag_pending_count = app
        .snapshot
        .dag_queue
        .iter()
        .filter(|q| matches!(q.status.as_str(), "pending" | "paused"))
        .count();
    let work_rows = app.overview_work_chain_rows();
    let deliveries = app.overview_delivery_entries();

    // Left column dynamic height allocation (exact algorithm from spec):
    let need_1: usize = if attention_entries.is_empty() {
        3
    } else {
        attention_entries.len() + 2
    };
    let need_2: usize = if running_entries.is_empty() && dag_pending_count == 0 {
        3
    } else {
        1 + running_entries.len() + 1 + 2 // col header + rows + queue note + borders
    };
    let need_3: usize = work_rows.len().max(1) + 2 + 2; // rows + divider + timed/deferred footnote + borders

    let (h1, h2, h3) = allocate_left_heights(h, need_1, need_2, need_3);

    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(h1 as u16),
            Constraint::Length(h2 as u16),
            Constraint::Min(h3 as u16),
        ])
        .split(cols[0]);

    render_attention_card(frame, app, &attention_entries, left_chunks[0]);
    render_running_card(frame, app, &running_entries, dag_pending_count, left_chunks[1]);
    render_work_chains_card(frame, app, &work_rows, left_chunks[2]);

    // Right column dynamic height allocation:
    let acct_count = app.snapshot.agy_accounts.accounts.len();
    let need_4: usize = if acct_count == 0 {
        4
    } else {
        acct_count * 4 + 3 + 2 // per account (1 header + 3 pool lines) + divider + 2 summary lines + borders
    };
    let h4 = if need_4 + 7 <= h {
        need_4
    } else {
        let cap = (h * 55 / 100).max(4);
        need_4.min(cap).min(h.saturating_sub(7).max(4))
    };
    let h5 = h.saturating_sub(h4);

    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(h4 as u16), Constraint::Min(h5 as u16)])
        .split(cols[1]);

    render_quota_summary_card(frame, app, right_chunks[0]);
    render_recent_deliveries_card(frame, app, &deliveries, right_chunks[1]);
}

/// Dynamic 3-card height donation algorithm for the left column of `1 总览`.
/// Empty cards stay strictly at 3 rows; surplus height goes entirely to ③ 工作表推进链.
pub(crate) fn allocate_left_heights(
    total_h: usize,
    need_1: usize,
    need_2: usize,
    need_3: usize,
) -> (usize, usize, usize) {
    if need_1 + need_2 + need_3 <= total_h {
        let h1 = need_1;
        let h2 = need_2;
        let h3 = total_h.saturating_sub(h1 + h2);
        return (h1, h2, h3);
    }
    let mut h1 = if need_1 <= 3 {
        need_1
    } else {
        need_1.min((total_h * 28 / 100).max(5))
    };
    let mut h2 = if need_2 <= 3 {
        need_2
    } else {
        need_2.min((total_h * 32 / 100).max(5))
    };
    let mut h3 = total_h.saturating_sub(h1 + h2);
    if h3 < 6 && total_h >= 12 {
        let mut deficit = 6 - h3;
        let s1 = h1.saturating_sub(5);
        let s2 = h2.saturating_sub(5);
        let avail = s1 + s2;
        if let Some(share1) = (deficit * s1).checked_div(avail) {
            let take1 = share1.min(s1);
            let take2 = (deficit - take1).min(s2);
            h1 -= take1;
            h2 -= take2;
            deficit = deficit.saturating_sub(take1 + take2);
            if deficit > 0 && h1 > 5 {
                let extra = deficit.min(h1 - 5);
                h1 -= extra;
                deficit -= extra;
            }
            if deficit > 0 && h2 > 5 {
                let extra = deficit.min(h2 - 5);
                h2 -= extra;
            }
        }
        h3 = total_h.saturating_sub(h1 + h2);
    }
    (h1, h2, h3)
}

fn render_attention_card(
    frame: &mut Frame,
    app: &mut App,
    entries: &[OverviewAttentionEntry],
    area: Rect,
) {
    let focused =
        app.overview_focus == OverviewFocus::Attention && matches!(app.overlay, Overlay::None);
    let pulse = app.snapshot.effective_pulse();
    let title_str = format!(
        " 需要关注 (等你 {} · 待主控 {}) ",
        pulse.owner_open, pulse.controller_pending
    );
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;

    let mut block = rounded_block(
        Line::from(Span::styled(
            truncate(&title_str, inner_w.max(8)),
            if focused { bold_primary() } else { muted() },
        )),
        focused,
    );

    if entries.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width > 0 && inner.height > 0 {
            let ok_msg = " ✓ 所有者待拍板 0 · 待主控 0（当前无阻塞）";
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    truncate(ok_msg, inner_w),
                    Style::default().fg(STATUS_RUNNING),
                ))),
                inner,
            );
        }
        return;
    }

    if app.overview_attention_sel >= entries.len() {
        app.overview_attention_sel = entries.len().saturating_sub(1);
    }
    let sel = app.overview_attention_sel;
    if sel < app.overview_attention_scroll {
        app.overview_attention_scroll = sel;
    } else if inner_h > 0 && sel >= app.overview_attention_scroll + inner_h {
        app.overview_attention_scroll = sel + 1 - inner_h;
    }
    let max_scroll = entries.len().saturating_sub(inner_h);
    if app.overview_attention_scroll > max_scroll {
        app.overview_attention_scroll = max_scroll;
    }

    if entries.len() > inner_h && inner_h > 0 {
        let first = app.overview_attention_scroll + 1;
        let last = (app.overview_attention_scroll + inner_h).min(entries.len());
        block = block.title_bottom(Line::from(Span::styled(
            format!(" {first}-{last}/{} ", entries.len()),
            muted(),
        )));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut lines = Vec::with_capacity(inner_h);
    for (idx, ent) in entries
        .iter()
        .enumerate()
        .skip(app.overview_attention_scroll)
        .take(inner_h)
    {
        let is_sel = focused && idx == sel;
        let line = match ent {
            OverviewAttentionEntry::OwnerWork { work_idx } => {
                let w = &app.snapshot.work[*work_idx];
                let (glyph, col) = work_item_glyph_color(w);
                let live = app.snapshot.work_live_status(w);
                let txt = format!(
                    "{glyph} {} [{}] {} · {}",
                    pad_right(w.state_badge(), 6),
                    w.id,
                    w.display_title(),
                    live
                );
                style_overview_row(&txt, col.unwrap_or(STATUS_PENDING), is_sel, inner_w)
            }
            OverviewAttentionEntry::PendingRun { repo_idx, run_idx } => {
                let r = &app.snapshot.repos[*repo_idx].runs[*run_idx];
                let (glyph, col) = run_glyph_color(r);
                let short_repo = short_repo_display(&r.repo_path, &r.repo_label);
                let wid_tag = app
                    .snapshot
                    .work_id_for_run(r)
                    .map(|id| format!(" [{id}]"))
                    .unwrap_or_default();
                let mut meta = Vec::new();
                if let Some(rev) = &r.review {
                    let sn = rev.row_snippet();
                    if !sn.is_empty() {
                        meta.push(sn);
                    }
                }
                if let Some(pm) = &r.premerge {
                    let sn = pm.row_snippet();
                    if !sn.is_empty() {
                        meta.push(sn);
                    }
                }
                let (f, add, del) = r.display_changes();
                if f > 0 {
                    if add > 0 || del > 0 {
                        meta.push(format!("{f}f +{add}/-{del}"));
                    } else {
                        meta.push(format!("{f}f"));
                    }
                }
                let age = r.last_activity_seconds();
                if age != u64::MAX {
                    meta.push(format!("卡主控 {}", format_duration(Some(age))));
                }
                let meta_str = meta.join(" · ");
                let txt = format!(
                    "{glyph} {} {} {}{wid_tag}  {meta_str}",
                    pad_right(&r.state_badge(), 6),
                    pad_right(&truncate(&short_repo, 10), 10),
                    r.display_name
                );
                style_overview_row(&txt, col.unwrap_or(STATUS_PENDING), is_sel, inner_w)
            }
            OverviewAttentionEntry::StaleFold { count } => {
                let arrow = if app.overview_stale_expanded { "▾" } else { "▸" };
                let txt = format!(
                    "{arrow} 更早陈旧待处理 {count} 条（静默 >24h，Space 展开或按 3 查看）"
                );
                style_overview_row(&txt, Color::DarkGray, is_sel, inner_w)
            }
        };
        lines.push(line);
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_running_card(
    frame: &mut Frame,
    app: &mut App,
    entries: &[(usize, usize)],
    dag_pending: usize,
    area: Rect,
) {
    let focused =
        app.overview_focus == OverviewFocus::Running && matches!(app.overlay, Overlay::None);
    let pulse = app.snapshot.effective_pulse();
    let wave = &app.snapshot.eta.wave;
    let mut title_parts = vec![
        format!("在跑 {}", pulse.running_runs),
        format!("排队 {}", pulse.queued_runs),
    ];
    if let Some(ref early) = wave.earliest_p50_clock {
        title_parts.push(format!("最快 {early}"));
    }
    if let Some(ref p50) = wave.wave_p50_clock {
        let p75 = wave
            .wave_p75_clock
            .as_deref()
            .map(|c| format!(" / p75 {c}"))
            .unwrap_or_default();
        title_parts.push(format!("本波 {p50}{p75}"));
    }
    let title_str = format!(" 正在运行与波次预估 ({}) ", title_parts.join(" · "));
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;

    let mut block = rounded_block(
        Line::from(Span::styled(
            truncate(&title_str, inner_w.max(8)),
            if focused { bold_primary() } else { muted() },
        )),
        focused,
    );

    if entries.is_empty() && dag_pending == 0 {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width > 0 && inner.height > 0 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    truncate(" 空闲 · 无在跑或排队运行", inner_w),
                    muted(),
                ))),
                inner,
            );
        }
        return;
    }

    let show_header = inner_h >= 3;
    let show_footer = inner_h >= 2;
    let list_h = inner_h.saturating_sub(usize::from(show_header) + usize::from(show_footer));

    if app.overview_running_sel >= entries.len() {
        app.overview_running_sel = entries.len().saturating_sub(1);
    }
    let sel = app.overview_running_sel;
    if sel < app.overview_running_scroll {
        app.overview_running_scroll = sel;
    } else if list_h > 0 && sel >= app.overview_running_scroll + list_h {
        app.overview_running_scroll = sel + 1 - list_h;
    }
    let max_scroll = entries.len().saturating_sub(list_h);
    if app.overview_running_scroll > max_scroll {
        app.overview_running_scroll = max_scroll;
    }
    if entries.len() > list_h && list_h > 0 {
        let first = app.overview_running_scroll + 1;
        let last = (app.overview_running_scroll + list_h).min(entries.len());
        block = block.title_bottom(Line::from(Span::styled(
            format!(" {first}-{last}/{} ", entries.len()),
            muted(),
        )));
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let state_w = 7usize;
    let repo_w = 10usize;
    let elapsed_w = 6usize;
    let eta_w = 6usize;
    let act_w = if inner_w >= 82 { 18 } else { 0 };
    let fixed = 4
        + state_w
        + 1
        + repo_w
        + 1
        + 1
        + (if act_w > 0 { act_w + 1 } else { 0 })
        + elapsed_w
        + 1
        + eta_w;
    let name_w = inner_w.saturating_sub(fixed).max(10);

    let mut lines = Vec::with_capacity(inner_h);
    if show_header {
        let mut hdr = format!(
            "    {} {} {}",
            pad_right("状态", state_w),
            pad_right("仓库", repo_w),
            pad_right("任务", name_w),
        );
        if act_w > 0 {
            hdr.push(' ');
            hdr.push_str(&pad_right("当前动作", act_w));
        }
        hdr.push_str(&format!(
            " {} {}",
            pad_left("已跑", elapsed_w),
            pad_left("预计", eta_w),
        ));
        lines.push(Line::from(Span::styled(truncate(&hdr, inner_w), muted())));
    }

    for (idx, &(ri, rj)) in entries
        .iter()
        .enumerate()
        .skip(app.overview_running_scroll)
        .take(list_h)
    {
        let r = &app.snapshot.repos[ri].runs[rj];
        let is_sel = focused && idx == sel;
        let (glyph, col) = run_glyph_color(r);
        let short_repo = short_repo_display(&r.repo_path, &r.repo_label);
        let elapsed = format_duration(r.elapsed_seconds.or(r.age_seconds));
        let eta = r
            .eta_badge()
            .map(|(b, _)| b.to_string())
            .unwrap_or_else(|| "-".to_string());
        let act_text = r
            .activity
            .as_ref()
            .map(|a| {
                if let Some(cmd) = &a.cmd {
                    let sc = shorten_running_cmd(cmd);
                    if !sc.is_empty() {
                        return format!("跑 {sc}");
                    }
                }
                a.label.clone()
            })
            .unwrap_or_else(|| "-".to_string());

        let mut row_txt = format!(
            "{glyph} {} {} {}",
            pad_right(&r.state_badge(), state_w),
            pad_right(&truncate(&short_repo, repo_w), repo_w),
            pad_right(&truncate(&r.display_name, name_w), name_w),
        );
        if act_w > 0 {
            row_txt.push(' ');
            row_txt.push_str(&pad_right(&truncate(&act_text, act_w), act_w));
        }
        row_txt.push_str(&format!(
            " {} {}",
            pad_left(&truncate(&elapsed, elapsed_w), elapsed_w),
            pad_left(&truncate(&eta, eta_w), eta_w),
        ));
        lines.push(style_overview_row(
            &row_txt,
            col.unwrap_or(STATUS_RUNNING),
            is_sel,
            inner_w,
        ));
    }

    if show_footer {
        let footer = if dag_pending > 0 {
            format!("待续派 {dag_pending} 项（按 3 查看续派队列）")
        } else if pulse.queued_runs > 0 {
            format!("容量排队 {} 项 · 无待触发续派", pulse.queued_runs)
        } else {
            "无容量排队 · 无待触发续派".to_string()
        };
        lines.push(Line::from(Span::styled(
            truncate(&format!(" {footer}"), inner_w),
            muted(),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_work_chains_card(
    frame: &mut Frame,
    app: &mut App,
    rows: &[WorkRow],
    area: Rect,
) {
    let focused =
        app.overview_focus == OverviewFocus::WorkChains && matches!(app.overlay, Overlay::None);
    let pulse = app.snapshot.effective_pulse();
    let wc = app.snapshot.work_counts();
    let we = &app.snapshot.eta.work;

    let mut tparts = vec![format!("推进 {} 项", pulse.work_active_pool)];
    if we.write_chains > 0 {
        tparts.push(format!("{} 条活跃链", we.write_chains));
    }
    if !we.dev_wall_range.is_empty() {
        tparts.push(format!("剩余 {}", we.dev_wall_range));
    }
    if let Some(ref cl) = we.earliest_finish_p50_clock {
        tparts.push(format!("活跃链预计 {cl}"));
    }
    let title_str = format!(" 工作表推进链 ({}) ", tparts.join(" · "));
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;

    let mut block = rounded_block(
        Line::from(Span::styled(
            truncate(&title_str, inner_w.max(8)),
            if focused { bold_primary() } else { muted() },
        )),
        focused,
    );

    let has_footnote = pulse.work_timed > 0 || pulse.work_deferred > 0;
    let foot_h = usize::from(has_footnote && inner_h >= 3);
    let list_h = inner_h.saturating_sub(foot_h);

    if rows.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width > 0 && inner.height > 0 {
            let mut lines = vec![Line::from(Span::styled(
                truncate(
                    &format!(
                        " 当前无推进中工作链（共完成 {} 项，按 2 查看工作表）",
                        wc.done_total
                    ),
                    inner_w,
                ),
                muted(),
            ))];
            if has_footnote && inner_h >= 2 {
                lines.push(build_timed_deferred_footnote(app, inner_w));
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
        return;
    }

    if app.overview_work_sel >= rows.len() {
        app.overview_work_sel = rows.len().saturating_sub(1);
    }
    let sel = app.overview_work_sel;
    if sel < app.overview_work_scroll {
        app.overview_work_scroll = sel;
    } else if list_h > 0 && sel >= app.overview_work_scroll + list_h {
        app.overview_work_scroll = sel + 1 - list_h;
    }
    let max_scroll = rows.len().saturating_sub(list_h);
    if app.overview_work_scroll > max_scroll {
        app.overview_work_scroll = max_scroll;
    }
    if rows.len() > list_h && list_h > 0 {
        let first = app.overview_work_scroll + 1;
        let last = (app.overview_work_scroll + list_h).min(rows.len());
        block = block.title_bottom(Line::from(Span::styled(
            format!(" {first}-{last}/{} ", rows.len()),
            muted(),
        )));
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut lines = Vec::with_capacity(inner_h);
    for (idx, row) in rows
        .iter()
        .enumerate()
        .skip(app.overview_work_scroll)
        .take(list_h)
    {
        let WorkRow::Item {
            work_idx,
            depth,
            branch_guides,
            is_last_sibling,
            ..
        } = row
        else {
            continue;
        };
        let w = &app.snapshot.work[*work_idx];
        let is_sel = focused && idx == sel;
        let (glyph, col) = work_item_glyph_color(w);
        let tree_prefix = format_tree_prefix_wide(*depth, branch_guides, *is_last_sibling);
        let live_raw = app.snapshot.work_live_status(w);
        let (live_merged, eta_opt) = crate::ui::merge_work_live_and_eta(w, &live_raw, false);
        let status_full = match eta_opt {
            Some((tag, _)) => format!("{live_merged}{tag}"),
            None => live_merged,
        };

        let status_w = if inner_w >= 86 { 28 } else { 18 };
        let state_w = 6usize;
        let repo_w = if inner_w >= 76 { 9 } else { 0 };
        let id_str = truncate(&w.id, 8);
        let lead = format!("{tree_prefix}{glyph} {id_str}");
        let lead_w = display_width(&lead);
        let fixed = 2 + lead_w + 1 + (if repo_w > 0 { repo_w + 1 } else { 0 }) + 1 + state_w + 1 + status_w;
        let title_w = inner_w.saturating_sub(fixed).max(8);

        let mut txt = format!("{lead} ");
        if repo_w > 0 {
            txt.push_str(&pad_right(&truncate(&w.repo_label, repo_w), repo_w));
            txt.push(' ');
        }
        txt.push_str(&format!(
            "{} {} {}",
            pad_right(&truncate(w.display_title(), title_w), title_w),
            pad_right(w.state_badge(), state_w),
            truncate(&status_full, status_w),
        ));
        lines.push(style_overview_row(
            &txt,
            col.unwrap_or(Color::Reset),
            is_sel,
            inner_w,
        ));
    }

    if foot_h > 0 {
        while lines.len() < list_h {
            lines.push(Line::raw(""));
        }
        lines.push(build_timed_deferred_footnote(app, inner_w));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn build_timed_deferred_footnote(app: &App, inner_w: usize) -> Line<'static> {
    let pulse = app.snapshot.effective_pulse();
    let we = &app.snapshot.eta.work;
    let mut parts = Vec::new();
    if pulse.work_timed > 0 {
        match we.earliest_date_wait_clock.as_deref() {
            Some(c) => parts.push(format!("定时等待 {} 项（最早 {c}）", pulse.work_timed)),
            None => parts.push(format!("定时等待 {} 项", pulse.work_timed)),
        }
    }
    if pulse.work_deferred > 0 {
        match we.earliest_deferred_clock.as_deref() {
            Some(c) => parts.push(format!("暂缓 {} 项（最早 {c}）", pulse.work_deferred)),
            None => parts.push(format!("暂缓 {} 项", pulse.work_deferred)),
        }
    }
    let txt = format!(" 另有 {}（不入 ETA）→ 按 2 详", parts.join(" · "));
    Line::from(Span::styled(truncate(&txt, inner_w), muted()))
}

fn render_quota_summary_card(frame: &mut Frame, app: &App, area: Rect) {
    let agy = &app.snapshot.agy_accounts;
    let an = &app.snapshot.analytics;
    let total_24h: u64 = an.hourly_24h_total.iter().sum();
    let total_7d = an.by_purpose.total();
    let upd = if app.quota_refreshing {
        "刷新中…".to_string()
    } else if let Some(age) = agy.quota_age_seconds {
        format_quota_updated(age)
    } else {
        "未同步".to_string()
    };
    let title_str = format!(
        " 额度与消耗 (近24h {} · 近7d {} · {upd}) ",
        format_tokens_short(total_24h),
        format_tokens_short(total_7d)
    );
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let block = rounded_block(
        Line::from(Span::styled(truncate(&title_str, inner_w.max(8)), muted())),
        false,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    if agy.accounts.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate(
                &format!(
                    " 本地账本：近24h {} · 近7d {}",
                    format_tokens_short(total_24h),
                    format_tokens_short(total_7d)
                ),
                inner_w,
            ),
            primary(),
        )));
        lines.push(Line::from(Span::styled(
            truncate(" 未配置 agy 账号 · 运行 delegate agy account add 接入额度监控", inner_w),
            muted(),
        )));
    } else {
        let mut sorted: Vec<&AgyAccountItem> = agy.accounts.iter().collect();
        sorted.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| a.id.cmp(&b.id)));
        for acct in sorted {
            let role = if acct.active {
                if agy.running_agy.is_empty() {
                    "活跃".to_string()
                } else {
                    format!("活跃·{}进程", agy.running_agy.len())
                }
            } else {
                "备用".to_string()
            };
            let prefix = if acct.active { "★" } else { "·" };
            let name = acct.display_name(app.is_account_revealed(&acct.id));
            let u24: u64 = acct.usage.hourly_24h.iter().sum();
            let spark = inline_spark_text(&acct.usage.hourly_24h, 10);
            let head = format!(
                "{prefix} {name} ({role})  24h {}  {spark}  近1h {}",
                format_tokens_short(u24),
                format_tokens_short(acct.usage.one_hour.total)
            );
            let head_style = if acct.active {
                Style::default()
                    .fg(STATUS_RUNNING)
                    .add_modifier(Modifier::BOLD)
            } else {
                bold_primary()
            };
            lines.push(Line::from(Span::styled(truncate(&head, inner_w), head_style)));

            if let Some(q) = &acct.quota {
                if q.identity_mismatch {
                    lines.push(Line::from(Span::styled(
                        truncate("  ⚠ 身份不符，未更新（按 4 核对）", inner_w),
                        Style::default().fg(STATUS_PENDING),
                    )));
                } else if q.ok && !q.entries.is_empty() {
                    let bar_w = if inner_w >= 68 { 14 } else { 10 };
                    for entry in q.entries.iter().take(3) {
                        let pname = short_pool_name(&entry.pool);
                        let wname = if entry.window == "fiveHour" { "5h" } else { "本周" };
                        let lbl = pad_right(&truncate(&format!("  {pname} {wname}"), 14), 15);
                        let pct = (entry.remaining_pct_x100 + 50) / 100;
                        let reset = format_reset_countdown_zh(entry.reset_in_seconds);
                        let warn_txt = find_entry_depletion_note(acct, entry);
                        let mut spans = vec![Span::styled(lbl, muted())];
                        spans.extend(render_quota_bar_spans(
                            (entry.remaining_pct_x100 as f64 / 10000.0).clamp(0.0, 1.0),
                            bar_w,
                            quota_bar_color(entry.remaining_pct_x100),
                        ));
                        let used_left = 15 + bar_w;
                        let right = format!(" {pct:>3}% · {reset}{warn_txt}");
                        let style = if warn_txt.contains('⚠') {
                            Style::default()
                                .fg(STATUS_FAILED)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            muted()
                        };
                        spans.push(Span::styled(
                            truncate(&right, inner_w.saturating_sub(used_left)),
                            style,
                        ));
                        lines.push(Line::from(spans));
                    }
                } else {
                    lines.push(Line::from(Span::styled("  额度未刷新（按 Q 刷新）", muted())));
                }
            } else {
                lines.push(Line::from(Span::styled("  额度未刷新（按 Q 刷新）", muted())));
            }
        }
    }

    // Compact 7d purpose + 7d efficiency summary at bottom of card ④
    let bp = &an.by_purpose;
    let tot = bp.total().max(1);
    let p_line = format!(
        "7d 用途: 写 {}({}%) · 审 {}({}%) · 返 {}({}%) · 读 {}({}%)",
        format_tokens_short(bp.write),
        bp.write * 100 / tot,
        format_tokens_short(bp.review),
        bp.review * 100 / tot,
        format_tokens_short(bp.rework),
        bp.rework * 100 / tot,
        format_tokens_short(bp.read_only),
        bp.read_only * 100 / tot,
    );
    let (merged_7d, median_s, pass_pct) = summarize_7d_trend(&an.daily_7d);
    let t_line = format!(
        "7d 走势: 合入 {merged_7d} 单 · 派发→合入中位 {} · 一轮通过率 {} → 按 4 详",
        median_s
            .map(|s| format_duration(Some(s)))
            .unwrap_or_else(|| "-".to_string()),
        pass_pct
            .map(|p| format!("{p}%"))
            .unwrap_or_else(|| "-".to_string()),
    );
    if lines.len() + 3 <= inner_h {
        lines.push(Line::from(Span::styled("─".repeat(inner_w), muted())));
        lines.push(Line::from(Span::styled(truncate(&p_line, inner_w), muted())));
        lines.push(Line::from(Span::styled(truncate(&t_line, inner_w), muted())));
    } else if lines.len() + 2 <= inner_h {
        lines.push(Line::from(Span::styled(truncate(&p_line, inner_w), muted())));
        lines.push(Line::from(Span::styled(truncate(&t_line, inner_w), muted())));
    }

    let visible: Vec<Line> = lines.into_iter().take(inner_h).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn find_entry_depletion_note(acct: &AgyAccountItem, entry: &QuotaPoolEntry) -> String {
    let Some(est) = &acct.quota_estimate else {
        return String::new();
    };
    let mut matched: Option<&QuotaWindowEstimate> = None;
    for pe in &est.pools {
        if pe.pool == entry.pool {
            matched = if entry.window == "fiveHour" {
                pe.five_hour.as_ref()
            } else {
                pe.weekly.as_ref()
            };
        }
    }
    if matched.is_none() {
        let candidate = if entry.window == "fiveHour" {
            est.five_hour.as_ref()
        } else {
            est.weekly.as_ref()
        };
        if let Some(w) = candidate {
            if w.pool
                .as_deref()
                .is_none_or(|p| p.is_empty() || p == entry.pool)
            {
                matched = Some(w);
            }
        }
    }
    let Some(w) = matched else {
        return String::new();
    };
    match evaluate_window_outcome(acct, &entry.window, w) {
        Some(WindowEstimateOutcome::RunsOutIn {
            mins,
            before_reset: true,
        }) => {
            if mins == 0 {
                "  ⚠ 即将用完".to_string()
            } else {
                format!("  ⚠ 约 {} 用完", format_duration(Some(mins * 60)))
            }
        }
        Some(WindowEstimateOutcome::RunsOutIn {
            before_reset: false,
            ..
        }) => "  重置前用不完".to_string(),
        _ => String::new(),
    }
}

pub(crate) fn summarize_7d_trend(days: &[DailyTrendPoint]) -> (u64, Option<u64>, Option<u64>) {
    let merged: u64 = days.iter().map(|d| d.merged_count).sum();
    let total_chains: u64 = days.iter().map(|d| d.total_chains).sum();
    let first_pass: u64 = days.iter().map(|d| d.first_round_pass).sum();
    let pass_pct = (first_pass * 100 + total_chains / 2).checked_div(total_chains);
    let mut meds: Vec<u64> = days.iter().filter_map(|d| d.median_merge_secs).collect();
    meds.sort_unstable();
    let median = if meds.is_empty() {
        None
    } else {
        Some(meds[meds.len() / 2])
    };
    (merged, median, pass_pct)
}

fn render_recent_deliveries_card(
    frame: &mut Frame,
    app: &mut App,
    deliveries: &[RecentDeliveryItem],
    area: Rect,
) {
    let focused =
        app.overview_focus == OverviewFocus::Deliveries && matches!(app.overlay, Overlay::None);
    let pulse = app.snapshot.effective_pulse();
    let title_str = format!(
        " 最近交付 (今日合入 {} · 近7d {}) ",
        pulse.merged_today,
        deliveries.len()
    );
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;

    let mut block = rounded_block(
        Line::from(Span::styled(
            truncate(&title_str, inner_w.max(8)),
            if focused { bold_primary() } else { muted() },
        )),
        focused,
    );

    if deliveries.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width > 0 && inner.height > 0 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(" 近 7 天暂无已合入交付", muted()))),
                inner,
            );
        }
        return;
    }

    // 2 lines per delivery (header + one-line conclusion), scrollable over ALL 7-day deliveries.
    let items_per_view = (inner_h / 2).max(1);
    if app.overview_deliveries_sel >= deliveries.len() {
        app.overview_deliveries_sel = deliveries.len().saturating_sub(1);
    }
    let sel = app.overview_deliveries_sel;
    if sel < app.overview_deliveries_scroll {
        app.overview_deliveries_scroll = sel;
    } else if sel >= app.overview_deliveries_scroll + items_per_view {
        app.overview_deliveries_scroll = sel + 1 - items_per_view;
    }
    let max_scroll = deliveries.len().saturating_sub(items_per_view);
    if app.overview_deliveries_scroll > max_scroll {
        app.overview_deliveries_scroll = max_scroll;
    }

    let footer = if deliveries.len() > items_per_view {
        let first = app.overview_deliveries_scroll + 1;
        let last = (app.overview_deliveries_scroll + items_per_view).min(deliveries.len());
        format!(" {first}-{last}/{} · Enter 看结论 ", deliveries.len())
    } else {
        " Enter 看结论 ".to_string()
    };
    block = block.title_bottom(Line::from(Span::styled(footer, muted())));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut lines = Vec::with_capacity(inner_h);
    for (idx, d) in deliveries
        .iter()
        .enumerate()
        .skip(app.overview_deliveries_scroll)
        .take(items_per_view)
    {
        let is_sel = focused && idx == sel;
        let when_raw = format_iso_local_short(&d.applied_at, &app.snapshot.today_ymd)
            .unwrap_or_else(|| "-".to_string());
        // Today -> `HH:MM` (5 cols); earlier day -> `MM-DD` (5 cols), never `11-25…`.
        let when = when_raw.split_whitespace().next().unwrap_or("-").to_string();
        let wid = d
            .work_id
            .as_deref()
            .map(|w| format!(" [{w}]"))
            .unwrap_or_default();
        let chg = if d.changes_files > 0 {
            format!("{}f +{}/-{}", d.changes_files, d.added, d.deleted)
        } else {
            String::new()
        };
        let dur = format_duration(Some(d.elapsed_seconds.max(0) as u64));
        let right = if chg.is_empty() {
            format!("{dur} · {}轮", d.rounds.max(1))
        } else {
            format!("{chg} · {dur} · {}轮", d.rounds.max(1))
        };
        let right_w = display_width(&right);
        let short_repo = short_repo_display(std::path::Path::new(&d.repo), &d.repo);
        let left_prefix = format!(
            "{} {} ",
            pad_right(&when, 5),
            pad_right(&truncate(&short_repo, 10), 10)
        );
        let name_budget = inner_w.saturating_sub(2 + display_width(&left_prefix) + right_w + 2);
        let name_col = pad_right(&truncate(&format!("{}{wid}", d.name), name_budget), name_budget);
        let line1_txt = format!("{left_prefix}{name_col} {right}");
        lines.push(style_overview_row(&line1_txt, STATUS_RUNNING, is_sel, inner_w));

        if lines.len() < inner_h {
            let head_tag = if d.applied_head.is_empty() {
                String::new()
            } else {
                format!(" (合入 {})", &d.applied_head[..d.applied_head.len().min(7)])
            };
            let conc_clean = clean_delivery_one_liner(&d.conclusion);
            let line2 = if conc_clean.is_empty() {
                format!("      └ 已合入{head_tag}")
            } else {
                format!("      └ {conc_clean}{head_tag}")
            };
            lines.push(Line::from(Span::styled(truncate(&line2, inner_w), muted())));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn clean_delivery_one_liner(raw: &str) -> String {
    for ln in raw.lines() {
        let stripped = ln.trim().trim_start_matches('#').trim();
        let cleaned = stripped
            .replace("**", "")
            .replace('`', "");
        let t = cleaned.trim();
        if t.is_empty()
            || matches!(
                t,
                "结论" | "核心结论" | "总结" | "Summary" | "Conclusion" | "摘要"
            )
        {
            continue;
        }
        return t.to_string();
    }
    String::new()
}

fn style_overview_row(text: &str, accent_color: Color, selected: bool, width: usize) -> Line<'static> {
    if selected {
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        let clipped = truncate(text, width.saturating_sub(2));
        let pad = width.saturating_sub(2 + display_width(&clipped));
        Line::from(vec![
            Span::styled(
                "▎",
                Style::default()
                    .fg(accent_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", rev),
            Span::styled(clipped, rev.add_modifier(Modifier::BOLD)),
            Span::styled(" ".repeat(pad), rev),
        ])
    } else {
        let clipped = truncate(&format!(" {text}"), width);
        let st = if accent_color == Color::DarkGray {
            muted()
        } else {
            primary()
        };
        Line::from(Span::styled(clipped, st))
    }
}

/// Narrow-screen (`< 110` cols) single-column scrollable flow for `1 总览`:
/// ① 需要关注 → ② 正在运行与波次 → ④ 额度紧凑行 → ③ 工作表推进链 → ⑤ 最近交付
fn render_overview_narrow(frame: &mut Frame, app: &mut App, area: Rect) {
    let pulse = app.snapshot.effective_pulse();
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;

    let mut lines: Vec<Line<'static>> = Vec::new();

    // ① 需要关注
    lines.push(Line::from(Span::styled(
        truncate(
            &format!(
                "▸ 需要关注 (等你 {} · 待主控 {})",
                pulse.owner_open, pulse.controller_pending
            ),
            inner_w,
        ),
        Style::default()
            .fg(if pulse.owner_open > 0 || pulse.controller_pending > 0 {
                STATUS_PENDING
            } else {
                STATUS_RUNNING
            })
            .add_modifier(Modifier::BOLD),
    )));
    let att = app.overview_attention_entries();
    if att.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate("  ✓ 所有者待拍板 0 · 待主控 0（无阻塞）", inner_w),
            Style::default().fg(STATUS_RUNNING),
        )));
    } else {
        for ent in &att {
            let txt = match ent {
                OverviewAttentionEntry::OwnerWork { work_idx } => {
                    let w = &app.snapshot.work[*work_idx];
                    format!("  ! [{}] {} · {}", w.id, w.display_title(), w.state_badge())
                }
                OverviewAttentionEntry::PendingRun { repo_idx, run_idx } => {
                    let r = &app.snapshot.repos[*repo_idx].runs[*run_idx];
                    format!(
                        "  ◆ {} {} {}",
                        r.state_badge(),
                        short_repo_display(&r.repo_path, &r.repo_label),
                        r.display_name
                    )
                }
                OverviewAttentionEntry::StaleFold { count } => {
                    format!("  ▸ 更早陈旧待处理 {count} 条（按 3 展开）")
                }
            };
            lines.push(Line::from(Span::styled(truncate(&txt, inner_w), primary())));
        }
    }
    lines.push(Line::raw(""));

    // ② 正在运行与波次
    let wave = &app.snapshot.eta.wave;
    let wave_clock = wave
        .wave_p50_clock
        .as_deref()
        .map(|c| format!(" · 本波 {c}"))
        .unwrap_or_default();
    lines.push(Line::from(Span::styled(
        truncate(
            &format!(
                "▸ 正在运行 (在跑 {} · 排队 {}{wave_clock})",
                pulse.running_runs, pulse.queued_runs
            ),
            inner_w,
        ),
        Style::default()
            .fg(STATUS_RUNNING)
            .add_modifier(Modifier::BOLD),
    )));
    let running = app.overview_running_entries();
    if running.is_empty() {
        lines.push(Line::from(Span::styled("  空闲 · 无在跑或排队运行", muted())));
    } else {
        for &(ri, rj) in &running {
            let r = &app.snapshot.repos[ri].runs[rj];
            let eta = r.eta_badge().map(|(b, _)| b).unwrap_or("-");
            let elapsed = format_duration(r.elapsed_seconds.or(r.age_seconds));
            let txt = format!(
                "  ● {} {} {} ({elapsed} {eta})",
                r.state_badge(),
                short_repo_display(&r.repo_path, &r.repo_label),
                r.display_name
            );
            lines.push(Line::from(Span::styled(truncate(&txt, inner_w), primary())));
        }
    }
    lines.push(Line::raw(""));

    // ④ 额度紧凑单行
    let an = &app.snapshot.analytics;
    lines.push(Line::from(Span::styled(
        truncate(
            &format!(
                "▸ 额度与消耗 (近24h {} · 近7d {})",
                format_tokens_short(an.hourly_24h_total.iter().sum()),
                format_tokens_short(an.by_purpose.total())
            ),
            inner_w,
        ),
        bold_primary(),
    )));
    if app.snapshot.agy_accounts.accounts.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate("  未配置 agy 账号 · delegate agy account add", inner_w),
            muted(),
        )));
    } else {
        for acct in &app.snapshot.agy_accounts.accounts {
            let mark = if acct.active { "★" } else { "·" };
            let name = acct.display_name(app.is_account_revealed(&acct.id));
            let mut summary = format!("  {mark} {name}");
            if let Some(q) = &acct.quota {
                if q.ok && !q.identity_mismatch {
                    if let Some(e) = q.entries.first() {
                        let ratio = (e.remaining_pct_x100 as f64 / 10000.0).clamp(0.0, 1.0);
                        let (fill, empty) = build_horiz_bar(ratio, 8);
                        let pct = (e.remaining_pct_x100 + 50) / 100;
                        let warn = find_entry_depletion_note(acct, e);
                        summary.push_str(&format!("  {fill}{empty} {pct}%{warn}"));
                    }
                } else if q.identity_mismatch {
                    summary.push_str("  ⚠ 身份不符，未更新");
                }
            }
            lines.push(Line::from(Span::styled(truncate(&summary, inner_w), primary())));
        }
    }
    lines.push(Line::raw(""));

    // ③ 工作表推进链
    lines.push(Line::from(Span::styled(
        truncate(
            &format!(
                "▸ 工作表推进链 (推进 {} · 定时/暂缓 {})",
                pulse.work_active_pool,
                pulse.work_timed + pulse.work_deferred
            ),
            inner_w,
        ),
        Style::default()
            .fg(STATUS_QUEUED)
            .add_modifier(Modifier::BOLD),
    )));
    let wrows = app.overview_work_chain_rows();
    if wrows.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate("  当前无推进中工作链（按 2 查看工作表）", inner_w),
            muted(),
        )));
    } else {
        for r in &wrows {
            if let WorkRow::Item { work_idx, depth, .. } = r {
                let w = &app.snapshot.work[*work_idx];
                let indent = "  ".repeat((*depth).min(3) + 1);
                let txt = format!(
                    "{indent}{} [{}] {} · {}",
                    w.state_badge(),
                    w.id,
                    w.display_title(),
                    app.snapshot.work_live_status(w)
                );
                lines.push(Line::from(Span::styled(truncate(&txt, inner_w), primary())));
            }
        }
    }
    lines.push(Line::raw(""));

    // ⑤ 最近交付
    let deliveries = app.overview_delivery_entries();
    lines.push(Line::from(Span::styled(
        truncate(
            &format!(
                "▸ 最近交付 (今日合入 {} · 近7d {})",
                pulse.merged_today,
                deliveries.len()
            ),
            inner_w,
        ),
        bold_primary(),
    )));
    if deliveries.is_empty() {
        lines.push(Line::from(Span::styled("  近 7 天暂无已合入交付", muted())));
    } else {
        for d in &deliveries {
            let when = format_iso_local_short(&d.applied_at, &app.snapshot.today_ymd)
                .unwrap_or_else(|| "-".to_string());
            let txt = format!(
                "  {when} [{}] {} ({}f · {}轮)",
                d.repo,
                d.name,
                d.changes_files,
                d.rounds.max(1)
            );
            lines.push(Line::from(Span::styled(truncate(&txt, inner_w), primary())));
        }
    }

    let max_scroll = lines.len().saturating_sub(inner_h);
    if app.overview_scroll > max_scroll {
        app.overview_scroll = max_scroll;
    }
    let footer = if lines.len() > inner_h && inner_h > 0 {
        format!(
            " {}-{}/{} ",
            app.overview_scroll + 1,
            (app.overview_scroll + inner_h).min(lines.len()),
            lines.len()
        )
    } else {
        String::new()
    };
    let mut block = rounded_block(
        Line::from(Span::styled(" 总览 ", bold_primary())),
        matches!(app.overlay, Overlay::None),
    );
    if !footer.is_empty() {
        block = block.title_bottom(Line::from(Span::styled(footer, muted())));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let visible: Vec<Line> = lines
        .into_iter()
        .skip(app.overview_scroll)
        .take(inner_h)
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);
}
