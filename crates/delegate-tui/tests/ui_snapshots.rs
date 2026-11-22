use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use delegate_tui::app::{App, Overlay, WriteAction};
use delegate_tui::data::{parse_lane_json, parse_status_jsonl};
use delegate_tui::model::*;
use delegate_tui::ui;
use delegate_tui::width::display_width;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::path::{Path, PathBuf};

const REPO_A_JSONL: &str = r#"
{"run":"20261122-100000-api-r1","name":"接口鉴权","state":"rejected","mode":"write","modelName":"sol","elapsedSeconds":300,"ageSeconds":3600}
{"run":"20261122-103000-api-r2","name":"reply to 接口鉴权","parent":"20261122-100000-api-r1","chainName":"接口鉴权","chainRound":2,"state":"delivered","finishedAt":"2026-11-22T10:40:00Z","mode":"write","tier":"write","modelName":"neon","elapsedSeconds":640,"ageSeconds":820,"worktree":"/tmp/wt/api-r2","files":["crates/api/src/auth.rs","crates/api/src/routes.rs","tests/api_auth.rs"],"changes":{"files":3,"added":182,"deleted":24},"pendingChanges":{"files":3,"added":182,"deleted":24},"shape":{"dirs":[{"path":"crates/api","added":132,"deleted":20},{"path":"tests","added":50,"deleted":4}]},"accept":{"command":"cargo test -p api","ok":true,"exitCode":0},"promptProtectedPaths":["migrations/"],"result":"/tmp/r2/result.md","resultChars":420,"next":"review delegate diff 20261122-103000-api-r2 --total, then merge with delegate apply 20261122-103000-api-r2"}
{"run":"20261122-104500-worker","name":"后台任务调度器","state":"running","mode":"write","tier":"write","modelName":"sol","elapsedSeconds":815,"ageSeconds":815,"idleSeconds":18,"turns":12,"last":"bash: cargo test --workspace","files":["crates/worker/src/sched.rs"]}
{"run":"20261122-105000-wait-q","name":"等待容量队列","state":"waiting","mode":"write","tier":"write","modelName":"neon","ageSeconds":90,"queue":{"reason":"model capacity","ahead":1}}
{"run":"20261122-090000-done-ok","name":"旧迁移脚本","state":"delivered","applied":true,"appliedBy":"delegate","mode":"write","modelName":"sol","elapsedSeconds":210,"ageSeconds":7410,"changes":{"files":1,"added":12,"deleted":3},"accept":{"command":"scripts/verify.sh","ok":true,"exitCode":0}}
"#;

const REPO_B_JSONL: &str = r#"
{"run":"20261122-104800-ui-fix","name":"移动端样式修正","state":"rejected","mode":"write","tier":"write","modelName":"neon","elapsedSeconds":320,"ageSeconds":500,"protectViolation":["packages/theme/tokens.css"],"changes":{"files":2,"added":44,"deleted":11},"accept":{"command":"pnpm test","ok":false,"exitCode":1},"next":"protected paths were changed; restore them then delegate reply 20261122-104800-ui-fix"}
"#;

fn make_sample_app() -> App {
    let repo_a = parse_status_jsonl(Path::new("/home/dev/demo-api"), "~/demo-api", REPO_A_JSONL);
    let repo_b = parse_status_jsonl(Path::new("/home/dev/web-app"), "~/web-app", REPO_B_JSONL);
    let lane = parse_lane_json(
        r#"{"maxHeavy":2,"local":[{"label":"demo-api: cargo test","since":"2026-11-22T10:40:00Z","running":true}],"remoteMaxHeavy":4,"remote":[]}"#,
    )
    .unwrap();
    let snap = AppSnapshot {
        repos: vec![repo_a, repo_b],
        lane,
        host_load: Some(HostLoad {
            load1_tenths: 18,
            load5_tenths: 14,
            load15_tenths: 11,
            mem_used_tenths_gib: Some(225),
            mem_total_tenths_gib: Some(640),
        }),
        refreshed_at: "10:52:00".to_string(),
        warnings: vec![],
    };
    let mut app = App::new(
        snap,
        PathBuf::from("delegate"),
        PathBuf::from("/home/dev/demo-api"),
        5,
    );
    if let Some(run) = app.selected_run().cloned() {
        app.result_cache.insert(
            run.cache_key(),
            Ok("## 核心结论\n\n**已完成接口鉴权改造**，详见 [`crates/api/src/auth.rs`](file:///tmp/auth.rs) 与 `cargo test -p api`。\n\n- 覆盖空令牌、过期令牌与合法请求。".to_string()),
        );
    }
    app
}

fn render_to_string(app: &mut App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui::render(f, app)).unwrap();
    let buf = terminal.backend().buffer();
    let mut lines = Vec::with_capacity(height as usize);
    for y in 0..height {
        let mut row = String::new();
        let mut x = 0u16;
        while x < width {
            let sym = buf[(x, y)].symbol();
            row.push_str(sym);
            let step = display_width(sym).max(1) as u16;
            x = x.saturating_add(step);
        }
        lines.push(row.trim_end().to_string());
    }
    lines.join("\n")
}

#[test]
fn snapshot_wide_screen_renders_rounded_borders_bars_relative_time_and_lane_meter() {
    let mut app = make_sample_app();
    let screen = render_to_string(&mut app, 148, 34);

    // Rounded borders & top bar counts
    assert!(screen.contains('╭') && screen.contains('╰'), "{screen}");
    assert!(screen.contains("delegate-tui"), "{screen}");
    assert!(screen.contains("待处理 2"), "{screen}");
    assert!(screen.contains("运行 1"), "{screen}");
    assert!(screen.contains("排队 1"), "{screen}");
    assert!(screen.contains("已合 1"), "{screen}");

    // Multi-repo headers, status groups, and folded merged section
    assert!(screen.contains("~/demo-api"), "{screen}");
    assert!(screen.contains("~/web-app"), "{screen}");
    assert!(screen.contains("待处理 (1)"), "{screen}");
    assert!(screen.contains("运行中 (1)"), "{screen}");
    assert!(screen.contains("排队 (1)"), "{screen}");
    assert!(screen.contains("▸ 已合未清 1 条"), "{screen}");

    // Collapsed chain, relative time ("3 分钟前"), mini diff bar, badges, selected stripe
    assert!(screen.contains('▎'), "{screen}");
    assert!(screen.contains("[2轮]"), "{screen}");
    assert!(screen.contains("3 分钟前"), "{screen}");
    assert!(screen.contains('▪'), "{screen}");
    assert!(screen.contains("保?"), "{screen}");
    assert!(screen.contains("验✓"), "{screen}");
    assert!(screen.contains("保!"), "{screen}");
    assert!(screen.contains("验✗"), "{screen}");

    // Right detail pane renders Markdown cleanly (no raw **, `, ##, or file:///)
    assert!(screen.contains("结论摘要"), "{screen}");
    assert!(screen.contains("核心结论"), "{screen}");
    assert!(screen.contains("已完成接口鉴权改造"), "{screen}");
    assert!(!screen.contains("**"), "raw markdown bold leaked: {screen}");
    assert!(!screen.contains('`'), "raw markdown backtick leaked: {screen}");
    assert!(!screen.contains("##"), "raw markdown heading leaked: {screen}");
    assert!(!screen.contains("file:///"), "raw markdown link URL leaked: {screen}");
    assert!(screen.contains("最近命令"), "{screen}");
    assert!(screen.contains("改动分布"), "{screen}");
    assert!(screen.contains("crates/api"), "{screen}");
    assert!(screen.contains("下一步"), "{screen}");

    // Bottom bar mini lane meter + host load + time
    assert!(screen.contains("Lane █░ 1/2"), "{screen}");
    assert!(screen.contains("负载 1.8 1.4"), "{screen}");
    assert!(screen.contains("10:52:00"), "{screen}");
}

#[test]
fn snapshot_narrow_80_columns_uses_readable_stacked_layout() {
    let mut app = make_sample_app();
    let screen = render_to_string(&mut app, 80, 30);

    assert!(screen.contains("任务 (5)"), "{screen}");
    assert!(screen.contains("接口鉴权"), "{screen}");
    assert!(screen.contains("~/demo-api"), "{screen}");
    assert!(screen.contains("[2轮]"), "{screen}");
    assert!(screen.contains("3 分钟前"), "{screen}");
    assert!(screen.contains("结论摘要"), "{screen}");
    assert!(screen.contains("Lane █░ 1/2"), "{screen}");
    assert!(screen.contains("jk选 Tab栏"), "{screen}");
}

#[test]
fn snapshot_overlays_result_diff_and_command_modal() {
    let mut app = make_sample_app();

    // Command modal ('a')
    app.open_command_modal(WriteAction::Apply);
    let modal_screen = render_to_string(&mut app, 110, 28);
    assert!(modal_screen.contains("合入 (apply)"), "{modal_screen}");
    assert!(modal_screen.contains("apply 20261122-103000-api-r2"), "{modal_screen}");
    assert!(modal_screen.contains("y/Enter 复制"), "{modal_screen}");

    // Diff stat overlay
    app.overlay = Overlay::DiffStat {
        run_id: "20261122-103000-api-r2".into(),
        title: "接口鉴权".into(),
        content: " crates/api/src/auth.rs | 120 ++++++++++---\n tests/api_auth.rs      |  54 +++++\n 2 files changed, 160 insertions(+), 14 deletions(-)".into(),
        scroll: 0,
    };
    let diff_screen = render_to_string(&mut app, 110, 28);
    assert!(diff_screen.contains("Diff 统计 · 接口鉴权"), "{diff_screen}");
    assert!(diff_screen.contains("crates/api/src/auth.rs"), "{diff_screen}");

    // Full result overlay
    app.overlay = Overlay::FullResult {
        run_id: "20261122-103000-api-r2".into(),
        title: "接口鉴权".into(),
        content: "完整结论标题\n\n第一段详细说明。\n第二段验证结果。".into(),
        scroll: 0,
    };
    let result_screen = render_to_string(&mut app, 110, 28);
    assert!(result_screen.contains("完整结论 · 接口鉴权"), "{result_screen}");
    assert!(result_screen.contains("P 分页器"), "{result_screen}");

    // Filter mode
    app.overlay = Overlay::None;
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    for c in "样式".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    let filter_screen = render_to_string(&mut app, 110, 28);
    assert!(filter_screen.contains("/过滤"), "{filter_screen}");
    assert!(filter_screen.contains("移动端样式修正"), "{filter_screen}");
}

#[test]
fn snapshot_empty_state_is_minimal_and_tiny_terminal_does_not_panic() {
    let snap = AppSnapshot {
        repos: vec![],
        lane: LaneStatus::default(),
        host_load: None,
        refreshed_at: "11:00:00".to_string(),
        warnings: vec![],
    };
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("."), 5);
    let empty = render_to_string(&mut app, 80, 24);
    assert!(empty.contains("暂无委派任务"), "{empty}");
    // No decorative filler in empty detail pane
    assert!(!empty.contains("选择一个任务以查看详情"), "{empty}");

    let tiny = render_to_string(&mut app, 16, 6);
    assert!(tiny.contains("窗口过小"), "{tiny}");
}
